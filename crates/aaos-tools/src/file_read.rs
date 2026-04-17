use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, Result, ToolDefinition};

const MAX_READ_BYTES: u64 = 1_048_576; // 1MB
const DEFAULT_LIMIT: usize = 2000;

pub struct FileReadTool;

#[async_trait]
impl Tool for FileReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_read".to_string(),
            description: format!(
                "Read a file's contents with optional line-range slicing. \
                 Returns content prefixed with 1-indexed line numbers (cat -n style) \
                 so edits can reference lines precisely. Default limit: {DEFAULT_LIMIT} lines. \
                 Path must be allowed by capability tokens."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to the file" },
                    "offset": {
                        "type": "integer",
                        "description": "1-indexed line number to start reading from (default: 1)",
                        "minimum": 1
                    },
                    "limit": {
                        "type": "integer",
                        "description": format!("Max number of lines to return (default: {DEFAULT_LIMIT})"),
                        "minimum": 1
                    }
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

        let offset: usize = input
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n.max(1) as usize)
            .unwrap_or(1);

        let limit: usize = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_LIMIT);

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
        let raw = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| CoreError::Ipc(format!("failed to read file: {e}")))?;

        let total_lines = raw.lines().count();
        let start_idx = offset.saturating_sub(1).min(total_lines); // clamp at EOF
        let end_idx = start_idx.saturating_add(limit).min(total_lines);
        let returned_lines = end_idx.saturating_sub(start_idx);

        // Build cat -n style output: "   N\t<line>\n" for each line in [start, end).
        // Using the same width (6) as cat -n for alignment stability; wider files
        // still work, just with a bit of extra right-padding.
        let mut out = String::with_capacity(raw.len().min(64 * 1024));
        for (i, line) in raw.lines().enumerate().skip(start_idx).take(returned_lines) {
            use std::fmt::Write as _;
            let _ = writeln!(out, "{:>6}\t{}", i + 1, line);
        }

        let truncated = end_idx < total_lines;

        Ok(json!({
            "content": out,
            "size_bytes": metadata.len(),
            "total_lines": total_lines,
            "offset": offset,
            "returned_lines": returned_lines,
            "truncated": truncated,
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
        // Line-numbered output: "     1\thello world\n"
        assert!(
            result["content"].as_str().unwrap().contains("hello world"),
            "content should contain original text; got {:?}",
            result["content"]
        );
        assert!(
            result["content"].as_str().unwrap().starts_with("     1\t"),
            "content should be line-numbered (cat -n style)"
        );
        assert_eq!(result["size_bytes"], 11);
        assert_eq!(result["total_lines"], 1);
        assert_eq!(result["offset"], 1);
        assert_eq!(result["truncated"], false);
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        use std::io::Write as _;
        let mut tmp = NamedTempFile::new().unwrap();
        for i in 1..=10 {
            writeln!(tmp, "line{i}").unwrap();
        }
        let path = tmp.path().to_str().unwrap().to_string();

        let ctx = ctx_with_read("/tmp/*");
        let result = FileReadTool
            .invoke(json!({"path": path, "offset": 3, "limit": 4}), &ctx)
            .await
            .unwrap();

        let content = result["content"].as_str().unwrap();
        // Should contain lines 3..6 inclusive, not 1, 2, 7, 8, 9, 10
        assert!(content.contains("line3"));
        assert!(content.contains("line6"));
        assert!(!content.contains("line2"));
        assert!(!content.contains("line7"));
        assert_eq!(result["total_lines"], 10);
        assert_eq!(result["returned_lines"], 4);
        assert_eq!(result["offset"], 3);
        assert_eq!(result["truncated"], true);
    }

    #[tokio::test]
    async fn read_offset_past_end_returns_empty() {
        use std::io::Write as _;
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "only line").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let ctx = ctx_with_read("/tmp/*");
        let result = FileReadTool
            .invoke(json!({"path": path, "offset": 100}), &ctx)
            .await
            .unwrap();

        assert_eq!(result["content"].as_str().unwrap(), "");
        assert_eq!(result["returned_lines"], 0);
        assert_eq!(result["truncated"], false);
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
