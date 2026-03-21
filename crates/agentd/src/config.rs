use std::path::PathBuf;

use clap::Parser;

/// aaOS Agent Daemon — manages agent lifecycles, IPC, and tool invocation.
#[derive(Parser, Debug)]
#[command(name = "agentd", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Start the agent daemon
    Run {
        /// Path to the daemon configuration file
        #[arg(short, long, default_value = "/etc/agentd/config.yaml")]
        config: PathBuf,
        /// Unix socket path for the API
        #[arg(short, long, default_value = "/var/run/agentd.sock")]
        socket: PathBuf,
    },
    /// Spawn an agent from a manifest file
    Spawn {
        /// Path to the agent manifest YAML file
        manifest: PathBuf,
        /// Unix socket path for the daemon
        #[arg(short, long, default_value = "/var/run/agentd.sock")]
        socket: PathBuf,
    },
    /// List running agents
    List {
        /// Unix socket path for the daemon
        #[arg(short, long, default_value = "/var/run/agentd.sock")]
        socket: PathBuf,
    },
    /// Get status of a specific agent
    Status {
        /// Agent ID
        agent_id: String,
        /// Unix socket path for the daemon
        #[arg(short, long, default_value = "/var/run/agentd.sock")]
        socket: PathBuf,
    },
    /// Stop an agent
    Stop {
        /// Agent ID
        agent_id: String,
        /// Unix socket path for the daemon
        #[arg(short, long, default_value = "/var/run/agentd.sock")]
        socket: PathBuf,
    },
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
            socket_path: PathBuf::from("/var/run/agentd.sock"),
            log_level: "info".to_string(),
        }
    }
}
