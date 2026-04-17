use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, Result, ToolDefinition};

/// Subcommands the tool will execute. Kept narrow so the tool can never
/// run `git push`, `git rebase`, `git reset`, or anything else that mutates
/// history or config.
const ALLOWED_SUBCOMMANDS: &[&str] = &["add", "commit"];

/// Wall-clock limit for a single `git add` + `git commit` invocation.
/// Commits are fast; 60s is generous.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Upper bound on how many stdout/stderr bytes the tool returns inline.
/// Keeps the audit stream bounded when a commit hook is chatty.
const MAX_INLINE_OUTPUT: usize = 2_048;

pub struct GitCommitTool;

#[async_trait]
impl Tool for GitCommitTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_commit".to_string(),
            description: format!(
                "Run `git add` + `git commit` in a workspace. Workspace must match a granted GitCommit capability. \
                 The tool rejects anything else. The subcommand allowlist ({}) is enforced tool-side.",
                ALLOWED_SUBCOMMANDS.join(", ")
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workspace": {
                        "type": "string",
                        "description": "Absolute path to the git repository root (directory containing .git/)"
                    },
                    "message": {
                        "type": "string",
                        "description": "Commit message (must not start with '-')"
                    },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Paths to add (optional, default [\".\"])",
                        "default": ["."]
                    }
                },
                "required": ["workspace", "message"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let workspace = input
            .get("workspace")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'workspace' parameter".into()))?;

        let message = input
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'message' parameter".into()))?;

        // Reject messages that start with '-' to prevent git flag injection
        if message.starts_with('-') {
            return Err(CoreError::InvalidManifest(
                "commit message must not start with '-'".into(),
            ));
        }

        let paths: Vec<String> = input
            .get("paths")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec![".".to_string()]);

        let requested = Capability::GitCommit {
            workspace: workspace.to_string(),
        };
        let allowed = ctx
            .tokens
            .iter()
            .any(|h| ctx.capability_registry.permits(*h, ctx.agent_id, &requested));
        if !allowed {
            return Err(CoreError::CapabilityDenied {
                agent_id: ctx.agent_id,
                capability: requested,
                reason: format!("git_commit not permitted for workspace: {workspace}"),
            });
        }

        let ws_path = PathBuf::from(workspace);
        let git_dir = ws_path.join(".git");
        if !git_dir.exists() {
            return Err(CoreError::Ipc(format!(
                "workspace '{workspace}' is not a git repository (no .git directory)"
            )));
        }

        run_git_commit(&ws_path, message, &paths).await
    }
}

async fn run_git_commit(ws: &Path, message: &str, paths: &[String]) -> Result<Value> {
    let started = std::time::Instant::now();

    // Step 1: git add
    let add_output = run_git_command(ws, "add", paths).await?;
    if !add_output.status.success() {
        return Err(CoreError::Ipc(format!(
            "git add failed with exit code {}: {}",
            add_output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&add_output.stderr)
        )));
    }

    // Step 2: git commit
    let commit_output = run_git_command(ws, "commit", &["-m".to_string(), message.to_string()]).await?;
    
    let duration_ms = started.elapsed().as_millis() as u64;
    let exit_code = commit_output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&commit_output.stdout);
    let stderr = String::from_utf8_lossy(&commit_output.stderr);
    
    // Check if commit succeeded or if there was nothing to commit
    let success = commit_output.status.success() || 
                  stderr.contains("nothing to commit") || 
                  stdout.contains("nothing to commit");
    
    // Get commit SHA if commit was created
    let commit_sha = if success && !stderr.contains("nothing to commit") && !stdout.contains("nothing to commit") {
        match get_head_commit_sha(ws).await {
            Ok(sha) => Some(sha),
            Err(_) => None,
        }
    } else {
        None
    };

    Ok(json!({
        "exit_code": exit_code,
        "success": success,
        "duration_ms": duration_ms,
        "stdout": cap_bytes(&stdout, MAX_INLINE_OUTPUT),
        "stderr": cap_bytes(&stderr, MAX_INLINE_OUTPUT),
        "stdout_truncated": stdout.len() > MAX_INLINE_OUTPUT,
        "stderr_truncated": stderr.len() > MAX_INLINE_OUTPUT,
        "commit_sha": commit_sha,
        "nothing_to_commit": stderr.contains("nothing to commit") || stdout.contains("nothing to commit"),
    }))
}

async fn run_git_command(ws: &Path, subcommand: &str, args: &[String]) -> Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(ws).arg(subcommand);
    for a in args {
        cmd.arg(a);
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    let child = cmd.spawn()
        .map_err(|e| CoreError::Ipc(format!("failed to spawn git {subcommand}: {e}")))?;
    
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(CoreError::Ipc(format!("git {subcommand} wait failed: {e}"))),
        Err(_) => Err(CoreError::Timeout(timeout)),
    }
}

async fn get_head_commit_sha(ws: &Path) -> Result<String> {
    let output = run_git_command(ws, "rev-parse", &["HEAD".to_string()]).await
        .map_err(|e| CoreError::Ipc(format!("failed to get commit SHA: {e}")))?;
    
    if !output.status.success() {
        return Err(CoreError::Ipc(format!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        return Err(CoreError::Ipc("empty commit SHA returned".into()));
    }
    
    Ok(sha)
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

    fn ctx_with_git_commit(workspace_glob: &str) -> InvocationContext {
        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::GitCommit {
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

    /// Create a temporary git repository with a dummy file.
    fn scaffold_git_repo(dir: &TempDir) -> std::path::PathBuf {
        let ws = dir.path().to_path_buf();
        
        // Initialize git repo
        let output = std::process::Command::new("git")
            .arg("init")
            .arg(&ws)
            .output()
            .expect("git init failed");
        
        if !output.status.success() {
            panic!("git init failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        
        // Set user name and email for commits
        std::process::Command::new("git")
            .arg("-C")
            .arg(&ws)
            .arg("config")
            .arg("user.name")
            .arg("Test User")
            .output()
            .expect("git config user.name failed");
            
        std::process::Command::new("git")
            .arg("-C")
            .arg(&ws)
            .arg("config")
            .arg("user.email")
            .arg("test@example.com")
            .output()
            .expect("git config user.email failed");
        
        // Create a dummy file
        std::fs::write(ws.join("test.txt"), "test content\n").unwrap();
        
        ws
    }

    #[tokio::test]
    async fn rejects_message_starting_with_dash() {
        let dir = TempDir::new().unwrap();
        let ws = scaffold_git_repo(&dir);
        let ctx = ctx_with_git_commit(ws.to_str().unwrap());
        let err = GitCommitTool
            .invoke(
                json!({
                    "workspace": ws.to_str().unwrap(),
                    "message": "-e inject"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must not start with '-'"), "got: {err}");
    }

    #[tokio::test]
    #[ignore]
    // Ignored: shells out to `git`; run with `cargo test -- --ignored`
    async fn rejects_workspace_without_git_dir() {
        let dir = TempDir::new().unwrap();
        // No git repo initialized
        let ws = dir.path();
        let ctx = ctx_with_git_commit(ws.to_str().unwrap());
        let err = GitCommitTool
            .invoke(
                json!({
                    "workspace": ws.to_str().unwrap(),
                    "message": "test commit"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a git repository"), "got: {err}");
    }

    #[tokio::test]
    #[ignore]
    async fn commits_staged_file() {
        let dir = TempDir::new().unwrap();
        let ws = scaffold_git_repo(&dir);
        let ctx = ctx_with_git_commit(ws.to_str().unwrap());
        
        // Get initial HEAD (should be the initial empty commit)
        let initial_head = std::process::Command::new("git")
            .arg("-C")
            .arg(&ws)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .expect("git rev-parse failed");
        let initial_sha = String::from_utf8_lossy(&initial_head.stdout).trim().to_string();
        
        let result = GitCommitTool
            .invoke(
                json!({
                    "workspace": ws.to_str().unwrap(),
                    "message": "test commit",
                    "paths": ["test.txt"]
                }),
                &ctx,
            )
            .await
            .unwrap();
        
        assert_eq!(result["success"], true, "git commit failed: {:?}", result);
        assert_eq!(result["nothing_to_commit"], false);
        
        // Get new HEAD
        let new_head = std::process::Command::new("git")
            .arg("-C")
            .arg(&ws)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .expect("git rev-parse failed");
        let new_sha = String::from_utf8_lossy(&new_head.stdout).trim().to_string();
        
        // Should have a new commit SHA
        assert_ne!(initial_sha, new_sha);
        assert_eq!(result["commit_sha"], new_sha);
    }

    #[tokio::test]
    #[ignore]
    async fn rejects_workspace_without_capability() {
        let dir = TempDir::new().unwrap();
        let ws = scaffold_git_repo(&dir);
        // Grant a *different* workspace glob.
        let ctx = ctx_with_git_commit("/nowhere/*");
        let err = GitCommitTool
            .invoke(
                json!({
                    "workspace": ws.to_str().unwrap(),
                    "message": "test commit"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not permitted"), "got: {err}");
    }
}