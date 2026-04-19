//! CLI error types, exit-code mapping, and operator-facing hint rendering.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("daemon not reachable at {0}: {1}")]
    DaemonUnreachable(String, String),
    #[error("{0}")]
    Usage(String),
    #[error("agent-reported failure: {0}")]
    AgentFailed(String),
    #[error("server error: {0}")]
    ServerError(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("broken pipe")]
    BrokenPipe,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub fn exit_code(e: &CliError) -> i32 {
    match e {
        CliError::Usage(_) => 2,
        CliError::AgentFailed(_) => 1,
        CliError::DaemonUnreachable(_, _)
        | CliError::BrokenPipe
        | CliError::ServerError(_)
        | CliError::Protocol(_)
        | CliError::Io(_) => 3,
    }
}

pub fn format_error(e: &CliError) -> String {
    match e {
        CliError::DaemonUnreachable(path, inner) => {
            let mut out = format!("error: daemon not reachable at {}\n\n", path);
            if inner.to_lowercase().contains("permission denied") {
                out.push_str("  Are you in the aaos group?   groups\n");
                out.push_str(
                    "  If not:                       sudo adduser $USER aaos  (log out + in)\n",
                );
            } else {
                out.push_str("  Is agentd running?   systemctl status agentd\n");
                out.push_str("  Check the journal:   journalctl -u agentd -n 50\n");
            }
            out
        }
        CliError::Usage(msg) => format!("error: {}\n", msg),
        CliError::AgentFailed(msg) => format!("error: agent failed: {}\n", msg),
        CliError::BrokenPipe => {
            "error: connection closed mid-stream\n\n  Daemon may have restarted. Try again.\n"
                .into()
        }
        CliError::Protocol(msg) => format!(
            "error: protocol: {}\n\n  Version skew: upgrade CLI or daemon to matching versions.\n",
            msg
        ),
        CliError::ServerError(msg) => format!("error: server: {}\n", msg),
        CliError::Io(e) => format!("error: {}\n", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_exit_code_is_2() {
        let e = CliError::Usage("missing --flag".into());
        assert_eq!(exit_code(&e), 2);
    }

    #[test]
    fn daemon_unreachable_exit_code_is_3() {
        let e = CliError::DaemonUnreachable(
            "/run/agentd/agentd.sock".into(),
            "Connection refused".into(),
        );
        assert_eq!(exit_code(&e), 3);
    }

    #[test]
    fn agent_failed_exit_code_is_1() {
        let e = CliError::AgentFailed("couldn't do it".into());
        assert_eq!(exit_code(&e), 1);
    }

    #[test]
    fn broken_pipe_exit_code_is_3() {
        let e = CliError::BrokenPipe;
        assert_eq!(exit_code(&e), 3);
    }

    #[test]
    fn server_error_exit_code_is_3() {
        let e = CliError::ServerError("bad".into());
        assert_eq!(exit_code(&e), 3);
    }

    #[test]
    fn protocol_error_exit_code_is_3() {
        let e = CliError::Protocol("bad frame".into());
        assert_eq!(exit_code(&e), 3);
    }

    #[test]
    fn io_error_exit_code_is_3() {
        let e = CliError::Io(std::io::Error::other("boom"));
        assert_eq!(exit_code(&e), 3);
    }

    #[test]
    fn daemon_unreachable_hint_mentions_systemctl() {
        let e = CliError::DaemonUnreachable(
            "/run/agentd/agentd.sock".into(),
            "Connection refused".into(),
        );
        let rendered = format_error(&e);
        assert!(
            rendered.contains("systemctl status agentd"),
            "rendered: {}",
            rendered
        );
        assert!(rendered.contains("/run/agentd/agentd.sock"));
        assert!(rendered.contains("journalctl"));
    }

    #[test]
    fn daemon_unreachable_permission_hint_mentions_group() {
        let e = CliError::DaemonUnreachable(
            "/run/agentd/agentd.sock".into(),
            "Permission denied".into(),
        );
        let rendered = format_error(&e);
        let lower = rendered.to_lowercase();
        assert!(lower.contains("adduser"), "rendered: {}", rendered);
        assert!(lower.contains("aaos"));
        assert!(lower.contains("group"));
        // Should NOT mention systemctl in the permission-denied path.
        assert!(!rendered.contains("systemctl"), "rendered: {}", rendered);
    }

    #[test]
    fn usage_error_renders_plain_message() {
        let e = CliError::Usage("agent not found: xyz".into());
        let rendered = format_error(&e);
        assert!(rendered.starts_with("error: agent not found: xyz"));
    }

    #[test]
    fn agent_failed_renders_prefix() {
        let e = CliError::AgentFailed("budget exceeded".into());
        let rendered = format_error(&e);
        assert!(rendered.contains("agent failed: budget exceeded"));
    }

    #[test]
    fn broken_pipe_hint_mentions_restart() {
        let e = CliError::BrokenPipe;
        let rendered = format_error(&e);
        assert!(
            rendered
                .to_lowercase()
                .contains("daemon may have restarted"),
            "rendered: {}",
            rendered
        );
    }

    #[test]
    fn protocol_hint_mentions_version_skew() {
        let e = CliError::Protocol("bad wire format".into());
        let rendered = format_error(&e);
        assert!(rendered.contains("Version skew"), "rendered: {}", rendered);
    }

    #[test]
    fn all_renders_end_with_newline() {
        let samples = vec![
            CliError::Usage("x".into()),
            CliError::DaemonUnreachable("/p".into(), "Connection refused".into()),
            CliError::DaemonUnreachable("/p".into(), "Permission denied".into()),
            CliError::AgentFailed("x".into()),
            CliError::BrokenPipe,
            CliError::Protocol("x".into()),
            CliError::ServerError("x".into()),
        ];
        for e in samples {
            let r = format_error(&e);
            assert!(
                r.ends_with('\n'),
                "missing trailing newline for {:?}: {:?}",
                e,
                r
            );
        }
    }
}
