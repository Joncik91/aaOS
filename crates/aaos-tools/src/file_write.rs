use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, Result, ToolDefinition};

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

        // Check path against FileWrite capability tokens
        let requested = Capability::FileWrite {
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
                reason: format!("file_write not permitted for path: {path_str}"),
            });
        }

        let path = Path::new(path_str);

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CoreError::Ipc(format!("failed to create directories: {e}")))?;
        }

        // Write or append
        if append {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await
                .map_err(|e| CoreError::Ipc(format!("failed to open file: {e}")))?;
            file.write_all(content.as_bytes())
                .await
                .map_err(|e| CoreError::Ipc(format!("failed to write: {e}")))?;
            file.flush()
                .await
                .map_err(|e| CoreError::Ipc(format!("failed to flush: {e}")))?;
        } else {
            tokio::fs::write(path, content.as_bytes())
                .await
                .map_err(|e| CoreError::Ipc(format!("failed to write: {e}")))?;
        }

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
        let ctx = ctx_with_write("/allowed/*");
        let result = FileWriteTool
            .invoke(json!({"path": "/denied/file.txt", "content": "bad"}), &ctx)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not permitted"));
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
