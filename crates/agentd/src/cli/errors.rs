//! CLI error types. Extended by Task 6 with hint rendering + exit-code mapping.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("daemon not reachable at {0}: {1}")]
    DaemonUnreachable(String, String),
    #[error("server error: {0}")]
    ServerError(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("broken pipe")]
    BrokenPipe,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
