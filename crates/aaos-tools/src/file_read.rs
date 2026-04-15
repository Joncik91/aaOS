use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, Result, ToolDefinition};

const MAX_READ_BYTES: u64 = 1_048_576; // 1MB

pub struct FileReadTool;

#[async_trait]
impl Tool for FileReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_read".to_string(),
            description: "Read a file's contents. Path must be allowed by capability tokens."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to the file" }
                },
                "required": ["path"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'path' parameter".into()))?;

        // Check path against FileRead capability tokens
        let requested = Capability::FileRead {
            path_glob: path_str.to_string(),
        };
        let allowed = ctx
            .tokens
            .iter()
            .any(|h| ctx.capability_registry.permits(*h, ctx.agent_id, &requested));
        if !allowed {
            return Err(CoreError::CapabilityDenied {
                agent_id: ctx.agent_id,
                capability: requested,
                reason: format!("file_read not permitted for path: {path_str}"),
            });
        }

        let path = Path::new(path_str);

        // Check file exists and is a regular file
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|e| CoreError::Ipc(format!("file not found: {e}")))?;

        if !metadata.is_file() {
            return Err(CoreError::Ipc(format!("{path_str} is not a regular file")));
        }

        if metadata.len() > MAX_READ_BYTES {
            return Err(CoreError::Ipc(format!(
                "file too large: {} bytes (max {})",
                metadata.len(),
                MAX_READ_BYTES
            )));
        }

        // Read as UTF-8 string (binary files not yet supported)
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| CoreError::Ipc(format!("failed to read file: {e}")))?;

        Ok(json!({
            "content": content,
            "size_bytes": metadata.len(),
        }))
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
    async fn read_file_within_glob() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "hello world").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        // Use wildcard glob that covers /tmp/
        let ctx = ctx_with_read("/tmp/*");
        let result = FileReadTool
            .invoke(json!({"path": path}), &ctx)
            .await
            .unwrap();
        assert_eq!(result["content"], "hello world");
        assert_eq!(result["size_bytes"], 11);
    }

    #[tokio::test]
    async fn read_file_path_denied() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "secret").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        // Glob doesn't cover the file's path
        let ctx = ctx_with_read("/other/*");
        let result = FileReadTool.invoke(json!({"path": path}), &ctx).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not permitted"));
    }

    #[tokio::test]
    async fn read_file_not_found() {
        let ctx = ctx_with_read("/tmp/*");
        let result = FileReadTool
            .invoke(json!({"path": "/tmp/nonexistent-aaos-test-file"}), &ctx)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_missing_path_param() {
        let ctx = ctx_with_read("*");
        let result = FileReadTool.invoke(json!({}), &ctx).await;
        assert!(result.is_err());
    }
}
