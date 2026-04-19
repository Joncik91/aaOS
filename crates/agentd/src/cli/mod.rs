//! Operator CLI surface for `agentd`.
//!
//! Subcommand implementations live in sibling files (`submit.rs`, `list.rs`,
//! etc.). This module defines the clap enum and re-exports each subcommand's
//! `run` entry point.

pub mod client;
pub mod errors;
pub mod output;
pub mod prefix;

use std::path::PathBuf;

#[derive(clap::Subcommand, Debug)]
pub enum CliCommand {
    /// Start the agent daemon.
    Run {
        /// Path to the daemon configuration file.
        #[arg(short, long, default_value = "/etc/agentd/config.yaml")]
        config: PathBuf,
        /// Unix socket path to listen on.
        #[arg(short, long, default_value = "/run/agentd/agentd.sock")]
        socket: PathBuf,
    },
    /// Send a goal to Bootstrap and stream the result.
    #[command(
        long_about = "Send a goal to Bootstrap, stream audit events live, exit when the goal completes.\n\nExample:\n    agentd submit \"fetch HN top 5 stories\""
    )]
    Submit {
        /// The goal text for Bootstrap.
        goal: String,
        /// Show every audit event (default: operator view only).
        #[arg(short, long)]
        verbose: bool,
        /// Unix socket path to connect to.
        #[arg(long, default_value = "/run/agentd/agentd.sock")]
        socket: PathBuf,
    },
    /// List running agents.
    List {
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = "/run/agentd/agentd.sock")]
        socket: PathBuf,
    },
    /// Show detail for one agent. AGENT_ID may be a unique prefix.
    Status {
        /// Agent id or any unique prefix.
        agent_id: String,
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = "/run/agentd/agentd.sock")]
        socket: PathBuf,
    },
    /// Terminate a running agent. AGENT_ID may be a unique prefix.
    Stop {
        /// Agent id or any unique prefix.
        agent_id: String,
        #[arg(long, default_value = "/run/agentd/agentd.sock")]
        socket: PathBuf,
    },
    /// Attach to a running agent's audit stream. Ctrl-C detaches (the agent
    /// keeps running).
    Logs {
        /// Agent id or any unique prefix.
        agent_id: String,
        /// Show every audit event (default: operator view only).
        #[arg(short, long)]
        verbose: bool,
        #[arg(long, default_value = "/run/agentd/agentd.sock")]
        socket: PathBuf,
    },
    /// Inspect the role catalog at /etc/aaos/roles/.
    Roles {
        #[command(subcommand)]
        subcommand: RolesCommand,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum RolesCommand {
    /// List all loaded roles with their parameter summaries.
    List {
        #[arg(long, default_value = "/etc/aaos/roles")]
        dir: std::path::PathBuf,
    },
    /// Print a role's full YAML.
    Show {
        name: String,
        #[arg(long, default_value = "/etc/aaos/roles")]
        dir: std::path::PathBuf,
    },
    /// Validate a single role YAML file without installing it.
    Validate { path: std::path::PathBuf },
}

// ---- Stub subcommand runners. Real implementations land in Tasks 9-13. ----

pub mod submit;

pub mod list;

pub mod status;

pub mod stop;

pub mod logs;

pub mod roles;

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        cmd: CliCommand,
    }

    #[test]
    fn submit_parses_with_goal() {
        let c = TestCli::parse_from(["agentd", "submit", "hello world"]);
        match c.cmd {
            CliCommand::Submit {
                goal,
                verbose,
                socket,
            } => {
                assert_eq!(goal, "hello world");
                assert!(!verbose);
                assert_eq!(socket.to_str().unwrap(), "/run/agentd/agentd.sock");
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn submit_accepts_verbose_flag() {
        let c = TestCli::parse_from(["agentd", "submit", "-v", "hi"]);
        match c.cmd {
            CliCommand::Submit { verbose, .. } => assert!(verbose),
            _ => panic!(),
        }
    }

    #[test]
    fn list_accepts_json_flag() {
        let c = TestCli::parse_from(["agentd", "list", "--json"]);
        match c.cmd {
            CliCommand::List { json, .. } => assert!(json),
            _ => panic!(),
        }
    }

    #[test]
    fn status_requires_agent_id() {
        let result = TestCli::try_parse_from(["agentd", "status"]);
        assert!(result.is_err(), "expected parse error for missing agent_id");
    }
}
