use std::path::PathBuf;

use clap::Parser;

pub use crate::cli::CliCommand as Command;

/// aaOS Agent Daemon — manages agent lifecycles, IPC, and tool invocation.
#[derive(Parser, Debug)]
#[command(name = "agentd", version, about = "aaOS agent daemon and operator CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Runtime configuration for the daemon.
#[derive(Debug)]
#[allow(dead_code)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub log_level: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/run/agentd/agentd.sock"),
            log_level: "info".to_string(),
        }
    }
}
