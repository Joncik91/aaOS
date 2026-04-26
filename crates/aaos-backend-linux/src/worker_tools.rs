//! Tools that execute inside the confined worker. Constructed after
//! `sandboxed-ready` fires — all registered tools therefore run with
//! Landlock + seccomp already applied.
//!
//! Fail-closed: unknown tool names return `TOOL_NOT_AVAILABLE` rather
//! than falling back to daemon-side execution. A routing bug must not
//! silently undo the confinement this module exists to provide.

use std::sync::Arc;

use aaos_tools::registry::ToolRegistry;

/// Explicit whitelist. Tools omitted here run daemon-side via
/// `aaos_core::tool_surface::DAEMON_SIDE_TOOLS`:
/// - Memory tools (`memory_store`/`memory_query`/`memory_delete`): need
///   HTTP access to the embedding endpoint, which the worker sandbox
///   cannot provide.
/// - Subprocess-spawning tools (`cargo_run`, `git_commit`, `grep`): the
///   worker's seccomp kill-filter denies execve, so shelling out to
///   cargo / git / rg inside the worker fails with "Operation not
///   permitted".
/// - Network tools (`web_fetch`): the worker's seccomp allowlist
///   permits `socket()` ONLY with `AF_UNIX` (broker IPC) — TCP/UDP
///   would return EPERM (Bug 34, v0.2.4).  Even if a network socket
///   could be created, no Landlock egress would let it reach the
///   internet; route through the daemon.
pub const WORKER_SIDE_TOOLS: &[&str] = &[
    "echo",
    "file_read",
    "file_write",
    "file_edit",
    "file_list",
    "file_read_many",
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
        assert!(
            reg.get("grep").is_err(),
            "grep must not be worker-side (ripgrep subprocess blocked by seccomp)"
        );
        assert!(
            reg.get("memory_store").is_err(),
            "memory_store must not be worker-side"
        );
        assert!(
            reg.get("memory_query").is_err(),
            "memory_query must not be worker-side"
        );
        assert!(
            reg.get("memory_delete").is_err(),
            "memory_delete must not be worker-side"
        );
    }
}
