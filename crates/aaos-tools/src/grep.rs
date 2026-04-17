use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, Result, ToolDefinition};

const MAX_INLINE_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_MATCHES: usize = 200;

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_string(),
            description: "Search for a regex pattern in files under a directory. \
                 Uses ripgrep. Capped at 200 matches / 16 KB inline output. \
                 Requires FileRead capability for the target path."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern (ripgrep syntax)" },
                    "path": { "type": "string", "description": "Root directory or file to search" },
                    "glob": { "type": "string", "description": "Optional file glob filter (e.g. \"*.rs\")" },
                    "case_insensitive": { "type": "boolean", "description": "Case-insensitive match (default: false)" }
                },
                "required": ["pattern", "path"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'pattern' parameter".into()))?;
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'path' parameter".into()))?;
        let glob = input.get("glob").and_then(|v| v.as_str());
        let case_insensitive = input
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Capability check: need FileRead on the search root.
        let requested = Capability::FileRead {
            path_glob: path_str.to_string(),
        };
        let allowed = ctx.tokens.iter().any(|h| {
            ctx.capability_registry
                .permits(*h, ctx.agent_id, &requested)
        });
        if !allowed {
            return Err(CoreError::CapabilityDenied {
                agent_id: ctx.agent_id,
                capability: requested,
                reason: format!("grep not permitted for path: {path_str}"),
            });
        }

        let path = PathBuf::from(path_str);
        if !path.exists() {
            return Err(CoreError::Ipc(format!("path does not exist: {path_str}")));
        }

        run_rg(pattern, &path, glob, case_insensitive).await
    }
}

async fn run_rg(
    pattern: &str,
    path: &std::path::Path,
    glob: Option<&str>,
    case_insensitive: bool,
) -> Result<Value> {
    // Use ripgrep's --json output so the parser doesn't depend on ':' as
    // a delimiter. The default "file:line:text" format mis-splits any
    // filename that itself contains a colon (valid on Unix). --json emits
    // one NDJSON event per line with structured fields.
    //
    // No --max-count flag: that flag is per-FILE in ripgrep, not global,
    // so passing MAX_MATCHES there cooks in a misleading bound. We cap
    // total matches via .take(MAX_MATCHES) on the iterator below, which
    // is the real contract.
    let mut cmd = tokio::process::Command::new("rg");
    cmd.arg("--json");
    if case_insensitive {
        cmd.arg("-i");
    }
    if let Some(g) = glob {
        cmd.arg("--glob").arg(g);
    }
    cmd.arg("--").arg(pattern).arg(path);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|e| CoreError::Ipc(format!("failed to spawn rg: {e}")))?;

    let timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(CoreError::Ipc(format!("rg wait failed: {e}"))),
        Err(_) => return Err(CoreError::Timeout(timeout)),
    };

    // ripgrep exit codes: 0 = matches, 1 = no matches, 2 = error.
    let exit_code = output.status.code().unwrap_or(-1);
    if exit_code >= 2 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CoreError::Ipc(format!(
            "rg error (exit {exit_code}): {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Each line is an NDJSON event. We only care about "type":"match".
    // Shape (per ripgrep docs):
    //   { "type": "match",
    //     "data": {
    //       "path": { "text": "<file>" },
    //       "lines": { "text": "<matching line>\n" },
    //       "line_number": 42,
    //       ...
    //     } }
    // We read `path.text`, `line_number`, and `lines.text` (trimming a
    // trailing newline). Any line we can't parse or that isn't a match
    // event is silently skipped — ripgrep emits `begin`/`end`/`summary`
    // events too.
    let mut matches = Vec::new();
    let mut total_match_events = 0usize;
    for line in stdout.lines() {
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if ev.get("type").and_then(|v| v.as_str()) != Some("match") {
            continue;
        }
        total_match_events += 1;
        if matches.len() >= MAX_MATCHES {
            continue;
        }
        let data = match ev.get("data") {
            Some(d) => d,
            None => continue,
        };
        let file = data
            .get("path")
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let line_no = data
            .get("line_number")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let text = data
            .get("lines")
            .and_then(|l| l.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        // ripgrep's `lines.text` includes the trailing newline; strip it.
        let text = text.strip_suffix('\n').unwrap_or(text);
        matches.push(json!({
            "file": file,
            "line": line_no,
            "text": cap(text, 512),
        }));
    }

    let truncated = total_match_events > MAX_MATCHES || stdout.len() > MAX_INLINE_BYTES;

    Ok(json!({
        "pattern": pattern,
        "matches": matches,
        "total_matches": matches.len(),
        "truncated": truncated,
    }))
}

fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, CapabilityRegistry, CapabilityToken, Constraints};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx_with_read(glob: &str) -> InvocationContext {
        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::FileRead {
                path_glob: glob.to_string(),
            },
            Constraints::default(),
        );
        let registry = Arc::new(CapabilityRegistry::new());
        let handle = registry.insert(agent_id, token);
        InvocationContext {
            agent_id,
            tokens: vec![handle],
            capability_registry: registry,
        }
    }

    // Gated behind --ignored because it shells out to a real `rg`
    // process. CI / dev machines without ripgrep installed would
    // otherwise see spurious test failures. Run locally with:
    //   cargo test -p aaos-tools grep -- --ignored
    #[tokio::test]
    #[ignore]
    async fn grep_finds_matches() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo\nbar\nbaz\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "qux\nfoo\n").unwrap();
        let path = dir.path().to_str().unwrap();
        let ctx = ctx_with_read(path);

        let result = GrepTool
            .invoke(json!({"pattern": "foo", "path": path}), &ctx)
            .await
            .unwrap();
        let matches = result["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[tokio::test]
    #[ignore]
    async fn grep_capability_denied() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_str().unwrap();
        let ctx = ctx_with_read("/nowhere/*");
        let err = GrepTool
            .invoke(json!({"pattern": "x", "path": path}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not permitted"));
    }

    #[tokio::test]
    #[ignore]
    async fn grep_no_matches_returns_empty() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "nothing here\n").unwrap();
        let path = dir.path().to_str().unwrap();
        let ctx = ctx_with_read(path);

        let result = GrepTool
            .invoke(json!({"pattern": "xyz", "path": path}), &ctx)
            .await
            .unwrap();
        assert_eq!(result["matches"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    #[ignore]
    async fn grep_parses_filename_with_colon() {
        // Regression: the old parser used split_once(':') on ripgrep's
        // default "file:line:text" output and mis-parsed any filename
        // containing a colon. With --json output the path is a
        // structured field so embedded colons round-trip correctly.
        let dir = TempDir::new().unwrap();
        let weird = dir.path().join("weird:name.txt");
        std::fs::write(&weird, "needle\n").unwrap();
        let path = dir.path().to_str().unwrap();
        let ctx = ctx_with_read(path);

        let result = GrepTool
            .invoke(json!({"pattern": "needle", "path": path}), &ctx)
            .await
            .unwrap();
        let matches = result["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1, "matches: {:?}", matches);
        let file = matches[0]["file"].as_str().unwrap();
        assert!(file.ends_with("weird:name.txt"), "file: {file}");
    }
}
