use serde::{Deserialize, Serialize};

use aaos_core::AgentId;

/// Policy for how the supervisor should handle agent failures.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    #[default]
    OnFailure,
    Never,
}

/// Configuration for the supervisor.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub restart_policy: RestartPolicy,
    pub max_restarts: u32,
    pub restart_backoff_ms: u64,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            restart_policy: RestartPolicy::OnFailure,
            max_restarts: 3,
            restart_backoff_ms: 1000,
        }
    }
}

/// Tracks restart state for a supervised agent.
#[derive(Debug)]
pub struct SupervisedAgent {
    pub agent_id: AgentId,
    pub config: SupervisorConfig,
    pub restart_count: u32,
}

impl SupervisedAgent {
    pub fn new(agent_id: AgentId, config: SupervisorConfig) -> Self {
        Self {
            agent_id,
            config,
            restart_count: 0,
        }
    }

    /// Determine if the agent should be restarted after a failure.
    pub fn should_restart(&self, was_error: bool) -> bool {
        if self.restart_count >= self.config.max_restarts {
            return false;
        }
        match self.config.restart_policy {
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => was_error,
            RestartPolicy::Never => false,
        }
    }

    /// Record a restart.
    pub fn record_restart(&mut self) {
        self.restart_count += 1;
    }

    /// Calculate backoff delay for the next restart.
    pub fn backoff_ms(&self) -> u64 {
        self.config.restart_backoff_ms * 2u64.pow(self.restart_count.min(5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_failure_restarts_on_error() {
        let agent = SupervisedAgent::new(AgentId::new(), SupervisorConfig::default());
        assert!(agent.should_restart(true));
        assert!(!agent.should_restart(false));
    }

    #[test]
    fn always_restarts_regardless() {
        let agent = SupervisedAgent::new(
            AgentId::new(),
            SupervisorConfig {
                restart_policy: RestartPolicy::Always,
                ..Default::default()
            },
        );
        assert!(agent.should_restart(true));
        assert!(agent.should_restart(false));
    }

    #[test]
    fn never_never_restarts() {
        let agent = SupervisedAgent::new(
            AgentId::new(),
            SupervisorConfig {
                restart_policy: RestartPolicy::Never,
                ..Default::default()
            },
        );
        assert!(!agent.should_restart(true));
    }

    #[test]
    fn max_restarts_enforced() {
        let mut agent = SupervisedAgent::new(
            AgentId::new(),
            SupervisorConfig {
                restart_policy: RestartPolicy::Always,
                max_restarts: 2,
                ..Default::default()
            },
        );
        agent.record_restart();
        agent.record_restart();
        assert!(!agent.should_restart(true));
    }

    #[test]
    fn exponential_backoff() {
        let mut agent = SupervisedAgent::new(
            AgentId::new(),
            SupervisorConfig {
                restart_backoff_ms: 100,
                ..Default::default()
            },
        );
        assert_eq!(agent.backoff_ms(), 100); // 100 * 2^0
        agent.record_restart();
        assert_eq!(agent.backoff_ms(), 200); // 100 * 2^1
        agent.record_restart();
        assert_eq!(agent.backoff_ms(), 400); // 100 * 2^2
    }
}
