use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, Result, ToolDefinition};

/// Upper bound on the old/new strings in one edit. Both params travel
/// inline in the LLM's tool-call JSON, so keep them bounded to protect
/// the context window and the audit-preview sink.
const MAX_EDIT_STRING_BYTES: usize = 64 * 1024;

/// Maximum size of the file being edited. Anything larger should be
/// rewritten via `file_write`, not surgically patched.
const MAX_FILE_BYTES: u64 = 1_048_576; // 1MB

/// Surgical find-and-replace in a single file.
///
/// Requires both `FileRead` and `FileWrite` capability for the path —
/// the tool reads the file to locate `old_string` and writes the
/// modified content back. This matches the Edit tool idiom in Claude
/// Code, Cursor, Aider, and other mainstream coding agents.
///
/// Uniqueness rule: if `old_string` matches more than once in the file,
/// the edit is refused unless `replace_all: true`. This prevents the
/// common LLM mistake of rewriting the first match when the intent was
/// a different occurrence.
pub struct FileEditTool;

#[async_trait]
impl Tool for FileEditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_edit".to_string(),
            description: "Surgically replace a unique substring in a file. \
                 Requires FileRead + FileWrite capability for the path. \
                 If `old_string` matches more than once, the call is refused \
                 unless `replace_all: true`. Use this for small edits; use \
                 `file_write` to create new files or do total rewrites."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path to the file to edit"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact substring to replace. Must appear in the file. \
                                        Must be unique unless replace_all is true."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text. May be empty (deletes old_string)."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "If true, replace every occurrence. Default: false."
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'path' parameter".into()))?;

        let old_string = input
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'old_string' parameter".into()))?;

        let new_string = input
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'new_string' parameter".into()))?;

        let replace_all = input
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old_string.is_empty() {
            return Err(CoreError::InvalidManifest(
                "old_string must be non-empty".into(),
            ));
        }

        if old_string.len() > MAX_EDIT_STRING_BYTES || new_string.len() > MAX_EDIT_STRING_BYTES {
            return Err(CoreError::Ipc(format!(
                "edit string too large: max {MAX_EDIT_STRING_BYTES} bytes per side"
            )));
        }

        // Capability check: need BOTH read and write for this path.
        let read_cap = Capability::FileRead {
            path_glob: path_str.to_string(),
        };
        let write_cap = Capability::FileWrite {
            path_glob: path_str.to_string(),
        };
        let has_read = ctx
            .tokens
            .iter()
            .any(|h| ctx.capability_registry.permits(*h, ctx.agent_id, &read_cap));
        let has_write = ctx
            .tokens
            .iter()
            .any(|h| ctx.capability_registry.permits(*h, ctx.agent_id, &write_cap));

        if !has_read {
            return Err(CoreError::CapabilityDenied {
                agent_id: ctx.agent_id,
                capability: read_cap,
                reason: format!("file_edit needs file_read for path: {path_str}"),
            });
        }
        if !has_write {
            return Err(CoreError::CapabilityDenied {
                agent_id: ctx.agent_id,
                capability: write_cap,
                reason: format!("file_edit needs file_write for path: {path_str}"),
            });
        }

        let path = Path::new(path_str);
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|e| CoreError::Ipc(format!("file not found: {e}")))?;

        if !metadata.is_file() {
            return Err(CoreError::Ipc(format!("{path_str} is not a regular file")));
        }

        if metadata.len() > MAX_FILE_BYTES {
            return Err(CoreError::Ipc(format!(
                "file too large to edit in-place: {} bytes (max {}). \
                 Use file_write for total rewrite.",
                metadata.len(),
                MAX_FILE_BYTES
            )));
        }

        let original = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| CoreError::Ipc(format!("failed to read file: {e}")))?;

        let match_count = count_non_overlapping(&original, old_string);
        if match_count == 0 {
            return Err(CoreError::Ipc(format!(
                "old_string not found in {path_str}"
            )));
        }
        if match_count > 1 && !replace_all {
            return Err(CoreError::Ipc(format!(
                "old_string matches {match_count} times in {path_str}; \
                 refusing ambiguous edit. Pass replace_all=true to replace all, \
                 or extend old_string with more context to make it unique."
            )));
        }

        let modified = if replace_all {
            original.replace(old_string, new_string)
        } else {
            // Single match — replacen(…, 1) is the precise primitive.
            original.replacen(old_string, new_string, 1)
        };

        let replacements = if replace_all { match_count } else { 1 };

        tokio::fs::write(path, modified.as_bytes())
            .await
            .map_err(|e| CoreError::Ipc(format!("failed to write file: {e}")))?;

        Ok(json!({
            "path": path_str,
            "replacements": replacements,
            "bytes_before": metadata.len(),
            "bytes_after": modified.len(),
        }))
    }
}

/// Count non-overlapping occurrences of `needle` in `haystack`.
/// Matches `str::replace`'s semantics so the count matches what the
/// replacement will actually do.
fn count_non_overlapping(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut rest = haystack;
    while let Some(pos) = rest.find(needle) {
        count += 1;
        rest = &rest[pos + needle.len()..];
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, CapabilityRegistry, CapabilityToken, Constraints};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx_with_read_write(glob: &str) -> InvocationContext {
        let agent_id = AgentId::new();
        let registry = Arc::new(CapabilityRegistry::new());
        let read_token = CapabilityToken::issue(
            agent_id,
            Capability::FileRead {
                path_glob: glob.to_string(),
            },
            Constraints::default(),
        );
        let write_token = CapabilityToken::issue(
            agent_id,
            Capability::FileWrite {
                path_glob: glob.to_string(),
            },
            Constraints::default(),
        );
        let h1 = registry.insert(agent_id, read_token);
        let h2 = registry.insert(agent_id, write_token);
        InvocationContext {
            agent_id,
            tokens: vec![h1, h2],
            capability_registry: registry,
        }
    }

    fn ctx_with_only_read(glob: &str) -> InvocationContext {
        let agent_id = AgentId::new();
        let registry = Arc::new(CapabilityRegistry::new());
        let read_token = CapabilityToken::issue(
            agent_id,
            Capability::FileRead {
                path_glob: glob.to_string(),
            },
            Constraints::default(),
        );
        let h = registry.insert(agent_id, read_token);
        InvocationContext {
            agent_id,
            tokens: vec![h],
            capability_registry: registry,
        }
    }

    fn write_file(dir: &TempDir, name: &str, content: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn edit_single_unique_match() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "a.txt", "alpha beta gamma\n");
        let glob = format!("{}/*", dir.path().to_str().unwrap());
        let ctx = ctx_with_read_write(&glob);

        let result = FileEditTool
            .invoke(
                json!({
                    "path": path,
                    "old_string": "beta",
                    "new_string": "BETA"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result["replacements"], 1);

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "alpha BETA gamma\n");
    }

    #[tokio::test]
    async fn edit_refuses_ambiguous_match() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "b.txt", "foo bar foo bar foo\n");
        let glob = format!("{}/*", dir.path().to_str().unwrap());
        let ctx = ctx_with_read_write(&glob);

        let err = FileEditTool
            .invoke(
                json!({
                    "path": path,
                    "old_string": "foo",
                    "new_string": "FOO"
                }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("3 times"), "got: {err}");
        assert!(err.contains("replace_all"), "got: {err}");

        // File unchanged after refused edit.
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "foo bar foo bar foo\n");
    }

    #[tokio::test]
    async fn edit_replace_all_when_requested() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "c.txt", "foo bar foo bar foo\n");
        let glob = format!("{}/*", dir.path().to_str().unwrap());
        let ctx = ctx_with_read_write(&glob);

        let result = FileEditTool
            .invoke(
                json!({
                    "path": path,
                    "old_string": "foo",
                    "new_string": "FOO",
                    "replace_all": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result["replacements"], 3);

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "FOO bar FOO bar FOO\n");
    }

    #[tokio::test]
    async fn edit_not_found_errors() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "d.txt", "hello\n");
        let glob = format!("{}/*", dir.path().to_str().unwrap());
        let ctx = ctx_with_read_write(&glob);

        let err = FileEditTool
            .invoke(
                json!({
                    "path": path,
                    "old_string": "nonexistent",
                    "new_string": "x"
                }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn edit_requires_write_capability() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "e.txt", "hello\n");
        let glob = format!("{}/*", dir.path().to_str().unwrap());
        let ctx = ctx_with_only_read(&glob);

        let err = FileEditTool
            .invoke(
                json!({
                    "path": path,
                    "old_string": "hello",
                    "new_string": "world"
                }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("file_write"), "got: {err}");
    }

    #[tokio::test]
    async fn edit_empty_old_string_rejected() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "f.txt", "hello\n");
        let glob = format!("{}/*", dir.path().to_str().unwrap());
        let ctx = ctx_with_read_write(&glob);

        let err = FileEditTool
            .invoke(
                json!({
                    "path": path,
                    "old_string": "",
                    "new_string": "x"
                }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-empty"), "got: {err}");
    }

    #[test]
    fn count_non_overlapping_counts_correctly() {
        assert_eq!(count_non_overlapping("aaaa", "aa"), 2);
        assert_eq!(count_non_overlapping("abc abc abc", "abc"), 3);
        assert_eq!(count_non_overlapping("hello", "z"), 0);
        assert_eq!(count_non_overlapping("", "a"), 0);
    }
}
