use async_trait::async_trait;
use serde_json::{json, Value};

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, FileAccess, Result, ToolDefinition};

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

        // TOCTOU-safe end-to-end: open ONCE with O_NOFOLLOW for read,
        // resolve the canonical via /proc/self/fd/<fd>, run the
        // capability check on that canonical, then perform metadata +
        // listing through the SAME fd (fstat + Dir::from_fd).  The
        // inode is pinned by the fd from open until close — a
        // symlink-swap or directory rename on the requested path
        // between the check and the listing has no effect.  Earlier
        // versions (Bug 29 in v0.2.1) re-opened by canonical-string
        // for the listing, which left the residual TOCTOU window.
        //
        // Discriminate file vs dir by trying the directory-open
        // first: if it succeeds the path is a directory and we can
        // use Dir::from_fd; if ENOTDIR it's a regular file and we
        // fall through to the Read mode for fstat.
        //
        // Capability snapshot to move into spawn_blocking — DashMap is
        // Send/Sync so cloning the Arc<CapabilityRegistry> is cheap.
        let path_owned = path_str.to_string();
        let tokens = ctx.tokens.clone();
        let agent_id = ctx.agent_id;
        let registry = ctx.capability_registry.clone();

        tokio::task::spawn_blocking(move || -> Result<Value> {
            #[cfg(target_os = "linux")]
            {
                use std::os::fd::IntoRawFd;

                // Try directory-open first.
                let dir_attempt = crate::path_safe::safe_open_for_capability(
                    &path_owned,
                    crate::path_safe::AccessMode::ReadDir,
                );

                let (fd, canonical, is_dir) = match dir_attempt {
                    Ok((fd, c)) => (fd, c, true),
                    Err(e) => {
                        // Anything other than "not a directory" is a hard
                        // failure — propagate. ENOTDIR (file_list called
                        // on a regular file) falls through to Read mode.
                        if !e.to_string().contains("ENOTDIR")
                            && !e.to_string().contains("Not a directory")
                        {
                            // Try Read mode anyway — open errors that look
                            // path-related (NotFound, symlink) get the same
                            // diagnostic by re-running through Read mode.
                            let (fd, c) = crate::path_safe::safe_open_for_capability(
                                &path_owned,
                                crate::path_safe::AccessMode::Read,
                            )?;
                            (fd, c, false)
                        } else {
                            let (fd, c) = crate::path_safe::safe_open_for_capability(
                                &path_owned,
                                crate::path_safe::AccessMode::Read,
                            )?;
                            (fd, c, false)
                        }
                    }
                };

                // Capability check on the kernel-pinned canonical.
                let requested = Capability::FileRead {
                    path_glob: canonical.clone(),
                };
                let allowed = tokens.iter().any(|h| {
                    registry.permits_canonical_file(*h, agent_id, FileAccess::Read, &canonical)
                });
                if !allowed {
                    return Err(CoreError::CapabilityDenied {
                        agent_id,
                        capability: requested,
                        reason: format!("file_list not permitted for path: {canonical}"),
                    });
                }

                // Single-file path: fstat through the fd, return one entry.
                if !is_dir {
                    let std_file = std::fs::File::from(fd);
                    let metadata = std_file
                        .metadata()
                        .map_err(|e| CoreError::Ipc(format!("metadata: {e}")))?;
                    let name = std::path::Path::new(&canonical)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    let kind = if metadata.is_file() {
                        "file"
                    } else {
                        return Err(CoreError::Ipc(format!(
                            "{canonical} is neither a regular file nor a directory"
                        )));
                    };
                    return Ok(json!({
                        "path": canonical,
                        "kind": kind,
                        "entries": [{
                            "name": name,
                            "kind": kind,
                            "size_bytes": metadata.len(),
                        }]
                    }));
                }

                // Directory path: Dir::from_fd consumes the fd's inode
                // ownership.  Iterate entries through the pinned inode;
                // a leaf-component symlink swap after the
                // safe_open_for_capability call cannot redirect us.
                let raw_fd = fd.into_raw_fd();
                let mut dir = nix::dir::Dir::from_fd(raw_fd).map_err(|e| {
                    CoreError::Ipc(format!("fdopendir failed for {canonical}: {e}"))
                })?;

                let mut entries = Vec::new();
                let mut truncated = false;
                for entry in dir.iter() {
                    if entries.len() >= MAX_ENTRIES {
                        truncated = true;
                        break;
                    }
                    let entry =
                        entry.map_err(|e| CoreError::Ipc(format!("dir iter failed: {e}")))?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let kind = match entry.file_type() {
                        Some(nix::dir::Type::File) => "file",
                        Some(nix::dir::Type::Directory) => "dir",
                        Some(nix::dir::Type::Symlink) => "symlink",
                        _ => "other",
                    };
                    // size_bytes via name-based fstatat would defeat the
                    // TOCTOU goal; we report 0 for entries that aren't
                    // worth a syscall round-trip.  Operators looking
                    // for sizes can file_read(path) the entry of interest.
                    entries.push(json!({
                        "name": name,
                        "kind": kind,
                        "size_bytes": 0,
                    }));
                }
                entries.sort_by(|a, b| {
                    a["name"]
                        .as_str()
                        .unwrap_or("")
                        .cmp(b["name"].as_str().unwrap_or(""))
                });

                Ok(json!({
                    "path": canonical,
                    "kind": "dir",
                    "entries": entries,
                    "truncated": truncated,
                }))
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(CoreError::Ipc(
                    "file_list TOCTOU-safe path requires Linux".to_string(),
                ))
            }
        })
        .await
        .map_err(|e| CoreError::Ipc(format!("file_list join: {e}")))?
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
        tokio::fs::write(dir.path().join("a.txt"), b"hi")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.txt"), b"hello")
            .await
            .unwrap();
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
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
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
            .invoke(
                json!({ "path": file.to_str().unwrap() }),
                &ctx_with_read(&glob),
            )
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
        // /tmp/../etc collapses to /etc — canonical from the fd readlink
        // is "/etc", which fails to match a /tmp/* glob, so the
        // capability check denies. This validates that traversal can't
        // be used to escape the granted glob (the fix is structural:
        // canonical comes from the fd, not the input string).
        let tool = FileListTool;
        let out = tool
            .invoke(json!({ "path": "/tmp/../etc" }), &ctx_with_read("/tmp/*"))
            .await;
        assert!(
            matches!(out, Err(CoreError::CapabilityDenied { .. })),
            "expected CapabilityDenied, got: {out:?}"
        );
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
