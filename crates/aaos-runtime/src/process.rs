use std::fmt;

use aaos_core::{AgentId, AgentManifest, CapabilityToken};
use tokio::sync::mpsc;

/// The state of an agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Starting,
    Running,
    Paused,
    Stopping,
    Stopped,
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Running => write!(f, "running"),
            Self::Paused => write!(f, "paused"),
            Self::Stopping => write!(f, "stopping"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

impl AgentState {
    /// Check if transitioning to the target state is valid.
    pub fn can_transition_to(&self, target: AgentState) -> bool {
        matches!(
            (self, target),
            (Self::Starting, Self::Running)
                | (Self::Starting, Self::Stopped) // failed to start
                | (Self::Running, Self::Paused)
                | (Self::Running, Self::Stopping)
                | (Self::Paused, Self::Running)
                | (Self::Paused, Self::Stopping)
                | (Self::Stopping, Self::Stopped)
        )
    }
}

/// Command sent to an agent process via its control channel.
#[derive(Debug)]
pub enum AgentCommand {
    Pause,
    Resume,
    Stop,
}

/// A running agent process managed by the runtime.
pub struct AgentProcess {
    pub id: AgentId,
    pub manifest: AgentManifest,
    pub state: AgentState,
    pub capabilities: Vec<CapabilityToken>,
    pub command_tx: mpsc::Sender<AgentCommand>,
    command_rx: Option<mpsc::Receiver<AgentCommand>>,
    pub message_rx: Option<tokio::sync::mpsc::Receiver<aaos_ipc::McpMessage>>,
    pub response_rx: Option<tokio::sync::mpsc::Receiver<aaos_ipc::McpResponse>>,
    pub task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl AgentProcess {
    /// Create a new agent process in the Starting state.
    pub fn new(id: AgentId, manifest: AgentManifest, capabilities: Vec<CapabilityToken>) -> Self {
        let (command_tx, command_rx) = mpsc::channel(32);
        Self {
            id,
            manifest,
            state: AgentState::Starting,
            capabilities,
            command_tx,
            command_rx: Some(command_rx),
            message_rx: None,
            response_rx: None,
            task_handle: None,
        }
    }

    /// Take the command receiver (used once when starting the agent task).
    pub fn take_command_rx(&mut self) -> Option<mpsc::Receiver<AgentCommand>> {
        self.command_rx.take()
    }

    /// Transition to a new state if the transition is valid.
    pub fn transition_to(&mut self, target: AgentState) -> aaos_core::Result<()> {
        if self.state.can_transition_to(target) {
            tracing::info!(
                agent_id = %self.id,
                from = %self.state,
                to = %target,
                "agent state transition"
            );
            self.state = target;
            Ok(())
        } else {
            Err(aaos_core::CoreError::InvalidStateTransition {
                from: self.state.to_string(),
                to: target.to_string(),
            })
        }
    }

    /// Check if the agent holds a token that permits the requested capability.
    pub fn has_capability(&self, requested: &aaos_core::Capability) -> bool {
        self.capabilities
            .iter()
            .any(|token| token.permits(requested))
    }

    /// Get a summary of this agent for API responses.
    pub fn info(&self) -> AgentInfo {
        AgentInfo {
            id: self.id,
            name: self.manifest.name.clone(),
            model: self.manifest.model.clone(),
            state: self.state,
            capability_count: self.capabilities.len(),
        }
    }
}

/// Summary information about an agent, suitable for API responses.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub id: AgentId,
    pub name: String,
    pub model: String,
    pub state: AgentState,
    pub capability_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{Capability, Constraints};

    fn test_manifest() -> AgentManifest {
        AgentManifest::from_yaml(
            r#"
name: test-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
"#,
        )
        .unwrap()
    }

    #[test]
    fn valid_state_transitions() {
        assert!(AgentState::Starting.can_transition_to(AgentState::Running));
        assert!(AgentState::Running.can_transition_to(AgentState::Paused));
        assert!(AgentState::Running.can_transition_to(AgentState::Stopping));
        assert!(AgentState::Stopping.can_transition_to(AgentState::Stopped));
    }

    #[test]
    fn invalid_state_transitions() {
        assert!(!AgentState::Stopped.can_transition_to(AgentState::Running));
        assert!(!AgentState::Starting.can_transition_to(AgentState::Paused));
    }

    #[test]
    fn agent_process_lifecycle() {
        let id = AgentId::new();
        let mut process = AgentProcess::new(id, test_manifest(), vec![]);

        assert_eq!(process.state, AgentState::Starting);
        process.transition_to(AgentState::Running).unwrap();
        assert_eq!(process.state, AgentState::Running);
        process.transition_to(AgentState::Stopping).unwrap();
        process.transition_to(AgentState::Stopped).unwrap();
        assert_eq!(process.state, AgentState::Stopped);
    }

    #[test]
    fn invalid_transition_returns_error() {
        let id = AgentId::new();
        let mut process = AgentProcess::new(id, test_manifest(), vec![]);
        let result = process.transition_to(AgentState::Stopped);
        // Starting -> Stopped is valid (failed to start)
        assert!(result.is_ok());
    }

    #[test]
    fn capability_check() {
        let id = AgentId::new();
        let token = CapabilityToken::issue(id, Capability::WebSearch, Constraints::default());
        let process = AgentProcess::new(id, test_manifest(), vec![token]);

        assert!(process.has_capability(&Capability::WebSearch));
        assert!(!process.has_capability(&Capability::FileRead {
            path_glob: "/tmp/*".into()
        }));
    }
}
