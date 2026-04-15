pub mod backend_in_process;
pub mod context;
pub mod handoff;
pub mod persistent;
pub mod process;
pub mod registry;
pub mod scheduler;
pub mod services;
pub mod session;
pub mod supervisor;

pub use backend_in_process::{InProcessBackend, InProcessBackendConfig};
pub use context::ContextManager;
pub use handoff::{wrap_initial_message, HandoffContext, MAX_PRIOR_FINDINGS_BYTES};
pub use process::{AgentCommand, AgentInfo, AgentProcess, AgentState};
pub use registry::AgentRegistry;
pub use scheduler::{Priority, RoundRobinScheduler, ScheduleEntry, Scheduler};
pub use services::InProcessAgentServices;
pub use session::{ArchiveSegment, InMemorySessionStore, JsonlSessionStore, SessionStore};
pub use persistent::persistent_agent_loop;
pub use supervisor::{RestartPolicy, SupervisedAgent, SupervisorConfig};
// Re-export from aaos-core for convenience
pub use aaos_core::CapabilityRegistry;
