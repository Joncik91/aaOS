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
        long_about = "Send a goal to Bootstrap, stream audit events live, exit when the goal completes.\n\nExample:\n    agentd submit \"fetch HN top 5 stories\"\n    agentd submit --orchestration persistent \"read the codebase and find bugs\""
    )]
    Submit {
        /// The goal text for Bootstrap.
        goal: String,
        /// Show every audit event (default: operator view only).
        #[arg(short, long)]
        verbose: bool,
        /// Orchestration mode: `plan` routes through the Planner + PlanExecutor
        /// DAG; `persistent` routes to the Bootstrap persistent agent.
        /// Defaults to `plan`. Use `persistent` for open-ended, exploratory,
        /// or long-context goals.
        #[arg(long, value_enum, default_value = "plan")]
        orchestration: crate::orchestration::OrchestrationMode,
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
    /// First-boot setup: prompt for an LLM API key, write /etc/default/aaos
    /// with mode 0600 root:root, restart the daemon.
    ///
    /// Run after `apt install ./aaos_*.deb` as root (e.g. via sudo) — the
    /// command seeds the env file that systemd reads before de-privileging
    /// to User=aaos, so the file must land at /etc/default/aaos with the
    /// key readable only by root.
    #[command(
        long_about = "Interactive first-boot setup. Prompts for a DeepSeek or Anthropic API key, writes /etc/default/aaos mode 0600, and optionally restarts agentd.\n\nExample:\n    sudo agentd configure"
    )]
    Configure {
        /// Provider to configure. Default is `deepseek` (cheapest; aaOS's
        /// first-choice default).
        #[arg(long, value_parser = ["deepseek", "anthropic"], default_value = "deepseek")]
        provider: String,
        /// Read the API key from the named env var instead of prompting.
        /// Useful for non-interactive provisioning (Ansible, cloud-init).
        #[arg(long)]
        key_from_env: Option<String>,
        /// Path to the env file. Defaults to /etc/default/aaos (what
        /// packaging/agentd.service reads via EnvironmentFile=).
        #[arg(long, default_value = "/etc/default/aaos")]
        env_file: PathBuf,
        /// Skip the systemctl restart at the end — useful on non-systemd
        /// hosts (CI, containers) or when editing before the daemon runs.
        #[arg(long)]
        no_restart: bool,
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

pub mod configure;

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
                ..
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

    // ---- orchestration flag tests ----

    #[test]
    fn submit_orchestration_defaults_to_plan() {
        let c = TestCli::parse_from(["agentd", "submit", "some goal"]);
        match c.cmd {
            CliCommand::Submit { orchestration, .. } => {
                assert_eq!(
                    orchestration,
                    crate::orchestration::OrchestrationMode::Plan,
                    "default orchestration must be Plan"
                );
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn submit_orchestration_plan_explicit() {
        let c = TestCli::parse_from(["agentd", "submit", "--orchestration", "plan", "goal"]);
        match c.cmd {
            CliCommand::Submit { orchestration, .. } => {
                assert_eq!(orchestration, crate::orchestration::OrchestrationMode::Plan);
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn submit_orchestration_persistent() {
        let c = TestCli::parse_from(["agentd", "submit", "--orchestration", "persistent", "goal"]);
        match c.cmd {
            CliCommand::Submit { orchestration, .. } => {
                assert_eq!(
                    orchestration,
                    crate::orchestration::OrchestrationMode::Persistent
                );
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn submit_orchestration_invalid_value_produces_error() {
        let result =
            TestCli::try_parse_from(["agentd", "submit", "--orchestration", "garbage", "goal"]);
        assert!(
            result.is_err(),
            "--orchestration garbage must produce a clap error"
        );
    }
}
