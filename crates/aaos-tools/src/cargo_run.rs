use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, Result, ToolDefinition};

/// Subcommands the tool will execute. Kept narrow so the tool can never
/// run `cargo install`, `cargo publish`, or anything else that mutates
/// state outside the workspace.
const ALLOWED_SUBCOMMANDS: &[&str] = &["check", "test", "clippy", "fmt"];

/// Wall-clock limit for a single `cargo <subcmd>` invocation. Four minutes
/// covers a clean `cargo check` on a mid-size workspace on a 4 vCPU box
/// with margin; beyond that the agent is almost certainly looping and we
/// want to surface a timeout.
const DEFAULT_TIMEOUT_SECS: u64 = 240;

/// Upper bound on how many output bytes the tool returns in the audit
/// preview path. Full output still lands on disk if the caller supplies
/// `stdout_path`; this cap just keeps the audit stream bounded.
const MAX_INLINE_OUTPUT: usize = 8_192;

pub struct CargoRunTool;

#[async_trait]
impl Tool for CargoRunTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "cargo_run".to_string(),
            description: format!(
                "Run `cargo <subcommand>` in a workspace. Subcommand must be one of: {}. \
                 Workspace must match a granted CargoRun capability.",
                ALLOWED_SUBCOMMANDS.join(", ")
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workspace": {
                        "type": "string",
                        "description": "Absolute path to the crate/workspace root (dir containing Cargo.toml)"
                    },
                    "subcommand": {
                        "type": "string",
                        "description": "Cargo subcommand to run",
                        "enum": ALLOWED_SUBCOMMANDS
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Extra args passed to cargo after the subcommand (optional)"
                    }
                },
                "required": ["workspace", "subcommand"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let workspace = input
            .get("workspace")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'workspace' parameter".into()))?;

        let subcommand = input
            .get("subcommand")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'subcommand' parameter".into()))?;

        if !ALLOWED_SUBCOMMANDS.contains(&subcommand) {
            return Err(CoreError::InvalidManifest(format!(
                "cargo subcommand '{subcommand}' not allowed; allowlist: {}",
                ALLOWED_SUBCOMMANDS.join(", ")
            )));
        }

        let extra_args: Vec<String> = input
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let requested = Capability::CargoRun {
            workspace: workspace.to_string(),
        };
        let allowed = ctx.tokens.iter().any(|h| {
            ctx.capability_registry
                .permits(*h, ctx.agent_id, &requested)
        });
        if !allowed {
            return Err(CoreError::CapabilityDenied {
                agent_id: ctx.agent_id,
                capability: requested,
                reason: format!("cargo_run not permitted for workspace: {workspace}"),
            });
        }

        let ws_path = PathBuf::from(workspace);
        let cargo_toml = ws_path.join("Cargo.toml");
        if !cargo_toml.is_file() {
            return Err(CoreError::Ipc(format!(
                "workspace '{workspace}' has no Cargo.toml"
            )));
        }

        run_cargo(&ws_path, subcommand, &extra_args).await
    }
}

async fn run_cargo(ws: &Path, subcommand: &str, extra_args: &[String]) -> Result<Value> {
    let started = std::time::Instant::now();

    let mut cmd = tokio::process::Command::new("cargo");
    cmd.arg(subcommand);
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.current_dir(ws)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|e| CoreError::Ipc(format!("failed to spawn cargo: {e}")))?;

    let timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return Err(CoreError::Ipc(format!("cargo wait failed: {e}")));
        }
        Err(_elapsed) => {
            return Err(CoreError::Timeout(timeout));
        }
    };

    let duration_ms = started.elapsed().as_millis() as u64;
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    Ok(json!({
        "subcommand": subcommand,
        "exit_code": exit_code,
        "success": output.status.success(),
        "duration_ms": duration_ms,
        "stdout": cap_bytes(&stdout, MAX_INLINE_OUTPUT),
        "stderr": cap_bytes(&stderr, MAX_INLINE_OUTPUT),
        "stdout_truncated": stdout.len() > MAX_INLINE_OUTPUT,
        "stderr_truncated": stderr.len() > MAX_INLINE_OUTPUT,
    }))
}

fn cap_bytes(s: &str, max: usize) -> String {
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

    fn ctx_with_cargo_run(workspace_glob: &str) -> InvocationContext {
        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::CargoRun {
                workspace: workspace_glob.to_string(),
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

    /// Create a tiny hello-world crate in a tempdir.
    fn scaffold_crate(dir: &TempDir) -> std::path::PathBuf {
        let ws = dir.path().to_path_buf();
        std::fs::write(
            ws.join("Cargo.toml"),
            r#"[package]
name = "probe"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(
            ws.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        ws
    }

    #[tokio::test]
    async fn rejects_subcommand_not_in_allowlist() {
        let dir = TempDir::new().unwrap();
        let ws = scaffold_crate(&dir);
        let ctx = ctx_with_cargo_run(ws.to_str().unwrap());
        let err = CargoRunTool
            .invoke(
                json!({"workspace": ws.to_str().unwrap(), "subcommand": "install"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "got: {err}");
    }

    #[tokio::test]
    async fn rejects_workspace_without_capability() {
        let dir = TempDir::new().unwrap();
        let ws = scaffold_crate(&dir);
        // Grant a *different* workspace glob.
        let ctx = ctx_with_cargo_run("/nowhere/*");
        let err = CargoRunTool
            .invoke(
                json!({"workspace": ws.to_str().unwrap(), "subcommand": "check"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not permitted"), "got: {err}");
    }

    #[tokio::test]
    async fn rejects_missing_cargo_toml() {
        let dir = TempDir::new().unwrap();
        // No Cargo.toml scaffolded.
        let ws = dir.path();
        let ctx = ctx_with_cargo_run(ws.to_str().unwrap());
        let err = CargoRunTool
            .invoke(
                json!({"workspace": ws.to_str().unwrap(), "subcommand": "check"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Cargo.toml"), "got: {err}");
    }

    // Runs real cargo — gated behind a feature so CI without network/cargo
    // doesn't flake. Local verification runs it with: `cargo test -p aaos-tools
    // --features online-tests cargo_run_check_on_probe_crate -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn cargo_run_check_on_probe_crate() {
        let dir = TempDir::new().unwrap();
        let ws = scaffold_crate(&dir);
        let ctx = ctx_with_cargo_run(ws.to_str().unwrap());
        let result = CargoRunTool
            .invoke(
                json!({"workspace": ws.to_str().unwrap(), "subcommand": "check"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result["success"], true, "cargo check failed: {:?}", result);
        assert_eq!(result["subcommand"], "check");
    }
}
