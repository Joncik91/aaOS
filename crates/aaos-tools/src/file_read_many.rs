use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, Result, ToolDefinition};

const MAX_READ_BYTES_PER_FILE: u64 = 1_048_576; // 1 MB per file (matches FileReadTool)
const MAX_PATHS: usize = 16; // cap paths per call so one tool_use can't flood the context

/// Batch version of `file_read`. Reads N files in parallel and returns an
/// array of results in request order. Each file's capability is checked
/// individually against the caller's tokens.
///
/// Designed to reduce LLM round-trips on scan-heavy tasks where the agent
/// knows which files it needs — Run 7b saw 20+ sequential `file_read` calls
/// where a single `file_read_many` would have cut the round-trips 5x.
///
/// Per-file failures (capability denied, not found, too large) are surfaced
/// in the result array as `{path, error, ...}` rather than aborting the
/// whole batch — the agent can then decide whether to retry individual paths.
pub struct FileReadManyTool;

#[async_trait]
impl Tool for FileReadManyTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_read_many".to_string(),
            description:
                "Read multiple files in parallel. Each path is capability-checked individually; \
                 per-file failures (not found, capability denied, too large, not a file) are \
                 returned as structured error entries alongside successes, so one bad path \
                 doesn't abort the batch. Use this when you know the 2-16 files you need upfront. \
                 Note: an internal task panic — a programming bug, not an expected per-file \
                 failure — aborts the entire batch with an error; do not use with untrusted \
                 file sources."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Absolute paths to read. Max 16 entries per call."
                    }
                },
                "required": ["paths"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let paths = input
            .get("paths")
            .and_then(|v| v.as_array())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'paths' array".into()))?;

        if paths.is_empty() {
            return Err(CoreError::InvalidManifest(
                "'paths' array must not be empty".into(),
            ));
        }
        if paths.len() > MAX_PATHS {
            return Err(CoreError::InvalidManifest(format!(
                "too many paths: {} (max {})",
                paths.len(),
                MAX_PATHS
            )));
        }

        let path_strings: Vec<String> = paths
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if path_strings.len() != paths.len() {
            return Err(CoreError::InvalidManifest(
                "all 'paths' entries must be strings".into(),
            ));
        }

        // Fan out per-path reads. Spawn on a JoinSet so they run concurrently
        // on the tokio runtime; capability check lives inside each task so one
        // denial doesn't short-circuit the batch.
        let tokens = ctx.tokens.clone();
        let agent_id = ctx.agent_id;
        let registry = ctx.capability_registry.clone();
        let mut set = tokio::task::JoinSet::new();
        for (idx, path_str) in path_strings.iter().cloned().enumerate() {
            let tokens = tokens.clone();
            let registry = registry.clone();
            set.spawn(async move { (idx, read_one(path_str, &tokens, &registry, agent_id).await) });
        }

        // Collect results, then sort back to request order — JoinSet completes
        // in whatever order the tasks finish, but the agent expects responses
        // aligned with the input paths array.
        let mut indexed: Vec<(usize, Value)> = Vec::with_capacity(path_strings.len());
        while let Some(join_result) = set.join_next().await {
            match join_result {
                Ok((idx, v)) => indexed.push((idx, v)),
                Err(e) => {
                    // A JoinError here is a task panic or cancellation — both
                    // are programming bugs (we never cancel tasks). Per Run 9
                    // peer review: fail loud rather than converting panics
                    // into per-file errors that would mask the root cause.
                    return Err(CoreError::Ipc(format!("file_read_many task panicked: {e}")));
                }
            }
        }
        indexed.sort_by_key(|(idx, _)| *idx);
        let results: Vec<Value> = indexed.into_iter().map(|(_, v)| v).collect();

        Ok(json!({
            "files": results,
            "count": path_strings.len(),
        }))
    }
}

async fn read_one(
    path_str: String,
    tokens: &[aaos_core::CapabilityHandle],
    registry: &aaos_core::CapabilityRegistry,
    agent_id: aaos_core::AgentId,
) -> Value {
    // Capability check
    let requested = Capability::FileRead {
        path_glob: path_str.clone(),
    };
    if !tokens
        .iter()
        .any(|h| registry.permits(*h, agent_id, &requested))
    {
        return json!({
            "path": path_str,
            "error": "capability_denied",
            "reason": format!("file_read not permitted for path: {path_str}"),
        });
    }

    let path = Path::new(&path_str);
    let metadata = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) => {
            return json!({
                "path": path_str,
                "error": "not_found",
                "reason": format!("file not found: {e}"),
            });
        }
    };

    if !metadata.is_file() {
        return json!({
            "path": path_str,
            "error": "not_a_file",
            "reason": format!("{path_str} is not a regular file"),
        });
    }

    if metadata.len() > MAX_READ_BYTES_PER_FILE {
        return json!({
            "path": path_str,
            "error": "too_large",
            "reason": format!(
                "file too large: {} bytes (max {})",
                metadata.len(),
                MAX_READ_BYTES_PER_FILE
            ),
            "size_bytes": metadata.len(),
        });
    }

    match tokio::fs::read_to_string(path).await {
        Ok(content) => json!({
            "path": path_str,
            "content": content,
            "size_bytes": metadata.len(),
        }),
        Err(e) => json!({
            "path": path_str,
            "error": "read_failed",
            "reason": format!("failed to read file: {e}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, CapabilityRegistry, CapabilityToken, Constraints};
    use std::io::Write;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

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

    #[tokio::test]
    async fn reads_multiple_files_in_parallel() {
        let mut tmp1 = NamedTempFile::new().unwrap();
        write!(tmp1, "file one").unwrap();
        let mut tmp2 = NamedTempFile::new().unwrap();
        write!(tmp2, "file two").unwrap();

        let p1 = tmp1.path().to_str().unwrap().to_string();
        let p2 = tmp2.path().to_str().unwrap().to_string();

        let ctx = ctx_with_read("/tmp/*");
        let result = FileReadManyTool
            .invoke(json!({"paths": [&p1, &p2]}), &ctx)
            .await
            .unwrap();

        let files = result["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["content"], "file one");
        assert_eq!(files[0]["path"], p1);
        assert_eq!(files[1]["content"], "file two");
        assert_eq!(files[1]["path"], p2);
        assert_eq!(result["count"], 2);
    }

    #[tokio::test]
    async fn partial_failure_does_not_abort_batch() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "ok").unwrap();
        let ok_path = tmp.path().to_str().unwrap().to_string();
        let missing_path = "/tmp/definitely-does-not-exist-aaos".to_string();

        let ctx = ctx_with_read("/tmp/*");
        let result = FileReadManyTool
            .invoke(json!({"paths": [&ok_path, &missing_path]}), &ctx)
            .await
            .unwrap();

        let files = result["files"].as_array().unwrap();
        assert_eq!(files[0]["content"], "ok");
        assert_eq!(files[1]["error"], "not_found");
    }

    #[tokio::test]
    async fn per_file_capability_check_denies_out_of_scope() {
        let mut tmp_allowed = NamedTempFile::new().unwrap();
        write!(tmp_allowed, "allowed").unwrap();
        let mut tmp_forbidden = NamedTempFile::new().unwrap();
        write!(tmp_forbidden, "forbidden").unwrap();

        // Token only grants the first file's exact path (not a wildcard)
        let allowed_path = tmp_allowed.path().to_str().unwrap().to_string();
        let forbidden_path = tmp_forbidden.path().to_str().unwrap().to_string();
        let ctx = ctx_with_read(&allowed_path);

        let result = FileReadManyTool
            .invoke(json!({"paths": [&allowed_path, &forbidden_path]}), &ctx)
            .await
            .unwrap();

        let files = result["files"].as_array().unwrap();
        assert_eq!(files[0]["content"], "allowed");
        assert_eq!(files[1]["error"], "capability_denied");
    }

    #[tokio::test]
    async fn rejects_empty_paths() {
        let ctx = ctx_with_read("/tmp/*");
        let result = FileReadManyTool
            .invoke(json!({"paths": []}), &ctx)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("must not be empty"));
    }

    #[tokio::test]
    async fn rejects_over_max_paths() {
        let ctx = ctx_with_read("/tmp/*");
        let paths: Vec<String> = (0..17).map(|i| format!("/tmp/file_{i}")).collect();
        let result = FileReadManyTool
            .invoke(json!({"paths": paths}), &ctx)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too many paths"));
    }

    #[tokio::test]
    async fn rejects_missing_paths_param() {
        let ctx = ctx_with_read("/tmp/*");
        let result = FileReadManyTool.invoke(json!({}), &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_non_string_paths_entries() {
        let ctx = ctx_with_read("/tmp/*");
        let result = FileReadManyTool
            .invoke(json!({"paths": ["/tmp/ok", 42]}), &ctx)
            .await;
        assert!(result.is_err());
    }
}
