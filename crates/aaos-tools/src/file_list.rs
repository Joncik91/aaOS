use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, Result, ToolDefinition};

const MAX_ENTRIES: usize = 500;

pub struct FileListTool;

#[async_trait]
impl Tool for FileListTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_list".to_string(),
            description:
                "List the contents of a directory. Returns entries with name, kind (file|dir|other), and size_bytes. \
                 Path must be allowed by a FileRead capability. For a single file, returns one entry."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to a directory or file" }
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
                reason: format!("file_list not permitted for path: {path_str}"),
            });
        }

        let path = Path::new(path_str);
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|e| CoreError::Ipc(format!("path not found: {e}")))?;

        if metadata.is_file() {
            return Ok(json!({
                "path": path_str,
                "kind": "file",
                "entries": [{
                    "name": path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                    "kind": "file",
                    "size_bytes": metadata.len(),
                }]
            }));
        }

        if !metadata.is_dir() {
            return Err(CoreError::Ipc(format!("{path_str} is neither a file nor a directory")));
        }

        let mut entries = Vec::new();
        let mut rd = tokio::fs::read_dir(path)
            .await
            .map_err(|e| CoreError::Ipc(format!("failed to read dir: {e}")))?;
        let mut truncated = false;
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| CoreError::Ipc(format!("dir iter failed: {e}")))?
        {
            if entries.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let ft = entry.file_type().await.ok();
            let kind = match ft {
                Some(t) if t.is_file() => "file",
                Some(t) if t.is_dir() => "dir",
                Some(t) if t.is_symlink() => "symlink",
                _ => "other",
            };
            let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
            entries.push(json!({
                "name": name,
                "kind": kind,
                "size_bytes": size,
            }));
        }
        entries.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });

        Ok(json!({
            "path": path_str,
            "kind": "dir",
            "entries": entries,
            "truncated": truncated,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, CapabilityRegistry, CapabilityToken, Constraints};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn ctx_with_read(path_glob: &str) -> InvocationContext {
        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::FileRead {
                path_glob: path_glob.to_string(),
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
    async fn lists_directory_contents() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), b"hi").await.unwrap();
        tokio::fs::write(dir.path().join("b.txt"), b"hello").await.unwrap();
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();

        let glob = format!("{}/*", dir.path().display());
        let tool = FileListTool;
        let out = tool
            .invoke(
                json!({ "path": dir.path().to_str().unwrap() }),
                &ctx_with_read(&glob),
            )
            .await
            .unwrap();

        assert_eq!(out["kind"], "dir");
        let entries = out["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        let names: Vec<&str> = entries.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
        assert!(names.contains(&"sub"));
    }

    #[tokio::test]
    async fn single_file_returns_one_entry() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("x.txt");
        tokio::fs::write(&file, b"data").await.unwrap();

        let glob = format!("{}/*", dir.path().display());
        let tool = FileListTool;
        let out = tool
            .invoke(json!({ "path": file.to_str().unwrap() }), &ctx_with_read(&glob))
            .await
            .unwrap();

        assert_eq!(out["kind"], "file");
        assert_eq!(out["entries"].as_array().unwrap().len(), 1);
        assert_eq!(out["entries"][0]["name"], "x.txt");
    }

    #[tokio::test]
    async fn denies_without_capability() {
        let dir = tempdir().unwrap();
        let tool = FileListTool;
        let out = tool
            .invoke(
                json!({ "path": dir.path().to_str().unwrap() }),
                &ctx_with_read("/etc/*"),
            )
            .await;
        assert!(matches!(out, Err(CoreError::CapabilityDenied { .. })));
    }

    #[tokio::test]
    async fn path_traversal_denied() {
        let tool = FileListTool;
        let out = tool
            .invoke(
                json!({ "path": "/data/../etc" }),
                &ctx_with_read("/data/*"),
            )
            .await;
        assert!(matches!(out, Err(CoreError::CapabilityDenied { .. })));
    }

    #[tokio::test]
    async fn missing_path_errors_clearly() {
        let tool = FileListTool;
        let out = tool
            .invoke(
                json!({ "path": "/definitely/not/there/xyz" }),
                &ctx_with_read("/*"),
            )
            .await;
        assert!(out.is_err());
    }
}
