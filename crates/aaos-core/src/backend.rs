//! Backend abstraction for launching agent processes.
//!
//! `AgentBackend` is the substrate boundary below `AgentServices`. An
//! `AgentServices` implementation doesn't care whether the agent it is
//! serving runs in the same address space (`InProcessBackend`), in a
//! Linux user namespace (`NamespacedBackend`, Phase F), or in a microVM
//! (`MicroVmBackend`, Phase G). All of those are instances of this trait.
//!
//! Commit 1 of `plans/2026-04-15-namespaced-backend-v4.md` introduces the
//! trait and the in-process implementation. Later commits add the
//! isolated variants without touching this file.
//!
//! Design notes (from the plan, round 1 review):
//!
//! - **Opaque handle.** `AgentLaunchHandle` carries backend-owned state
//!   as `Arc<dyn Any + Send + Sync>`. A future `NamespacedBackend` can
//!   stash its child PID, session socket, and Landlock ruleset there
//!   without leaking variants into this crate. Consumers use
//!   `backend_kind` + `agent_id` for identification; only the owning
//!   backend downcasts the state.
//! - **Serializable spec.** `AgentLaunchSpec` carries only data that
//!   crosses process boundaries cleanly. Runtime dependencies
//!   (executor, session store, router, context manager) live on the
//!   backend itself, injected at construction.
//! - **Structured health.** `BackendHealth` distinguishes "exited
//!   cleanly", "signaled", "lost connection", and "unknown" so
//!   supervisors can make informed restart decisions. The in-process
//!   backend maps task completion and abortion to these variants even
//!   though there's no real exit code.

use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agent_id::AgentId;
use crate::budget::BudgetConfig;
use crate::capability::CapabilityHandle;
use crate::error::Result;
use crate::manifest::AgentManifest;

/// Substrate-agnostic interface for launching and supervising agents.
///
/// An `AgentBackend` takes a serializable `AgentLaunchSpec` and produces
/// a running agent, returning an opaque `AgentLaunchHandle` that the
/// caller (typically an `AgentServices` implementation) stores for
/// later lifecycle calls.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Launch a new agent according to the spec.
    ///
    /// Blocks (async) until the agent is running and ready to receive
    /// messages. For isolated backends this includes the sandbox
    /// readiness handshake.
    async fn launch(&self, spec: AgentLaunchSpec) -> Result<AgentLaunchHandle>;

    /// Request the agent shut down.
    ///
    /// Implementations SHOULD be idempotent: calling `stop` twice on
    /// the same handle must not error. If the agent already exited,
    /// `stop` returns `Ok(())`.
    async fn stop(&self, handle: &AgentLaunchHandle) -> Result<()>;

    /// Snapshot the agent's health.
    ///
    /// Cheap to call; no blocking I/O. Intended for supervisors that
    /// poll or react to external events.
    async fn health(&self, handle: &AgentLaunchHandle) -> BackendHealth;
}

/// Serializable launch description.
///
/// Contains everything a backend needs to know *about this specific
/// launch* that can safely cross a process boundary. Runtime
/// dependencies (executor, router, session store) live on the backend
/// and are not part of the spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLaunchSpec {
    /// Stable identity for the launched agent. The backend does not
    /// mint this; the caller (e.g., `AgentRegistry::spawn`) does.
    pub agent_id: AgentId,

    /// Full parsed manifest. Carries model selection, capability
    /// declarations, memory configuration, and lifecycle policy.
    pub manifest: AgentManifest,

    /// Capability handles already issued to this agent. Backends that
    /// cross process boundaries use these to bind a broker session.
    /// In-process backends just forward them to the tool-invocation
    /// path as today.
    pub capability_handles: Vec<CapabilityHandle>,

    /// Path the backend may use for per-agent scratch state. For
    /// isolated backends this becomes a private tmpfs mount inside
    /// the agent's namespace. For `InProcessBackend` it's advisory
    /// (we don't enforce a chroot).
    pub workspace_path: PathBuf,

    /// Optional per-agent token budget. If `None`, no enforcement.
    pub budget_config: Option<BudgetConfig>,
}

/// Opaque handle returned from `AgentBackend::launch`.
///
/// Callers store it and pass it back to `stop` / `health`. The `state`
/// field is intentionally `Arc<dyn Any + Send + Sync>` so each backend
/// can stash whatever it needs (JoinHandle, child PID, socket fd,
/// Landlock ruleset, ...) without leaking those types into this crate.
pub struct AgentLaunchHandle {
    /// Identity of the launched agent.
    pub agent_id: AgentId,

    /// Short backend tag for logging/metrics (e.g. "in_process",
    /// "namespaced", "microvm"). Stable across a single backend
    /// implementation's lifetime; clients SHOULD NOT parse it.
    pub backend_kind: &'static str,

    /// Backend-owned state. Opaque to everything except the backend
    /// that produced the handle.
    state: Arc<dyn Any + Send + Sync>,
}

impl AgentLaunchHandle {
    /// Construct a handle from any `Send + Sync + 'static` state.
    pub fn new<S: Any + Send + Sync>(
        agent_id: AgentId,
        backend_kind: &'static str,
        state: S,
    ) -> Self {
        Self {
            agent_id,
            backend_kind,
            state: Arc::new(state),
        }
    }

    /// Downcast the opaque state to a concrete type. Returns `None`
    /// if the backend's state type does not match `S` (e.g., a
    /// caller from a different backend asking).
    pub fn state<S: Any + Send + Sync>(&self) -> Option<&S> {
        self.state.downcast_ref::<S>()
    }
}

impl std::fmt::Debug for AgentLaunchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLaunchHandle")
            .field("agent_id", &self.agent_id)
            .field("backend_kind", &self.backend_kind)
            .field("state", &"<opaque>")
            .finish()
    }
}

/// Health snapshot for a launched agent.
///
/// `Healthy` means the backend can still see and talk to the agent.
/// The exit variants are populated once the agent's task/process
/// terminates. `Disconnected` is reserved for isolated backends where
/// a broker socket can drop independently of the worker's exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendHealth {
    /// Running, responsive.
    Healthy,
    /// Exited with an OS-style exit code. For `InProcessBackend`,
    /// `Exited(0)` is reported when the task future completes without
    /// panic; cancellation surfaces as `Signaled`.
    Exited(i32),
    /// Terminated by a signal (or, for in-process, an abort).
    Signaled(i32),
    /// The backend lost its IPC channel to the agent. Isolated-only
    /// in practice; in-process backends never use this variant.
    Disconnected,
    /// Backend can't determine health. Message is for operators, not
    /// for machine parsing.
    Unknown(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::AgentManifest;

    fn sample_manifest() -> AgentManifest {
        AgentManifest::from_yaml(
            r#"
name: spec-sample
model: claude-haiku-4-5-20251001
system_prompt: "test"
"#,
        )
        .unwrap()
    }

    #[test]
    fn agent_launch_spec_serializes() {
        let spec = AgentLaunchSpec {
            agent_id: AgentId::new(),
            manifest: sample_manifest(),
            capability_handles: vec![CapabilityHandle::from_raw(17)],
            workspace_path: PathBuf::from("/tmp/aaos/agent-xyz"),
            budget_config: Some(BudgetConfig {
                max_tokens: 5_000,
                reset_period_seconds: 3600,
            }),
        };

        let json = serde_json::to_string(&spec).expect("spec must serialize");
        let round: AgentLaunchSpec = serde_json::from_str(&json).expect("spec must deserialize");

        assert_eq!(round.agent_id, spec.agent_id);
        assert_eq!(round.capability_handles, spec.capability_handles);
        assert_eq!(round.workspace_path, spec.workspace_path);
        assert_eq!(round.budget_config.map(|b| b.max_tokens), Some(5_000));
        assert_eq!(round.manifest.name, "spec-sample");
    }

    #[test]
    fn backend_handle_opaque_state() {
        #[derive(Debug, PartialEq)]
        struct MyState {
            tag: &'static str,
            count: u32,
        }
        struct OtherState;

        let id = AgentId::new();
        let handle = AgentLaunchHandle::new(
            id,
            "test",
            MyState {
                tag: "hello",
                count: 42,
            },
        );

        assert_eq!(handle.agent_id, id);
        assert_eq!(handle.backend_kind, "test");

        let got = handle
            .state::<MyState>()
            .expect("state<MyState> must downcast");
        assert_eq!(got.tag, "hello");
        assert_eq!(got.count, 42);

        // Wrong-type downcast returns None, never panics.
        assert!(handle.state::<OtherState>().is_none());
    }

    #[test]
    fn backend_health_variants_are_distinct() {
        assert_ne!(BackendHealth::Healthy, BackendHealth::Exited(0));
        assert_ne!(BackendHealth::Exited(0), BackendHealth::Signaled(0));
        assert_ne!(BackendHealth::Healthy, BackendHealth::Disconnected);
        assert_ne!(
            BackendHealth::Unknown("a".into()),
            BackendHealth::Unknown("b".into())
        );
    }
}
