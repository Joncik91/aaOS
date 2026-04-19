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
/// - `web_fetch`: seccomp allowlist has no socket/connect syscalls.
/// - `cargo_run`, `git_commit`: seccomp kill-filter denies execve.
pub const DAEMON_SIDE_TOOLS: &[&str] = &["web_fetch", "cargo_run", "git_commit"];

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
        assert_eq!(route_for("file_write", "in_process"), ToolExecutionSurface::Daemon);
        assert_eq!(route_for("web_fetch", "in_process"), ToolExecutionSurface::Daemon);
    }

    #[test]
    fn namespaced_routes_most_to_worker() {
        assert_eq!(route_for("file_write", "namespaced"), ToolExecutionSurface::Worker);
        assert_eq!(route_for("grep", "namespaced"), ToolExecutionSurface::Worker);
    }

    #[test]
    fn namespaced_keeps_daemon_side_list_on_daemon() {
        for t in DAEMON_SIDE_TOOLS {
            assert_eq!(route_for(t, "namespaced"), ToolExecutionSurface::Daemon, "{t}");
        }
    }
}
