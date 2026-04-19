//! Tools that execute inside the confined worker. Constructed after
//! `sandboxed-ready` fires — all registered tools therefore run with
//! Landlock + seccomp already applied.
//!
//! Fail-closed: unknown tool names return `TOOL_NOT_AVAILABLE` rather
//! than falling back to daemon-side execution. A routing bug must not
//! silently undo the confinement this module exists to provide.

use std::sync::Arc;

use aaos_tools::registry::ToolRegistry;

/// Explicit whitelist. Memory + skill tools are omitted in v1 because
/// they require a memory store that is not wired into the worker yet;
/// those stay daemon-side until a follow-up sub-project adds a
/// broker-mediated memory backend.
pub const WORKER_SIDE_TOOLS: &[&str] = &[
    "echo",
    "file_read",
    "file_write",
    "file_edit",
    "file_list",
    "file_read_many",
    "grep",
];

/// Build a registry containing only the worker-safe tools.
pub fn build_worker_registry() -> Arc<ToolRegistry> {
    let reg = ToolRegistry::new();
    reg.register(Arc::new(aaos_tools::EchoTool));
    reg.register(Arc::new(aaos_tools::FileReadTool));
    reg.register(Arc::new(aaos_tools::FileWriteTool));
    reg.register(Arc::new(aaos_tools::FileEditTool));
    reg.register(Arc::new(aaos_tools::FileListTool));
    reg.register(Arc::new(aaos_tools::FileReadManyTool));
    reg.register(Arc::new(aaos_tools::GrepTool));
    Arc::new(reg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_registry_has_whitelist_only() {
        let reg = build_worker_registry();
        for name in WORKER_SIDE_TOOLS {
            assert!(reg.get(name).is_ok(), "missing tool: {name}");
        }
        assert!(
            reg.get("web_fetch").is_err(),
            "web_fetch must not be worker-side"
        );
        assert!(
            reg.get("cargo_run").is_err(),
            "cargo_run must not be worker-side"
        );
        assert!(
            reg.get("git_commit").is_err(),
            "git_commit must not be worker-side"
        );
    }
}
