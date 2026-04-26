use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, FileAccess, Result, ToolDefinition};

const MAX_WRITE_BYTES: usize = 1_048_576; // 1MB

pub struct FileWriteTool;

#[async_trait]
impl Tool for FileWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_write".to_string(),
            description: "Write content to a file. Path must be allowed by capability tokens."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to write to" },
                    "content": { "type": "string", "description": "Content to write" },
                    "append": { "type": "boolean", "description": "Append instead of overwrite (default: false)" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'path' parameter".into()))?;

        let content = input
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'content' parameter".into()))?;

        let append = input
            .get("append")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if content.len() > MAX_WRITE_BYTES {
            return Err(CoreError::Ipc(format!(
                "content too large: {} bytes (max {})",
                content.len(),
                MAX_WRITE_BYTES
            )));
        }

        let path = Path::new(path_str);

        // Pre-create parent directories.  The capability check + I/O
        // below run through `safe_open_for_capability` which uses
        // `openat2(RESOLVE_NO_SYMLINKS)` (Bug 32, v0.2.3) so symlinks
        // at any path component — leaf or intermediate — are rejected.
        // The parent-dir create is the only path-string operation
        // remaining; tokio::fs::create_dir_all does NOT use
        // RESOLVE_NO_SYMLINKS, so an attacker with mid-traversal
        // symlink-write access could still steer the create.  But
        // since the subsequent `safe_open_for_capability` then refuses
        // any symlink-containing path, the worst case is a directory
        // tree gets created in an attacker location and the open fails
        // closed — no file write lands.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CoreError::Ipc(format!("failed to create directories: {e}")))?;
        }

        // TOCTOU-safe open: O_NOFOLLOW + capability check on the
        // /proc/self/fd/<fd> canonical, write through the same fd.
        let path_owned = path_str.to_string();
        let (fd, canonical) =
            tokio::task::spawn_blocking(move || -> Result<(std::os::fd::OwnedFd, String)> {
                #[cfg(target_os = "linux")]
                {
                    let mode = if append {
                        crate::path_safe::AccessMode::WriteCreateAppend
                    } else {
                        crate::path_safe::AccessMode::WriteCreateTrunc
                    };
                    crate::path_safe::safe_open_for_capability(&path_owned, mode)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Err(CoreError::Ipc(
                        "file_write TOCTOU-safe path requires Linux".to_string(),
                    ))
                }
            })
            .await
            .map_err(|e| CoreError::Ipc(format!("safe_open join: {e}")))??;

        let requested = Capability::FileWrite {
            path_glob: canonical.clone(),
        };
        let allowed = ctx.tokens.iter().any(|h| {
            ctx.capability_registry.permits_canonical_file(
                *h,
                ctx.agent_id,
                FileAccess::Write,
                &canonical,
            )
        });
        if !allowed {
            // The fd is dropped here, closing the (possibly newly-created)
            // file. We do not unlink — leaving an empty file behind is
            // less surprising than racing rmdir on the parent path.
            return Err(CoreError::CapabilityDenied {
                agent_id: ctx.agent_id,
                capability: requested,
                reason: format!("file_write not permitted for path: {canonical}"),
            });
        }

        let std_file = std::fs::File::from(fd);
        let mut tokio_file = tokio::fs::File::from_std(std_file);
        use tokio::io::AsyncWriteExt;
        tokio_file
            .write_all(content.as_bytes())
            .await
            .map_err(|e| CoreError::Ipc(format!("failed to write: {e}")))?;
        tokio_file
            .flush()
            .await
            .map_err(|e| CoreError::Ipc(format!("failed to flush: {e}")))?;

        Ok(json!({
            "bytes_written": content.len(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, CapabilityRegistry, CapabilityToken, Constraints};
    use std::sync::Arc;

    fn ctx_with_write(glob: &str) -> InvocationContext {
        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::FileWrite {
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
    async fn write_file_within_glob() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        let path_str = path.to_str().unwrap();
        let glob = format!("{}/*", dir.path().to_str().unwrap());

        let ctx = ctx_with_write(&glob);
        let result = FileWriteTool
            .invoke(json!({"path": path_str, "content": "hello"}), &ctx)
            .await
            .unwrap();
        assert_eq!(result["bytes_written"], 5);

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn write_file_path_denied() {
        // Use two writable tempdirs so the test runs as non-root (CI):
        // grant covers `allowed/*`, request lands in `denied/`.
        // Pre-Bug-32 the test used absolute paths (/allowed, /denied)
        // which only worked under root because `create_dir_all` could
        // create top-level dirs.  CI runs as non-root and the test
        // started failing as create_dir_all returned EACCES before the
        // capability check could run.
        let dir = tempfile::tempdir().unwrap();
        let allowed_glob = format!("{}/allowed/*", dir.path().display());
        let denied_path = dir.path().join("denied").join("file.txt");
        let ctx = ctx_with_write(&allowed_glob);
        let result = FileWriteTool
            .invoke(
                json!({"path": denied_path.to_str().unwrap(), "content": "bad"}),
                &ctx,
            )
            .await;
        assert!(result.is_err(), "expected denial");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not permitted"),
            "expected 'not permitted' in error, got: {err}"
        );
    }

    #[tokio::test]
    async fn write_file_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("append.txt");
        let path_str = path.to_str().unwrap();
        let glob = format!("{}/*", dir.path().to_str().unwrap());

        let ctx = ctx_with_write(&glob);

        // First write
        FileWriteTool
            .invoke(json!({"path": path_str, "content": "aaa"}), &ctx)
            .await
            .unwrap();

        // Append
        FileWriteTool
            .invoke(
                json!({"path": path_str, "content": "bbb", "append": true}),
                &ctx,
            )
            .await
            .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "aaabbb");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("dir").join("file.txt");
        let path_str = path.to_str().unwrap();
        let glob = format!("{}/*", dir.path().to_str().unwrap());

        let ctx = ctx_with_write(&glob);
        FileWriteTool
            .invoke(json!({"path": path_str, "content": "nested"}), &ctx)
            .await
            .unwrap();

        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "nested");
    }
}
