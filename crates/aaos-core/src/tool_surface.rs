//! Per-tool-call execution surface (daemon vs worker) and the static
//! list of tools that must always run daemon-side because the worker
//! sandbox cannot host them (no network, no subprocess execution).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionSurface {
    #[default]
    Daemon,
    Worker,
}

impl ToolExecutionSurface {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolExecutionSurface::Daemon => "daemon",
            ToolExecutionSurface::Worker => "worker",
        }
    }
}

/// Tools that must always execute daemon-side, regardless of backend.
///
/// - `web_fetch`: the worker's seccomp allowlist permits `socket()`
///   only with `AF_UNIX` (broker IPC).  TCP/UDP (`AF_INET`/`AF_INET6`)
///   and other address families return EPERM, so HTTP outbound from
///   the worker is structurally impossible.  v0.2.4 (Bug 34) tightened
///   this; earlier doc claims of "no socket/connect syscalls" were
///   factually wrong (the syscalls were allowed, just unused) — the
///   current claim is honest: socket() is allowed but argument-filtered.
/// - `cargo_run`, `git_commit`, `grep`: seccomp kill-filter denies
///   execve — these tools shell out to external binaries (cargo, git,
///   rg) and would fail with "Operation not permitted" under the
///   worker sandbox.
/// - `memory_store`, `memory_query`, `memory_delete`: require HTTP access
///   to the embedding endpoint (Ollama / OpenAI-compatible), which the
///   worker sandbox cannot provide.
pub const DAEMON_SIDE_TOOLS: &[&str] = &[
    "web_fetch",
    "cargo_run",
    "git_commit",
    "grep",
    "memory_store",
    "memory_query",
    "memory_delete",
];

/// Return the intended execution surface for a tool call given the
/// active backend kind (as reported by `AgentLaunchHandle::backend_kind`).
pub fn route_for(tool_name: &str, backend_kind: &str) -> ToolExecutionSurface {
    if backend_kind != "namespaced" || DAEMON_SIDE_TOOLS.contains(&tool_name) {
        ToolExecutionSurface::Daemon
    } else {
        ToolExecutionSurface::Worker
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_process_always_daemon() {
        assert_eq!(
            route_for("file_write", "in_process"),
            ToolExecutionSurface::Daemon
        );
        assert_eq!(
            route_for("web_fetch", "in_process"),
            ToolExecutionSurface::Daemon
        );
    }

    #[test]
    fn namespaced_routes_most_to_worker() {
        assert_eq!(
            route_for("file_write", "namespaced"),
            ToolExecutionSurface::Worker
        );
        assert_eq!(
            route_for("file_read", "namespaced"),
            ToolExecutionSurface::Worker
        );
    }

    #[test]
    fn namespaced_keeps_daemon_side_list_on_daemon() {
        for t in DAEMON_SIDE_TOOLS {
            assert_eq!(
                route_for(t, "namespaced"),
                ToolExecutionSurface::Daemon,
                "{t}"
            );
        }
    }
}
