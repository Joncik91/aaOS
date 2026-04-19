use std::path::{Path, PathBuf};
use std::sync::Arc;

use aaos_core::{
    AgentBackend, AgentLaunchSpec, AgentManifest, AgentServices, ApprovalService, AuditLog,
    InMemoryAuditLog,
};
use aaos_ipc::{MessageRouter, SchemaValidator};
use aaos_llm::{AgentExecutor, ExecutorConfig, LlmClient};
use aaos_runtime::plan::{
    PlanExecutor, Planner, RoleCatalog, SubtaskExecutorOverrides, SubtaskResult, SubtaskRunner,
};
use aaos_runtime::{AgentRegistry, AgentState, InProcessAgentServices, InProcessBackend};
use aaos_tools::{EchoTool, ToolInvocation, ToolRegistry};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::api::{JsonRpcResponse, INTERNAL_ERROR, METHOD_NOT_FOUND};
use crate::broadcast_audit::BroadcastAuditLog;

/// Methods dispatched through the streaming path (connection stays open for
/// multi-frame NDJSON responses instead of a single JSON-RPC reply).
const STREAMING_METHODS: &[&str] = &["agent.submit_streaming", "agent.logs_streaming"];

/// The core daemon server holding all subsystems.
#[allow(dead_code)]
pub struct Server {
    pub registry: Arc<AgentRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub tool_invocation: Arc<ToolInvocation>,
    pub router: Arc<MessageRouter>,
    pub validator: Arc<SchemaValidator>,
    pub audit_log: Arc<dyn AuditLog>,
    /// Concrete handle to the broadcast fan-out sink underlying `audit_log`.
    /// Streaming JSON-RPC handlers call `.subscribe()` on this to receive
    /// live audit events. `audit_log` above is the same object as a
    /// trait-object alias.
    pub broadcast_audit: Arc<BroadcastAuditLog>,
    /// Optional computed-orchestration executor. Installed lazily by
    /// `install_plan_executor_runner` after the Server Arc exists — before
    /// that point the OnceLock is empty and any attempt to execute a plan
    /// falls through to the legacy Bootstrap path. Present when the role
    /// catalog at /etc/aaos/roles/ (or AAOS_ROLES_DIR) loaded cleanly
    /// at server construction.
    pub plan_executor: std::sync::OnceLock<Arc<PlanExecutor>>,
    /// Role catalog loaded at construction time. Held here so
    /// `install_plan_executor_runner` can rebuild a PlanExecutor with
    /// the real runner closure (without needing to re-read the roles
    /// directory or expose PlanExecutor internals).
    pub role_catalog: Option<Arc<RoleCatalog>>,
    /// Planner loaded at construction time. See `role_catalog`.
    pub planner: Option<Arc<Planner>>,
    /// Workspace base path used by PlanExecutor for per-run scratch dirs.
    pub run_root_base: PathBuf,
    pub approval_queue: Arc<crate::approval::ApprovalQueue>,
    pub llm_client: Option<Arc<dyn LlmClient>>,
    pub session_store: Arc<dyn aaos_runtime::SessionStore>,
    pub memory_store: Arc<dyn aaos_memory::MemoryStore>,
    pub embedding_source: Arc<dyn aaos_memory::EmbeddingSource>,
    pub skill_registry: Arc<aaos_tools::SkillRegistry>,
    /// Substrate that actually launches agent processes. Today always
    /// `InProcessBackend`; when `AAOS_DEFAULT_BACKEND=namespaced` and
    /// the `namespaced-agents` feature is compiled in, this is a
    /// `NamespacedBackend` behind the same trait.
    pub backend: Arc<dyn AgentBackend>,
    /// Concrete `NamespacedBackend` arc — present only when the namespaced
    /// backend was successfully activated. Held so `BrokerWorkerHandle`
    /// can call `.session(&agent_id)` without downcasting the trait object.
    #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
    pub(crate) namespaced: Option<Arc<aaos_backend_linux::NamespacedBackend>>,
    /// Reasoning-slot scheduler. Awards inference slots with TTL-aware
    /// priority. One per server. Replaces the role that
    /// `aaos_llm::ScheduledLlmClient` played before Phase F-b — that
    /// client remains in-tree for code paths that don't go through
    /// `run_subtask_inline`, but production plan-executor traffic is
    /// now scheduler-gated.
    pub(crate) reasoning_scheduler: Arc<aaos_runtime::scheduler::ReasoningScheduler>,
    /// Per-subtask wall-clock tracker. Queried by the TTL watcher; Gap 2
    /// will add a per-model variant implementing the same trait.
    pub(crate) latency_tracker: Arc<dyn aaos_runtime::LatencyTracker>,
    /// Per-model latency tracker. Fed observability-only samples via
    /// `CompositeLatencyTracker` alongside `latency_tracker`. Used by
    /// the router/introspection path; not consulted for TTL.
    pub(crate) per_model_latency: Arc<aaos_runtime::scheduler::PerModelLatencyTracker>,
}

/// Return type of `maybe_swap_for_namespaced` / `build_in_process_backend`.
/// Bundles the trait-object backend with an optional concrete
/// `NamespacedBackend` arc so callers can construct `BrokerWorkerHandle`
/// without needing to downcast.
struct SelectedBackend {
    backend: Arc<dyn AgentBackend>,
    /// Present only when the namespaced backend was activated; absent on
    /// in-process path or non-linux / feature-off builds.
    #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
    namespaced: Option<Arc<aaos_backend_linux::NamespacedBackend>>,
}

/// Adapter that implements `aaos_tools::WorkerHandle` by delegating to
/// a `BrokerSession` looked up from the `NamespacedBackend`. One instance
/// per `ToolInvocation`; agent sessions are resolved on each call.
#[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
struct BrokerWorkerHandle {
    backend: Arc<aaos_backend_linux::NamespacedBackend>,
}

#[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
#[async_trait::async_trait]
impl aaos_tools::WorkerHandle for BrokerWorkerHandle {
    fn backend_kind(&self) -> &'static str {
        "namespaced"
    }

    async fn invoke_over_worker(
        &self,
        agent_id: aaos_core::AgentId,
        tool_name: &str,
        input: serde_json::Value,
        tokens: Vec<aaos_core::CapabilityToken>,
    ) -> std::result::Result<serde_json::Value, aaos_tools::WorkerInvokeError> {
        let session = self
            .backend
            .session(&agent_id)
            .ok_or(aaos_tools::WorkerInvokeError::NoSession)?;
        session
            .invoke_over_worker(tool_name, input, tokens)
            .await
            .map_err(|e| aaos_tools::WorkerInvokeError::Transport(e.to_string()))
    }
}

impl Server {
    /// Build an `InProcessBackend` from the pieces the server already
    /// assembled. Centralizes the `services_builder` closure so the
    /// three server constructors don't each re-derive it. Every
    /// agent launched through this backend sees an
    /// `InProcessAgentServices` wired with the same subsystems as
    /// today's inline construction, plus a self-referential handle to
    /// the backend (so future `spawn_agent` calls on the trait can
    /// delegate without additional wiring).
    fn build_in_process_backend(
        registry: Arc<AgentRegistry>,
        session_store: Arc<dyn aaos_runtime::SessionStore>,
        router: Arc<MessageRouter>,
        audit_log: Arc<dyn AuditLog>,
        tool_invocation: Arc<ToolInvocation>,
        tool_registry: Arc<ToolRegistry>,
        approval_queue: Arc<crate::approval::ApprovalQueue>,
        llm_client: Option<Arc<dyn LlmClient>>,
    ) -> SelectedBackend {
        let registry_b = registry.clone();
        let tool_invocation_b = tool_invocation.clone();
        let tool_registry_b = tool_registry.clone();
        let audit_log_b = audit_log.clone();
        let router_b = router.clone();
        let approval_b = approval_queue.clone();

        let services_builder: Arc<dyn Fn() -> Arc<dyn AgentServices> + Send + Sync> =
            Arc::new(move || {
                Arc::new(InProcessAgentServices::new(
                    registry_b.clone(),
                    tool_invocation_b.clone(),
                    tool_registry_b.clone(),
                    audit_log_b.clone(),
                    router_b.clone(),
                    approval_b.clone() as Arc<dyn ApprovalService>,
                )) as Arc<dyn AgentServices>
            });

        let in_process: Arc<dyn AgentBackend> = Arc::new(InProcessBackend::new(
            registry,
            session_store,
            router,
            audit_log,
            llm_client,
            services_builder,
        ));

        Self::maybe_swap_for_namespaced(in_process)
    }

    /// If the `namespaced-agents` feature was compiled in AND the
    /// runtime env `AAOS_DEFAULT_BACKEND=namespaced` is set, swap the
    /// `InProcessBackend` for a `NamespacedBackend`. Otherwise, pass
    /// through.
    ///
    /// Policy for commit 2 of
    /// `plans/2026-04-15-namespaced-backend-v4.md`:
    /// - Default build: namespaced code not compiled. `InProcessBackend`.
    /// - `cargo build --features namespaced-agents` + env unset:
    ///   `InProcessBackend` (feature compiled but not active — useful
    ///   for building the .deb without mandating namespaced by
    ///   default).
    /// - `cargo build --features namespaced-agents` +
    ///   `AAOS_DEFAULT_BACKEND=namespaced`: `NamespacedBackend`. If
    ///   construction fails (e.g. Landlock unavailable), we fall back
    ///   to `InProcessBackend` **with a loud warning log** — the plan
    ///   calls for fail-closed at the `NamespacedBackend::new` level,
    ///   which we respect here; the env-var selector is operator
    ///   intent, not policy.
    #[allow(unused_variables, unused_mut)]
    fn maybe_swap_for_namespaced(in_process: Arc<dyn AgentBackend>) -> SelectedBackend {
        #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
        {
            let requested = std::env::var("AAOS_DEFAULT_BACKEND")
                .map(|s| s.eq_ignore_ascii_case("namespaced"))
                .unwrap_or(false);
            if !requested {
                return SelectedBackend {
                    backend: in_process,
                    namespaced: None,
                };
            }
            match aaos_backend_linux::NamespacedBackend::new(
                aaos_backend_linux::NamespacedBackendConfig::default(),
            ) {
                Ok(backend) => {
                    tracing::info!(
                        "agentd: AAOS_DEFAULT_BACKEND=namespaced — using NamespacedBackend"
                    );
                    let nb = Arc::new(backend);
                    return SelectedBackend {
                        backend: nb.clone() as Arc<dyn AgentBackend>,
                        namespaced: Some(nb),
                    };
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "AAOS_DEFAULT_BACKEND=namespaced requested but \
                         NamespacedBackend::new failed; falling back to \
                         InProcessBackend"
                    );
                    return SelectedBackend {
                        backend: in_process,
                        namespaced: None,
                    };
                }
            }
        }
        #[cfg(not(all(target_os = "linux", feature = "namespaced-agents")))]
        {
            SelectedBackend {
                backend: in_process,
            }
        }
    }

    /// Load the role catalog at /etc/aaos/roles/ (or AAOS_ROLES_DIR if
    /// set) and build a Planner. Returns (None, None) if the catalog
    /// can't be loaded — the daemon continues with the legacy Bootstrap
    /// path in that case.
    ///
    /// The actual PlanExecutor is built later by
    /// `install_plan_executor_runner`, which has access to the Server Arc
    /// and can wire a real SubtaskRunner + the real broadcast_audit sink.
    fn load_role_catalog(
        client: &Arc<dyn aaos_llm::LlmClient>,
    ) -> (Option<Arc<RoleCatalog>>, Option<Arc<Planner>>) {
        let roles_dir = std::path::PathBuf::from(
            std::env::var("AAOS_ROLES_DIR").unwrap_or_else(|_| "/etc/aaos/roles".into()),
        );
        let catalog = match RoleCatalog::load_from_dir(&roles_dir) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    dir = %roles_dir.display(),
                    "role catalog unavailable; computed orchestration disabled \
                     (falling back to bootstrap manifest)"
                );
                return (None, None);
            }
        };
        let catalog = Arc::new(catalog);
        let planner = Arc::new(Planner::new(client.clone(), "deepseek-chat".into()));
        (Some(catalog), Some(planner))
    }

    /// Default workspace base used by PlanExecutor for per-run scratch
    /// dirs. Read from `AAOS_WORKSPACE_BASE` if set, else
    /// `/var/lib/aaos/workspace`.
    fn default_run_root_base() -> PathBuf {
        std::path::PathBuf::from(
            std::env::var("AAOS_WORKSPACE_BASE")
                .unwrap_or_else(|_| "/var/lib/aaos/workspace".into()),
        )
    }

    /// Rebuild `plan_executor` with a real SubtaskRunner that closes over
    /// `self` (the Arc<Server>) and forwards to `run_subtask_inline`.
    /// Also swaps in the Server's real broadcast_audit so subtask audit
    /// events stream to connected CLIs like any other event.
    ///
    /// Called by the LLM-aware constructors after wrapping the partially-
    /// built Server in an Arc. No-op if the role catalog didn't load
    /// (computed-orchestration disabled).
    pub fn install_plan_executor_runner(self: &Arc<Self>) {
        let (Some(catalog), Some(planner)) = (self.role_catalog.clone(), self.planner.clone())
        else {
            return;
        };
        let server_weak = self.clone();
        let runner: SubtaskRunner = Arc::new(
            move |subtask_id, manifest_yaml, message, overrides, deadline, run_root| {
                let s = server_weak.clone();
                Box::pin(async move {
                    s.run_subtask_inline(
                        &subtask_id,
                        &manifest_yaml,
                        &message,
                        overrides,
                        deadline,
                        run_root,
                    )
                    .await
                })
            },
        );

        // Build the scaffold runner — deterministic runtime-side execution
        // for roles with `scaffold: {kind: ...}`. Handles "fetcher" today;
        // other kinds can be added without changing PlanExecutor.
        let server_weak2 = self.clone();
        let scaffold_runner: aaos_runtime::plan::ScaffoldRunner =
            Arc::new(move |subtask_id, kind, params| {
                let s = server_weak2.clone();
                Box::pin(async move { s.run_scaffold_inline(&subtask_id, &kind, params).await })
            });

        let audit: Arc<dyn aaos_core::AuditLog> = self.broadcast_audit.clone();
        let mut executor =
            PlanExecutor::new(catalog, planner, runner, audit, self.run_root_base.clone());
        executor.set_scaffold_runner(scaffold_runner);
        // Best-effort: install once. If install_plan_executor_runner is
        // called twice on the same Arc (shouldn't happen today), the
        // second call silently no-ops — the first wins.
        let _ = self.plan_executor.set(Arc::new(executor));
    }

    /// Run a scaffold-marked role deterministically — no LLM loop. Spawns
    /// an ephemeral child with the role's capability set (so `tool_invocation`
    /// sees a real agent context), then dispatches on `kind`:
    ///
    ///   * "fetcher" — call web_fetch(url=<params.url>), then
    ///     file_write(path=<params.workspace>, content=<body>), return
    ///     workspace as the response text.
    ///
    /// Other kinds return Err. Audit events (ToolInvoked, ToolResult)
    /// flow through the normal capability-checked path so the CLI sees
    /// the same event stream shape it would for an LLM-powered role.
    #[doc(hidden)]
    pub async fn run_scaffold_inline(
        self: &Arc<Self>,
        subtask_id: &str,
        kind: &str,
        params: serde_json::Value,
    ) -> Result<SubtaskResult, aaos_core::CoreError> {
        let catalog = self.role_catalog.as_ref().ok_or_else(|| {
            aaos_core::CoreError::Ipc("scaffold requires role catalog (none loaded)".into())
        })?;

        // Find the role whose scaffold.kind matches. The scaffold runner
        // is keyed on kind rather than role name so multiple roles could
        // share one scaffold implementation (e.g. different fetcher
        // presets pointing at the same "fetcher" kind).
        let role = catalog
            .names()
            .iter()
            .filter_map(|n| catalog.get(n))
            .find(|r| r.scaffold.as_ref().map(|s| s.kind == kind).unwrap_or(false))
            .ok_or_else(|| {
                aaos_core::CoreError::Ipc(format!(
                    "no role in catalog declares scaffold kind '{kind}'"
                ))
            })?;

        // Render the manifest so the ephemeral child has the right
        // capabilities (e.g. file_write: {workspace} substituted).
        let manifest_yaml = role.render_manifest(&params);
        let manifest = aaos_core::AgentManifest::from_yaml(&manifest_yaml).map_err(|e| {
            aaos_core::CoreError::Ipc(format!("scaffold: parse rendered manifest: {e}"))
        })?;

        // Spawn the child — same pattern as run_subtask_inline. Scopeguard
        // ensures cleanup on any return path.
        //
        // Intentionally NOT launching a NamespacedBackend worker for
        // scaffolds — today's workers have no visibility into
        // /var/lib/aaos/workspace/ (the mount namespace only exposes
        // /scratch + shared_libs per PolicyDescription). Scaffolds are
        // the workflow's plumbing (fetch → write to workspace →
        // hand-off), so running them daemon-side keeps files flowing
        // into the shared workspace. Confining scaffolds requires a
        // design round on workspace bind-mounts — tracked for a
        // follow-up sub-project.
        let agent_id = self
            .registry
            .spawn(manifest.clone())
            .map_err(|e| aaos_core::CoreError::Ipc(format!("scaffold spawn: {e}")))?;
        let cleanup_registry = self.registry.clone();
        let _guard = scopeguard::guard(agent_id, move |aid| {
            let _ = cleanup_registry.stop_sync(aid);
        });

        // Fetch the agent's capability handles — the tool_invocation
        // path enforces capabilities via these.
        let token_handles = self
            .registry
            .get_token_handles(agent_id)
            .map_err(|e| aaos_core::CoreError::Ipc(format!("scaffold: get token handles: {e}")))?;

        // Dispatch on kind.
        let response = match kind {
            "fetcher" => {
                self.scaffold_fetcher(agent_id, &token_handles, &params)
                    .await?
            }
            other => {
                return Err(aaos_core::CoreError::Ipc(format!(
                    "unknown scaffold kind '{other}'"
                )));
            }
        };

        Ok(SubtaskResult {
            subtask_id: subtask_id.to_string(),
            agent_id,
            response,
            // Scaffolds don't touch the LLM; real token usage is zero.
            input_tokens: 0,
            output_tokens: 0,
        })
    }

    /// Deterministic fetcher: web_fetch(url) → file_write(workspace, body)
    /// → return workspace path. Both calls route through tool_invocation
    /// so capability checks + audit events fire normally.
    async fn scaffold_fetcher(
        self: &Arc<Self>,
        agent_id: aaos_core::AgentId,
        token_handles: &[aaos_core::CapabilityHandle],
        params: &serde_json::Value,
    ) -> Result<String, aaos_core::CoreError> {
        let url = params.get("url").and_then(|v| v.as_str()).ok_or_else(|| {
            aaos_core::CoreError::Ipc("scaffold fetcher: missing 'url' param".into())
        })?;
        let workspace = params
            .get("workspace")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                aaos_core::CoreError::Ipc("scaffold fetcher: missing 'workspace' param".into())
            })?;

        // Ensure the workspace's parent directory exists. The runtime
        // creates /var/lib/aaos/workspace/<run-id>/ but roles may write
        // into nested subdirs (e.g. {run}/fetched/<file>.html).
        if let Some(parent) = std::path::Path::new(workspace).parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                aaos_core::CoreError::Ipc(format!(
                    "scaffold fetcher: mkdir {}: {e}",
                    parent.display()
                ))
            })?;
        }

        // Step 1: web_fetch. Returns JSON {status, content_type, body}.
        let fetch_input = serde_json::json!({"url": url});
        let fetch_result = self
            .tool_invocation
            .invoke(agent_id, "web_fetch", fetch_input, token_handles)
            .await?;
        // Reject non-2xx up front — otherwise the scaffold silently
        // writes an error page (or empty body) to disk and downstream
        // subtasks treat it as real content.
        let status = fetch_result
            .get("status")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                aaos_core::CoreError::Ipc(
                    "scaffold fetcher: web_fetch returned no status field".into(),
                )
            })?;
        if !(200..300).contains(&status) {
            return Err(aaos_core::CoreError::Ipc(format!(
                "scaffold fetcher: {} returned HTTP {}",
                url, status
            )));
        }
        let body = fetch_result
            .get("body")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                aaos_core::CoreError::Ipc(
                    "scaffold fetcher: web_fetch returned no body field".into(),
                )
            })?
            .to_string();
        if body.is_empty() {
            return Err(aaos_core::CoreError::Ipc(format!(
                "scaffold fetcher: {} returned empty body",
                url
            )));
        }

        // Step 2: file_write.
        let write_input = serde_json::json!({
            "path": workspace,
            "content": body,
        });
        let _ = self
            .tool_invocation
            .invoke(agent_id, "file_write", write_input, token_handles)
            .await?;

        // Step 3: return the workspace path as the subtask response.
        Ok(workspace.to_string())
    }

    /// Run a subtask by spawning a child from the rendered manifest,
    /// running its LLM execution loop to completion, and collecting the
    /// final assistant text. The child is ephemeral — stopped on any
    /// return path via a scopeguard so no orphans survive an early
    /// error.
    #[doc(hidden)]
    pub async fn run_subtask_inline(
        self: &Arc<Self>,
        subtask_id: &str,
        manifest_yaml: &str,
        message: &str,
        overrides: SubtaskExecutorOverrides,
        deadline: Option<std::time::Instant>,
        run_root: std::path::PathBuf,
    ) -> Result<SubtaskResult, aaos_core::CoreError> {
        // Parse the rendered manifest.
        let manifest = aaos_core::AgentManifest::from_yaml(manifest_yaml)
            .map_err(|e| aaos_core::CoreError::Ipc(format!("parse role manifest: {e}")))?;

        // Spawn ephemeral child — NOT persistent, no session_store pinning.
        let agent_id = self
            .registry
            .spawn(manifest.clone())
            .map_err(|e| aaos_core::CoreError::Ipc(format!("spawn subtask: {e}")))?;

        // Scopeguard: stop the agent on any return path (success or error).
        let cleanup_registry = self.registry.clone();
        let _guard = scopeguard::guard(agent_id, move |aid| {
            let _ = cleanup_registry.stop_sync(aid);
        });

        // Phase F-b/3b+3c: when a NamespacedBackend is active, launch a
        // worker session for this subtask agent so tool calls route
        // worker-side under Landlock + seccomp. `run_root` is the
        // per-run workspace (/var/lib/aaos/workspace/<run-id>/) —
        // bind-mounted into the worker's mount ns at the same absolute
        // path so tools using workspace paths resolve identically
        // daemon-side vs worker-side.
        //
        // Default ON now that Gap C (workspace bind-mount) is fixed.
        // Opt out via AAOS_CONFINE_SUBTASKS=0 to force the legacy
        // daemon-side path (e.g. for latency debugging).
        #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
        let _namespaced_guard = {
            let confine = std::env::var("AAOS_CONFINE_SUBTASKS")
                .map(|v| v != "0")
                .unwrap_or(true);
            if confine {
                self.launch_worker_session_for_subtask(agent_id, &manifest, run_root.clone())
                    .await
            } else {
                None
            }
        };
        #[cfg(not(all(target_os = "linux", feature = "namespaced-agents")))]
        let _ = &run_root;

        // Run the LLM execution loop for this agent. Overrides carry the
        // role's budget (max_output_tokens) + retry (max_iterations) so
        // the per-role YAML actually constrains the LLM call.
        let response = self
            .execute_agent_for_subtask(
                agent_id, &manifest, message, overrides, subtask_id, deadline,
            )
            .await?;

        Ok(SubtaskResult {
            subtask_id: subtask_id.to_string(),
            agent_id,
            response,
            // Token aggregation arrives in a follow-up; today we zero
            // these out rather than lie.
            input_tokens: 0,
            output_tokens: 0,
        })
    }

    /// Run the LLM execution loop for a freshly-spawned subtask agent
    /// and return the final assistant text. Mirrors the construction
    /// recipe of `execute_agent` but returns the response string
    /// directly instead of a `JsonRpcResponse`. Kept as a small
    /// duplication (~25 lines) rather than refactoring `execute_agent`
    /// to preserve the existing JSON-RPC call path untouched; a future
    /// cleanup task can extract the shared builder.
    async fn execute_agent_for_subtask(
        self: &Arc<Self>,
        agent_id: aaos_core::AgentId,
        manifest: &aaos_core::AgentManifest,
        first_message: &str,
        overrides: SubtaskExecutorOverrides,
        subtask_id: &str,
        deadline: Option<std::time::Instant>,
    ) -> Result<String, aaos_core::CoreError> {
        let raw_llm = self.llm_client.clone().ok_or_else(|| {
            aaos_core::CoreError::Ipc("no LLM client configured for subtask execution".into())
        })?;

        // Phase F-b/2: register subtask→model for per-model latency stats.
        // Use the manifest's own model field — that's what the subtask
        // will actually call (render_manifest_with_model baked it in
        // based on the current tier in spawn_subtask).
        self.per_model_latency.register(subtask_id, &manifest.model);

        // Composite tracker feeds both SubtaskWallClockTracker (TTL-consuming)
        // and PerModelLatencyTracker (observability) from one wrap.
        let composite: Arc<dyn aaos_runtime::LatencyTracker> =
            Arc::new(aaos_runtime::scheduler::CompositeLatencyTracker::new(vec![
                self.latency_tracker.clone(),
                self.per_model_latency.clone() as Arc<dyn aaos_runtime::LatencyTracker>,
            ]));

        // Wrap the real client with a per-subtask SchedulerView so every
        // complete() call routes through the reasoning scheduler and
        // records elapsed time in the latency tracker. Priority from
        // role YAML comes in a follow-up; for now use the default mid-
        // bucket (128).
        let llm: Arc<dyn aaos_llm::LlmClient> =
            Arc::new(aaos_runtime::scheduler::SchedulerView::new(
                raw_llm,
                self.reasoning_scheduler.clone(),
                composite,
                subtask_id.to_string(),
                128,
                deadline,
            ));

        // Emit execution started audit event (correlated to the subtask
        // child via agent_id).
        self.audit_log.record(aaos_core::AuditEvent::new(
            agent_id,
            aaos_core::AuditEventKind::AgentExecutionStarted {
                message_preview: first_message.chars().take(100).collect(),
            },
        ));

        let services: Arc<dyn AgentServices> = Arc::new(InProcessAgentServices::new(
            self.registry.clone(),
            self.tool_invocation.clone(),
            self.tool_registry.clone(),
            self.audit_log.clone(),
            self.router.clone(),
            self.approval_queue.clone() as Arc<dyn ApprovalService>,
        ));

        // Build the per-subtask ExecutorConfig from the role's overrides.
        // max_total_tokens inherits the default (1_000_000) — that's a
        // whole-run cap and shouldn't need per-role tuning today.
        let config = ExecutorConfig {
            max_iterations: overrides.max_iterations,
            max_total_tokens: ExecutorConfig::default().max_total_tokens,
            max_output_tokens: overrides.max_output_tokens,
        };
        let executor = AgentExecutor::new(llm, services, config);
        let result = executor.run(agent_id, manifest, first_message).await;

        self.audit_log.record(aaos_core::AuditEvent::new(
            agent_id,
            aaos_core::AuditEventKind::AgentExecutionCompleted {
                stop_reason: result.stop_reason.to_string(),
                total_iterations: result.iterations,
            },
        ));

        Ok(result.response)
    }

    /// Launch a per-subtask worker session on the namespaced backend so
    /// tool calls emitted by this subtask's LLM loop route worker-side
    /// (confined under Landlock + seccomp) instead of falling back to
    /// daemon-side via `WorkerInvokeError::NoSession`.
    ///
    /// Returns a scopeguard that stops the worker on drop. `None` if
    /// the namespaced backend isn't in use (feature off, env not set,
    /// or NamespacedBackend::new failed at startup) — in that case the
    /// subtask runs daemon-side as before.
    ///
    /// The LLM loop itself stays in the daemon; only tool invocations
    /// cross the broker. That's the whole point of sub-project 3 —
    /// the daemon keeps provider clients + API keys out of the
    /// sandbox while tool code runs under confinement.
    #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
    async fn launch_worker_session_for_subtask(
        self: &Arc<Self>,
        agent_id: aaos_core::AgentId,
        manifest: &aaos_core::AgentManifest,
        workspace_path: std::path::PathBuf,
    ) -> Option<scopeguard::ScopeGuard<
        (
            Arc<aaos_backend_linux::NamespacedBackend>,
            aaos_core::AgentLaunchHandle,
        ),
        impl FnOnce(
            (
                Arc<aaos_backend_linux::NamespacedBackend>,
                aaos_core::AgentLaunchHandle,
            ),
        ),
    >> {
        let nb = self.namespaced.as_ref()?.clone();

        // Look up the subtask agent's capability handles so the worker
        // can receive them in the launch spec. If the lookup fails
        // (agent stopped between spawn and here) treat as "no session";
        // the tool routing code will fall back daemon-side.
        let caps = self.registry.get_token_handles(agent_id).ok()?;

        let spec = aaos_core::AgentLaunchSpec {
            agent_id,
            manifest: manifest.clone(),
            capability_handles: caps,
            workspace_path,
            budget_config: manifest.budget_config,
        };

        match nb.launch(spec).await {
            Ok(handle) => {
                tracing::debug!(%agent_id, "subtask worker launched under NamespacedBackend");
                Some(scopeguard::guard((nb, handle), |(nb, handle)| {
                    // Best-effort cleanup; block briefly to tear the
                    // worker down. If we can't reach the tokio runtime,
                    // the backend will reap the worker when agentd
                    // exits.
                    let rt = tokio::runtime::Handle::try_current();
                    if let Ok(handle_rt) = rt {
                        handle_rt.spawn(async move {
                            let _ = nb.stop(&handle).await;
                        });
                    }
                }))
            }
            Err(e) => {
                tracing::warn!(
                    %agent_id, error = %e,
                    "subtask worker launch failed; falling back to daemon-side tool execution"
                );
                None
            }
        }
    }

    /// Build the reasoning-slot scheduler + latency tracker used by
    /// `run_subtask_inline`. Factored out of the three constructors
    /// (new, with_llm_and_audit, with_memory) so they can't silently
    /// diverge — a changed env-var name or default would otherwise
    /// need to be updated in three places. See Phase F-b design.
    fn build_scheduler_and_tracker() -> (
        Arc<aaos_runtime::scheduler::ReasoningScheduler>,
        Arc<dyn aaos_runtime::LatencyTracker>,
        Arc<aaos_runtime::scheduler::PerModelLatencyTracker>,
    ) {
        let max_concurrent = std::env::var("AAOS_MAX_CONCURRENT_INFERENCE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(3);
        (
            aaos_runtime::scheduler::ReasoningScheduler::new(max_concurrent),
            Arc::new(aaos_runtime::SubtaskWallClockTracker::new()),
            Arc::new(aaos_runtime::scheduler::PerModelLatencyTracker::new()),
        )
    }

    /// Create a new server with default configuration.
    pub fn new() -> Self {
        let inner_audit: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let broadcast_audit = Arc::new(BroadcastAuditLog::new(inner_audit, 256));
        let audit_log: Arc<dyn AuditLog> = broadcast_audit.clone();
        let approval_queue = Arc::new(crate::approval::ApprovalQueue::new());
        let registry = Arc::new(AgentRegistry::new(audit_log.clone()));
        let tool_registry = Arc::new(ToolRegistry::new());
        let validator = Arc::new(SchemaValidator::new());

        // Register built-in tools
        tool_registry.register(Arc::new(EchoTool));
        tool_registry.register(Arc::new(aaos_tools::WebFetchTool::new()));
        tool_registry.register(Arc::new(aaos_tools::FileReadTool));
        tool_registry.register(Arc::new(aaos_tools::FileListTool));
        tool_registry.register(Arc::new(aaos_tools::FileReadManyTool));
        tool_registry.register(Arc::new(aaos_tools::FileWriteTool));
        tool_registry.register(Arc::new(aaos_tools::FileEditTool));
        tool_registry.register(Arc::new(aaos_tools::CargoRunTool));
        tool_registry.register(Arc::new(aaos_tools::GrepTool));
        tool_registry.register(Arc::new(aaos_tools::GitCommitTool));

        // Memory subsystem: SQLite if AAOS_MEMORY_DB is set, in-memory otherwise
        let embedding_source: Arc<dyn aaos_memory::EmbeddingSource> =
            Arc::new(aaos_memory::MockEmbeddingSource::new(768));
        let memory_store: Arc<dyn aaos_memory::MemoryStore> =
            create_memory_store(embedding_source.model_name());

        // Register memory tools
        tool_registry.register(Arc::new(aaos_tools::MemoryStoreTool::new(
            memory_store.clone(),
            embedding_source.clone(),
            audit_log.clone(),
            4096,
        )));
        tool_registry.register(Arc::new(aaos_tools::MemoryQueryTool::new(
            memory_store.clone(),
            embedding_source.clone(),
            audit_log.clone(),
        )));
        tool_registry.register(Arc::new(aaos_tools::MemoryDeleteTool::new(
            memory_store.clone(),
            audit_log.clone(),
        )));

        // Build a base ToolInvocation (no WorkerHandle yet) to pass into the
        // InProcessAgentServices closure inside the backend builder. A second
        // ToolInvocation wired with the BrokerWorkerHandle is constructed below
        // after we know which backend was selected.
        let tool_invocation_base = Arc::new(ToolInvocation::new(
            tool_registry.clone(),
            audit_log.clone(),
            registry.capability_registry().clone(),
        ));

        // Create message router with capability checking via the registry
        let registry_clone = registry.clone();
        let router = Arc::new(MessageRouter::new(
            audit_log.clone(),
            move |agent_id, cap| {
                registry_clone
                    .check_capability(agent_id, cap)
                    .unwrap_or(false)
            },
        ));

        // Set router on registry for spawn/stop registration
        registry.set_router(router.clone());

        let session_store: Arc<dyn aaos_runtime::SessionStore> =
            Arc::new(aaos_runtime::InMemorySessionStore::new());

        let selected = Self::build_in_process_backend(
            registry.clone(),
            session_store.clone(),
            router.clone(),
            audit_log.clone(),
            tool_invocation_base.clone(),
            tool_registry.clone(),
            approval_queue.clone(),
            None,
        );

        // Build the real ToolInvocation: wire BrokerWorkerHandle when namespaced.
        #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
        let tool_invocation = match &selected.namespaced {
            Some(nb) => Arc::new(ToolInvocation::new_with_worker_handle(
                tool_registry.clone(),
                audit_log.clone(),
                registry.capability_registry().clone(),
                Arc::new(BrokerWorkerHandle { backend: nb.clone() }),
            )),
            None => tool_invocation_base,
        };
        #[cfg(not(all(target_os = "linux", feature = "namespaced-agents")))]
        let tool_invocation = tool_invocation_base;

        // Phase F-b sub-project 1: reasoning-slot scheduler + latency tracker.
        // Slot count honors AAOS_MAX_CONCURRENT_INFERENCE (existing env var;
        // default 3). SchedulerView wraps the LLM client per subtask.
        let (reasoning_scheduler, latency_tracker, per_model_latency) =
            Self::build_scheduler_and_tracker();

        Self {
            registry,
            tool_registry,
            tool_invocation,
            router,
            validator,
            audit_log,
            broadcast_audit,
            plan_executor: std::sync::OnceLock::new(),
            role_catalog: None,
            planner: None,
            run_root_base: Self::default_run_root_base(),
            approval_queue,
            llm_client: None,
            session_store,
            memory_store,
            embedding_source,
            skill_registry: Arc::new(aaos_tools::SkillRegistry::new(vec![])),
            backend: selected.backend,
            #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
            namespaced: selected.namespaced,
            reasoning_scheduler,
            latency_tracker,
            per_model_latency,
        }
    }

    /// Create a server with a specific LLM client (for testing).
    #[allow(dead_code)]
    pub fn with_llm_client(llm_client: Arc<dyn LlmClient>) -> Arc<Self> {
        let mut server = Self::new();
        // Register SpawnAgentTool with the LLM client
        let spawn_tool = Arc::new(crate::spawn_tool::SpawnAgentTool::new(
            llm_client.clone(),
            server.registry.clone(),
            server.tool_registry.clone(),
            server.tool_invocation.clone(),
            server.audit_log.clone(),
            server.router.clone(),
            server.approval_queue.clone() as Arc<dyn ApprovalService>,
        ));
        server.tool_registry.register(spawn_tool.clone());
        // Register SpawnAgentsTool (batch, parallel) — delegates per-child
        // to SpawnAgentTool so cleanup (scopeguard) stays in one place.
        server
            .tool_registry
            .register(Arc::new(crate::spawn_agents_tool::SpawnAgentsTool::new(
                spawn_tool,
                server.registry.clone(),
            )));
        server.llm_client = Some(llm_client.clone());
        let (catalog, planner) = Self::load_role_catalog(&llm_client);
        server.role_catalog = catalog;
        server.planner = planner;
        // Rebuild the backend with the LLM client so persistent
        // agents can launch through it.
        let selected = Self::build_in_process_backend(
            server.registry.clone(),
            server.session_store.clone(),
            server.router.clone(),
            server.audit_log.clone(),
            server.tool_invocation.clone(),
            server.tool_registry.clone(),
            server.approval_queue.clone(),
            Some(llm_client),
        );
        server.backend = selected.backend;
        #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
        {
            if selected.namespaced.is_some() {
                // Rebuild tool_invocation with the BrokerWorkerHandle now that
                // namespaced is confirmed active.
                server.tool_invocation = Arc::new(ToolInvocation::new_with_worker_handle(
                    server.tool_registry.clone(),
                    server.audit_log.clone(),
                    server.registry.capability_registry().clone(),
                    Arc::new(BrokerWorkerHandle {
                        backend: selected.namespaced.clone().unwrap(),
                    }),
                ));
            }
            server.namespaced = selected.namespaced;
        }
        let server = Arc::new(server);
        server.install_plan_executor_runner();
        server
    }

    /// Create a server with a specific LLM client and a custom audit log.
    /// Used by bootstrap mode to wire StdoutAuditLog for container observability.
    pub fn with_llm_and_audit(
        llm_client: Arc<dyn LlmClient>,
        audit_log: Arc<dyn AuditLog>,
    ) -> Arc<Self> {
        // Wrap whatever audit sink the caller provided (e.g. StdoutAuditLog
        // for bootstrap container mode) with a BroadcastAuditLog so
        // streaming handlers can still subscribe. The original sink stays
        // the inner; everything recorded flows through unchanged.
        let broadcast_audit = Arc::new(BroadcastAuditLog::new(audit_log, 256));
        let audit_log: Arc<dyn AuditLog> = broadcast_audit.clone();
        let approval_queue = Arc::new(crate::approval::ApprovalQueue::new());
        let registry = Arc::new(AgentRegistry::new(audit_log.clone()));
        let tool_registry = Arc::new(ToolRegistry::new());
        let validator = Arc::new(SchemaValidator::new());

        // Register built-in tools
        tool_registry.register(Arc::new(EchoTool));
        tool_registry.register(Arc::new(aaos_tools::WebFetchTool::new()));
        tool_registry.register(Arc::new(aaos_tools::FileReadTool));
        tool_registry.register(Arc::new(aaos_tools::FileListTool));
        tool_registry.register(Arc::new(aaos_tools::FileReadManyTool));
        tool_registry.register(Arc::new(aaos_tools::FileWriteTool));
        tool_registry.register(Arc::new(aaos_tools::FileEditTool));
        tool_registry.register(Arc::new(aaos_tools::CargoRunTool));
        tool_registry.register(Arc::new(aaos_tools::GrepTool));
        tool_registry.register(Arc::new(aaos_tools::GitCommitTool));

        // Memory subsystem: SQLite if AAOS_MEMORY_DB is set, in-memory otherwise
        let embedding_source: Arc<dyn aaos_memory::EmbeddingSource> =
            Arc::new(aaos_memory::MockEmbeddingSource::new(768));
        let memory_store: Arc<dyn aaos_memory::MemoryStore> =
            create_memory_store(embedding_source.model_name());

        // Register memory tools
        tool_registry.register(Arc::new(aaos_tools::MemoryStoreTool::new(
            memory_store.clone(),
            embedding_source.clone(),
            audit_log.clone(),
            4096,
        )));
        tool_registry.register(Arc::new(aaos_tools::MemoryQueryTool::new(
            memory_store.clone(),
            embedding_source.clone(),
            audit_log.clone(),
        )));
        tool_registry.register(Arc::new(aaos_tools::MemoryDeleteTool::new(
            memory_store.clone(),
            audit_log.clone(),
        )));

        // Build base ToolInvocation (no WorkerHandle) for the InProcessAgentServices closure.
        let tool_invocation_base = Arc::new(ToolInvocation::new(
            tool_registry.clone(),
            audit_log.clone(),
            registry.capability_registry().clone(),
        ));

        let registry_clone = registry.clone();
        let router = Arc::new(MessageRouter::new(
            audit_log.clone(),
            move |agent_id, cap| {
                registry_clone
                    .check_capability(agent_id, cap)
                    .unwrap_or(false)
            },
        ));
        registry.set_router(router.clone());

        let session_store: Arc<dyn aaos_runtime::SessionStore> =
            Arc::new(aaos_runtime::InMemorySessionStore::new());

        // Discover and load skills
        let skill_registry = Arc::new(aaos_tools::SkillRegistry::new(discover_all_skills()));

        // Register skill_read tool
        tool_registry.register(Arc::new(aaos_tools::SkillReadTool::new(
            skill_registry.clone(),
            audit_log.clone(),
        )));

        // Register SpawnAgentTool with the LLM client
        let spawn_tool = Arc::new(crate::spawn_tool::SpawnAgentTool::new(
            llm_client.clone(),
            registry.clone(),
            tool_registry.clone(),
            tool_invocation_base.clone(),
            audit_log.clone(),
            router.clone(),
            approval_queue.clone() as Arc<dyn ApprovalService>,
        ));
        tool_registry.register(spawn_tool.clone());
        // Register SpawnAgentsTool (batch, parallel).
        tool_registry.register(Arc::new(crate::spawn_agents_tool::SpawnAgentsTool::new(
            spawn_tool,
            registry.clone(),
        )));

        let selected = Self::build_in_process_backend(
            registry.clone(),
            session_store.clone(),
            router.clone(),
            audit_log.clone(),
            tool_invocation_base.clone(),
            tool_registry.clone(),
            approval_queue.clone(),
            Some(llm_client.clone()),
        );

        // Build the real ToolInvocation: wire BrokerWorkerHandle when namespaced.
        #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
        let tool_invocation = match &selected.namespaced {
            Some(nb) => Arc::new(ToolInvocation::new_with_worker_handle(
                tool_registry.clone(),
                audit_log.clone(),
                registry.capability_registry().clone(),
                Arc::new(BrokerWorkerHandle { backend: nb.clone() }),
            )),
            None => tool_invocation_base,
        };
        #[cfg(not(all(target_os = "linux", feature = "namespaced-agents")))]
        let tool_invocation = tool_invocation_base;

        let (role_catalog, planner) = Self::load_role_catalog(&llm_client);

        // Phase F-b sub-project 1: reasoning-slot scheduler + latency tracker.
        let (reasoning_scheduler, latency_tracker, per_model_latency) =
            Self::build_scheduler_and_tracker();

        let server = Arc::new(Self {
            registry,
            tool_registry,
            tool_invocation,
            router,
            validator,
            audit_log,
            broadcast_audit,
            plan_executor: std::sync::OnceLock::new(),
            role_catalog,
            planner,
            run_root_base: Self::default_run_root_base(),
            approval_queue,
            llm_client: Some(llm_client),
            session_store,
            memory_store,
            embedding_source,
            skill_registry,
            backend: selected.backend,
            #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
            namespaced: selected.namespaced,
            reasoning_scheduler,
            latency_tracker,
            per_model_latency,
        });
        server.install_plan_executor_runner();
        server
    }

    /// Create a server with a specific LLM client and custom memory/embedding sources.
    #[allow(dead_code)]
    pub fn with_memory(
        llm_client: Arc<dyn LlmClient>,
        memory_store: Arc<dyn aaos_memory::MemoryStore>,
        embedding_source: Arc<dyn aaos_memory::EmbeddingSource>,
    ) -> Arc<Self> {
        let inner_audit: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let broadcast_audit = Arc::new(BroadcastAuditLog::new(inner_audit, 256));
        let audit_log: Arc<dyn AuditLog> = broadcast_audit.clone();
        let approval_queue = Arc::new(crate::approval::ApprovalQueue::new());
        let registry = Arc::new(AgentRegistry::new(audit_log.clone()));
        let tool_registry = Arc::new(ToolRegistry::new());
        let validator = Arc::new(SchemaValidator::new());

        // Register built-in tools
        tool_registry.register(Arc::new(EchoTool));
        tool_registry.register(Arc::new(aaos_tools::WebFetchTool::new()));
        tool_registry.register(Arc::new(aaos_tools::FileReadTool));
        tool_registry.register(Arc::new(aaos_tools::FileListTool));
        tool_registry.register(Arc::new(aaos_tools::FileReadManyTool));
        tool_registry.register(Arc::new(aaos_tools::FileWriteTool));
        tool_registry.register(Arc::new(aaos_tools::FileEditTool));
        tool_registry.register(Arc::new(aaos_tools::CargoRunTool));
        tool_registry.register(Arc::new(aaos_tools::GrepTool));
        tool_registry.register(Arc::new(aaos_tools::GitCommitTool));

        // Register memory tools with the provided sources
        tool_registry.register(Arc::new(aaos_tools::MemoryStoreTool::new(
            memory_store.clone(),
            embedding_source.clone(),
            audit_log.clone(),
            4096,
        )));
        tool_registry.register(Arc::new(aaos_tools::MemoryQueryTool::new(
            memory_store.clone(),
            embedding_source.clone(),
            audit_log.clone(),
        )));
        tool_registry.register(Arc::new(aaos_tools::MemoryDeleteTool::new(
            memory_store.clone(),
            audit_log.clone(),
        )));

        // Build base ToolInvocation (no WorkerHandle) for the InProcessAgentServices closure.
        let tool_invocation_base = Arc::new(ToolInvocation::new(
            tool_registry.clone(),
            audit_log.clone(),
            registry.capability_registry().clone(),
        ));

        let registry_clone = registry.clone();
        let router = Arc::new(MessageRouter::new(
            audit_log.clone(),
            move |agent_id, cap| {
                registry_clone
                    .check_capability(agent_id, cap)
                    .unwrap_or(false)
            },
        ));
        registry.set_router(router.clone());

        let session_store: Arc<dyn aaos_runtime::SessionStore> =
            Arc::new(aaos_runtime::InMemorySessionStore::new());

        // Register SpawnAgentTool with the LLM client
        let spawn_tool = Arc::new(crate::spawn_tool::SpawnAgentTool::new(
            llm_client.clone(),
            registry.clone(),
            tool_registry.clone(),
            tool_invocation_base.clone(),
            audit_log.clone(),
            router.clone(),
            approval_queue.clone() as Arc<dyn ApprovalService>,
        ));
        tool_registry.register(spawn_tool.clone());
        // Register SpawnAgentsTool (batch, parallel).
        tool_registry.register(Arc::new(crate::spawn_agents_tool::SpawnAgentsTool::new(
            spawn_tool,
            registry.clone(),
        )));

        let selected = Self::build_in_process_backend(
            registry.clone(),
            session_store.clone(),
            router.clone(),
            audit_log.clone(),
            tool_invocation_base.clone(),
            tool_registry.clone(),
            approval_queue.clone(),
            Some(llm_client.clone()),
        );

        // Build the real ToolInvocation: wire BrokerWorkerHandle when namespaced.
        #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
        let tool_invocation = match &selected.namespaced {
            Some(nb) => Arc::new(ToolInvocation::new_with_worker_handle(
                tool_registry.clone(),
                audit_log.clone(),
                registry.capability_registry().clone(),
                Arc::new(BrokerWorkerHandle { backend: nb.clone() }),
            )),
            None => tool_invocation_base,
        };
        #[cfg(not(all(target_os = "linux", feature = "namespaced-agents")))]
        let tool_invocation = tool_invocation_base;

        let (role_catalog, planner) = Self::load_role_catalog(&llm_client);

        // Phase F-b sub-project 1: reasoning-slot scheduler + latency tracker.
        let (reasoning_scheduler, latency_tracker, per_model_latency) =
            Self::build_scheduler_and_tracker();

        let server = Arc::new(Self {
            registry,
            tool_registry,
            tool_invocation,
            router,
            validator,
            audit_log,
            broadcast_audit,
            plan_executor: std::sync::OnceLock::new(),
            role_catalog,
            planner,
            run_root_base: Self::default_run_root_base(),
            approval_queue,
            llm_client: Some(llm_client),
            session_store,
            memory_store,
            embedding_source,
            skill_registry: Arc::new(aaos_tools::SkillRegistry::new(vec![])),
            backend: selected.backend,
            #[cfg(all(target_os = "linux", feature = "namespaced-agents"))]
            namespaced: selected.namespaced,
            reasoning_scheduler,
            latency_tracker,
            per_model_latency,
        });
        server.install_plan_executor_runner();
        server
    }

    /// Spawn an agent from YAML with a caller-supplied stable ID.
    /// Only used by privileged internal paths (Bootstrap Agent persistence).
    pub async fn spawn_with_pinned_id(
        &self,
        yaml: &str,
        pinned_id: aaos_core::AgentId,
    ) -> JsonRpcResponse {
        self.spawn_from_yaml_with_id(yaml, serde_json::Value::Null, Some(pinned_id))
            .await
    }

    /// Handle a JSON-RPC request and return a response.
    pub async fn handle_request(&self, request: &crate::api::JsonRpcRequest) -> JsonRpcResponse {
        match request.method.as_str() {
            "agent.spawn" => {
                self.handle_agent_spawn(&request.params, request.id.clone())
                    .await
            }
            "agent.stop" => {
                self.handle_agent_stop(&request.params, request.id.clone())
                    .await
            }
            "agent.list" => self.handle_agent_list(request.id.clone()),
            "agent.status" => self.handle_agent_status(&request.params, request.id.clone()),
            "tool.list" => self.handle_tool_list(request.id.clone()),
            "tool.invoke" => {
                self.handle_tool_invoke(&request.params, request.id.clone())
                    .await
            }
            "agent.run" => {
                self.handle_agent_run(&request.params, request.id.clone())
                    .await
            }
            "agent.spawn_and_run" => {
                self.handle_agent_spawn_and_run(&request.params, request.id.clone())
                    .await
            }
            "approval.list" => self.handle_approval_list(request.id.clone()),
            "approval.respond" => self.handle_approval_respond(&request.params, request.id.clone()),
            _ => JsonRpcResponse::error(request.id.clone(), METHOD_NOT_FOUND, "method not found"),
        }
    }

    async fn handle_agent_spawn(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let manifest_yaml = match params.get("manifest").and_then(|m| m.as_str()) {
            Some(yaml) => yaml,
            None => match params.get("manifest_path").and_then(|p| p.as_str()) {
                Some(path) => match std::fs::read_to_string(path) {
                    Ok(content) => return self.spawn_from_yaml(&content, id).await,
                    Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                },
                None => {
                    return JsonRpcResponse::error(
                        id,
                        INTERNAL_ERROR,
                        "missing 'manifest' or 'manifest_path' parameter",
                    )
                }
            },
        };
        self.spawn_from_yaml(manifest_yaml, id).await
    }

    async fn spawn_from_yaml(&self, yaml: &str, id: serde_json::Value) -> JsonRpcResponse {
        self.spawn_from_yaml_with_id(yaml, id, None).await
    }

    async fn spawn_from_yaml_with_id(
        &self,
        yaml: &str,
        id: serde_json::Value,
        pinned_id: Option<aaos_core::AgentId>,
    ) -> JsonRpcResponse {
        let mut manifest = match AgentManifest::from_yaml(yaml) {
            Ok(m) => m,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };

        // Inject skill catalog into the agent's system prompt (progressive disclosure tier 1)
        let catalog = self.skill_registry.catalog();
        if !catalog.is_empty() {
            let original_prompt = match &manifest.system_prompt {
                aaos_core::PromptSource::Inline(s) => s.clone(),
                aaos_core::PromptSource::File(p) => std::fs::read_to_string(p).unwrap_or_default(),
            };
            manifest.system_prompt =
                aaos_core::PromptSource::Inline(format!("{original_prompt}\n\n{catalog}"));
        }

        let is_persistent = manifest.lifecycle == aaos_core::Lifecycle::Persistent;

        let agent_id = match pinned_id {
            Some(pin) => match self.registry.spawn_with_id(manifest.clone(), pin) {
                Ok(id) => id,
                Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            },
            None => match self.registry.spawn(manifest.clone()) {
                Ok(id) => id,
                Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            },
        };

        // For persistent agents: start the background message loop
        // through the configured backend. This used to inline the
        // executor + ContextManager construction here; the refactor in
        // commit 1 of `plans/2026-04-15-namespaced-backend-v4.md`
        // moved it into `InProcessBackend::launch`.
        if is_persistent {
            if self.llm_client.is_none() {
                return JsonRpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    "persistent agents require an LLM client",
                );
            }

            let caps = match self.registry.get_token_handles(agent_id) {
                Ok(c) => c,
                Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            };
            let budget_config = manifest.budget_config;
            let spec = AgentLaunchSpec {
                agent_id,
                manifest: manifest.clone(),
                capability_handles: caps,
                // No per-agent workspace in the in-process path; kept
                // as a field on the spec for future backends.
                workspace_path: std::path::PathBuf::new(),
                budget_config,
            };

            if let Err(e) = self.backend.launch(spec).await {
                return JsonRpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    format!("failed to launch persistent agent: {e}"),
                );
            }
        }

        JsonRpcResponse::success(id, json!({"agent_id": agent_id}))
    }

    async fn handle_agent_stop(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let agent_id_str = match params.get("agent_id").and_then(|a| a.as_str()) {
            Some(s) => s,
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'agent_id' parameter")
            }
        };
        let agent_id: aaos_core::AgentId = match serde_json::from_value(json!(agent_id_str)) {
            Ok(id) => id,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };
        match self.registry.stop(agent_id).await {
            Ok(()) => JsonRpcResponse::success(id, json!({"ok": true})),
            Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        }
    }

    fn handle_agent_list(&self, id: serde_json::Value) -> JsonRpcResponse {
        let agents: Vec<_> = self
            .registry
            .list()
            .into_iter()
            .map(|info| {
                json!({
                    "id": info.id,
                    "name": info.name,
                    "model": info.model,
                    "state": format!("{}", info.state),
                    "capability_count": info.capability_count,
                    "started_at": info.started_at,
                    "parent_agent": info.parent_agent,
                })
            })
            .collect();
        JsonRpcResponse::success(id, json!({"agents": agents}))
    }

    fn handle_agent_status(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let agent_id_str = match params.get("agent_id").and_then(|a| a.as_str()) {
            Some(s) => s,
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'agent_id' parameter")
            }
        };
        let agent_id: aaos_core::AgentId = match serde_json::from_value(json!(agent_id_str)) {
            Ok(id) => id,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };
        match self.registry.get_info(agent_id) {
            Ok(info) => JsonRpcResponse::success(
                id,
                json!({
                    "id": info.id,
                    "name": info.name,
                    "model": info.model,
                    "state": format!("{}", info.state),
                    "capability_count": info.capability_count,
                    "started_at": info.started_at,
                    "parent_agent": info.parent_agent,
                }),
            ),
            Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        }
    }

    fn handle_tool_list(&self, id: serde_json::Value) -> JsonRpcResponse {
        let tools: Vec<_> = self.tool_registry.list();
        JsonRpcResponse::success(id, json!({"tools": tools}))
    }

    async fn handle_tool_invoke(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let agent_id_str = match params.get("agent_id").and_then(|a| a.as_str()) {
            Some(s) => s,
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'agent_id' parameter")
            }
        };
        let agent_id: aaos_core::AgentId = match serde_json::from_value(json!(agent_id_str)) {
            Ok(id) => id,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };

        // Validate agent exists and is running
        match self.registry.get_info(agent_id) {
            Ok(info) => {
                if info.state != AgentState::Running {
                    return JsonRpcResponse::error(
                        id,
                        INTERNAL_ERROR,
                        format!("agent is not running (state: {})", info.state),
                    );
                }
            }
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        }

        let tool_name = match params.get("tool").and_then(|t| t.as_str()) {
            Some(s) => s,
            None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'tool' parameter"),
        };
        let input = params.get("input").cloned().unwrap_or(json!({}));

        // Get tokens and invoke
        match self.registry.get_token_handles(agent_id) {
            Ok(tokens) => {
                match self
                    .tool_invocation
                    .invoke(agent_id, tool_name, input, &tokens)
                    .await
                {
                    Ok(result) => JsonRpcResponse::success(id, json!({"result": result})),
                    Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                }
            }
            Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        }
    }

    async fn handle_agent_run(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let agent_id_str = match params.get("agent_id").and_then(|a| a.as_str()) {
            Some(s) => s,
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'agent_id' parameter")
            }
        };
        let agent_id: aaos_core::AgentId = match serde_json::from_value(json!(agent_id_str)) {
            Ok(id) => id,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };
        let message = match params.get("message").and_then(|m| m.as_str()) {
            Some(s) => s,
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'message' parameter")
            }
        };

        // Validate agent exists and is running, get manifest
        let manifest = match self.registry.get_info(agent_id) {
            Ok(info) => {
                if info.state != aaos_runtime::AgentState::Running {
                    return JsonRpcResponse::error(
                        id,
                        INTERNAL_ERROR,
                        format!("agent is not running (state: {})", info.state),
                    );
                }
                match self.registry.get_manifest(agent_id) {
                    Ok(m) => m,
                    Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                }
            }
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };

        if manifest.lifecycle == aaos_core::Lifecycle::Persistent {
            let msg = aaos_ipc::McpMessage::new(
                agent_id,
                agent_id,
                "agent.run",
                json!({"message": message}),
            );
            let trace_id = msg.metadata.trace_id;

            match self.router.route(msg).await {
                Ok(()) => JsonRpcResponse::success(
                    id,
                    json!({
                        "trace_id": trace_id.to_string(),
                        "status": "delivered",
                    }),
                ),
                Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        } else {
            self.execute_agent(agent_id, &manifest, message, id).await
        }
    }

    async fn handle_agent_spawn_and_run(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let message = match params.get("message").and_then(|m| m.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'message' parameter")
            }
        };

        // Spawn first
        let spawn_resp = self.handle_agent_spawn(params, json!(null)).await;
        let agent_id_str = match spawn_resp.result {
            Some(ref v) => match v.get("agent_id").and_then(|a| a.as_str()) {
                Some(s) => s.to_string(),
                None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "spawn failed"),
            },
            None => {
                return JsonRpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    spawn_resp
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "spawn failed".into()),
                )
            }
        };
        let agent_id: aaos_core::AgentId = serde_json::from_value(json!(agent_id_str))
            .expect("agent_id_str just returned from successful spawn is a valid AgentId");

        let manifest = self
            .registry
            .get_manifest(agent_id)
            .expect("agent just spawned successfully must have a manifest in the registry");
        let mut result = self.execute_agent(agent_id, &manifest, &message, id).await;

        // Inject agent_id into the result
        if let Some(ref mut v) = result.result {
            v["agent_id"] = json!(agent_id_str);
        }
        result
    }

    async fn execute_agent(
        &self,
        agent_id: aaos_core::AgentId,
        manifest: &aaos_core::AgentManifest,
        message: &str,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let llm = match &self.llm_client {
            Some(client) => client.clone(),
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "no LLM client configured");
            }
        };

        // Emit execution started audit event
        self.audit_log.record(aaos_core::AuditEvent::new(
            agent_id,
            aaos_core::AuditEventKind::AgentExecutionStarted {
                message_preview: message.chars().take(100).collect(),
            },
        ));

        let services: Arc<dyn AgentServices> = Arc::new(InProcessAgentServices::new(
            self.registry.clone(),
            self.tool_invocation.clone(),
            self.tool_registry.clone(),
            self.audit_log.clone(),
            self.router.clone(),
            self.approval_queue.clone() as Arc<dyn ApprovalService>,
        ));

        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());
        let result = executor.run(agent_id, manifest, message).await;

        // Emit execution completed audit event
        self.audit_log.record(aaos_core::AuditEvent::new(
            agent_id,
            aaos_core::AuditEventKind::AgentExecutionCompleted {
                stop_reason: result.stop_reason.to_string(),
                total_iterations: result.iterations,
            },
        ));

        JsonRpcResponse::success(
            id,
            json!({
                "response": result.response,
                "usage": {
                    "input_tokens": result.usage.input_tokens,
                    "output_tokens": result.usage.output_tokens,
                },
                "iterations": result.iterations,
                "stop_reason": result.stop_reason.to_string(),
            }),
        )
    }

    fn handle_approval_list(&self, id: serde_json::Value) -> JsonRpcResponse {
        let pending = self.approval_queue.list();
        JsonRpcResponse::success(id, json!({"pending": pending}))
    }

    fn handle_approval_respond(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let approval_id = match params.get("id").and_then(|v| v.as_str()) {
            Some(s) => match uuid::Uuid::parse_str(s) {
                Ok(uid) => uid,
                Err(e) => {
                    return JsonRpcResponse::error(id, INTERNAL_ERROR, format!("invalid id: {e}"))
                }
            },
            None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'id' parameter"),
        };

        let decision = match params.get("decision").and_then(|v| v.as_str()) {
            Some("approve") => aaos_core::ApprovalResult::Approved,
            Some("deny") => {
                let reason = params
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("denied by human")
                    .to_string();
                aaos_core::ApprovalResult::Denied { reason }
            }
            Some(other) => {
                return JsonRpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    format!("invalid decision: {other}. Use 'approve' or 'deny'"),
                )
            }
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'decision' parameter")
            }
        };

        match self.approval_queue.respond(approval_id, decision) {
            Ok(()) => JsonRpcResponse::success(id, json!({"ok": true})),
            Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        }
    }

    /// Dispatch a streaming JSON-RPC method. Owns the connection writer for the
    /// remainder of the connection; emits NDJSON frames terminated by an `end`
    /// frame. The caller closes the connection when this returns.
    async fn handle_streaming<W: tokio::io::AsyncWrite + Unpin>(
        &self,
        request: &crate::api::JsonRpcRequest,
        writer: &mut W,
    ) {
        match request.method.as_str() {
            "agent.submit_streaming" => self.handle_submit_streaming(&request.params, writer).await,
            "agent.logs_streaming" => self.handle_logs_streaming(&request.params, writer).await,
            other => {
                let err = json!({
                    "kind": "end",
                    "exit_code": 1,
                    "error": format!("unknown streaming method: {other}"),
                });
                let _ = write_ndjson(writer, &err).await;
            }
        }
    }

    /// agent.submit_streaming — deliver a goal to Bootstrap, then forward every
    /// audit event in Bootstrap's subtree as NDJSON frames until Bootstrap
    /// reaches a terminal state. UsageReported events are aggregated, not
    /// forwarded. The final frame is `{"kind":"end",...}` with exit code +
    /// aggregated token usage + wall-clock elapsed.
    async fn handle_submit_streaming<W: tokio::io::AsyncWrite + Unpin>(
        &self,
        params: &serde_json::Value,
        writer: &mut W,
    ) {
        use aaos_core::AuditEventKind;
        use tokio::sync::broadcast::error::RecvError;

        let goal = match params.get("goal").and_then(|g| g.as_str()) {
            Some(s) => s.to_string(),
            None => {
                let err = json!({
                    "kind": "end",
                    "exit_code": 2,
                    "error": "missing 'goal' parameter",
                });
                let _ = write_ndjson(writer, &err).await;
                return;
            }
        };

        // ===== PlanExecutor branch =====
        // When a role catalog loaded at startup (/etc/aaos/roles/ or
        // AAOS_ROLES_DIR), route the goal through the two-phase
        // Planner → DAG walk path. When absent, fall through to the
        // legacy Bootstrap path below.
        if let Some(executor) = self.plan_executor.get().cloned() {
            let run_id = uuid::Uuid::new_v4();
            let started = std::time::Instant::now();
            // Subscribe BEFORE spawning so we don't miss the earliest
            // PlanProduced/SubtaskStarted events the executor emits.
            let mut rx = self.broadcast_audit.subscribe();

            let exec_task = tokio::spawn({
                let goal = goal.clone();
                async move { executor.run(&goal, run_id).await }
            });
            tokio::pin!(exec_task);

            loop {
                tokio::select! {
                    result = &mut exec_task => {
                        match result {
                            Ok(Ok(plan_result)) => {
                                let (in_tok, out_tok) =
                                    plan_result.results.values().fold(
                                        (0u64, 0u64),
                                        |(a, b), r| {
                                            (a + r.input_tokens, b + r.output_tokens)
                                        },
                                    );

                                let plan_frame = json!({
                                    "kind": "plan",
                                    "plan": plan_result.plan,
                                });
                                let _ = write_ndjson(writer, &plan_frame).await;

                                if let Some(last) = plan_result.plan.subtasks.last() {
                                    if let Some(r) = plan_result.results.get(&last.id) {
                                        if !r.response.is_empty() {
                                            let ft = json!({
                                                "kind": "final_text",
                                                "text": r.response,
                                            });
                                            let _ = write_ndjson(writer, &ft).await;
                                        }
                                    }
                                }

                                let end = json!({
                                    "kind": "end",
                                    "exit_code": 0,
                                    "input_tokens": in_tok,
                                    "output_tokens": out_tok,
                                    "elapsed_ms": started.elapsed().as_millis() as u64,
                                });
                                let _ = write_ndjson(writer, &end).await;
                            }
                            Ok(Err(e)) => {
                                let frame = json!({
                                    "kind": "end",
                                    "exit_code": 1,
                                    "error": format!("{e}"),
                                    "input_tokens": 0,
                                    "output_tokens": 0,
                                    "elapsed_ms": started.elapsed().as_millis() as u64,
                                });
                                let _ = write_ndjson(writer, &frame).await;
                            }
                            Err(e) => {
                                let frame = json!({
                                    "kind": "end",
                                    "exit_code": 1,
                                    "error": format!("executor task panic: {e}"),
                                    "input_tokens": 0,
                                    "output_tokens": 0,
                                    "elapsed_ms": started.elapsed().as_millis() as u64,
                                });
                                let _ = write_ndjson(writer, &frame).await;
                            }
                        }
                        return;
                    }
                    evt = rx.recv() => {
                        match evt {
                            Ok(event) => {
                                let frame = serde_json::to_value(&event)
                                    .unwrap_or(serde_json::Value::Null);
                                let frame = json!({
                                    "kind": "event",
                                    "event": frame,
                                });
                                if write_ndjson(writer, &frame).await.is_err() {
                                    // Client disconnected mid-run (e.g. Ctrl-C
                                    // at the CLI). Abort the executor task so
                                    // its agent subtree does not keep running
                                    // as an orphan. See run 12: a torn-down
                                    // submit left a zombie builder churning
                                    // in a scratch dir for ~10 minutes.
                                    exec_task.abort();
                                    return;
                                }
                            }
                            Err(RecvError::Lagged(n)) => {
                                let frame = json!({
                                    "kind": "lag",
                                    "missed": n,
                                });
                                let _ = write_ndjson(writer, &frame).await;
                            }
                            Err(RecvError::Closed) => {
                                exec_task.abort();
                                return;
                            }
                        }
                    }
                }
            }
        }
        // ===== End PlanExecutor branch — Bootstrap fallback below =====

        // Subscribe BEFORE routing the goal so we don't miss the first events
        // Bootstrap emits in response.
        let mut rx = self.broadcast_audit.subscribe();

        // Ensure Bootstrap is running (idempotent).
        let bootstrap_id = match self.ensure_bootstrap_running().await {
            Ok(id) => id,
            Err(e) => {
                let err = json!({
                    "kind": "end",
                    "exit_code": 3,
                    "error": format!("failed to start bootstrap: {e}"),
                });
                let _ = write_ndjson(writer, &err).await;
                return;
            }
        };

        // Deliver the goal to Bootstrap via the router — same path as
        // `agent.run` takes for persistent agents.
        if let Err(e) = self.route_goal_to(bootstrap_id, &goal).await {
            let err = json!({
                "kind": "end",
                "exit_code": 3,
                "error": format!("failed to route goal: {e}"),
            });
            let _ = write_ndjson(writer, &err).await;
            return;
        }

        let started = std::time::Instant::now();
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut exit_code: i32 = 0;

        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !self.event_in_subtree(event.agent_id, bootstrap_id) {
                        continue;
                    }
                    // Aggregate usage; never forward.
                    if let AuditEventKind::UsageReported {
                        input_tokens: i,
                        output_tokens: o,
                    } = &event.event
                    {
                        input_tokens = input_tokens.saturating_add(*i);
                        output_tokens = output_tokens.saturating_add(*o);
                        continue;
                    }

                    let frame = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                    let frame = json!({ "kind": "event", "event": frame });
                    if write_ndjson(writer, &frame).await.is_err() {
                        return;
                    }

                    // Only Bootstrap's own terminal events close the stream.
                    // Child failures don't escalate — Bootstrap decides.
                    if event.agent_id == bootstrap_id {
                        match &event.event {
                            AuditEventKind::AgentExecutionCompleted { .. } => break,
                            AuditEventKind::AgentLoopStopped { reason, .. } => {
                                if reason == "error" || reason == "budget_exceeded" {
                                    exit_code = 1;
                                }
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    let frame = json!({ "kind": "lag", "missed": n });
                    let _ = write_ndjson(writer, &frame).await;
                }
                Err(RecvError::Closed) => break,
            }
        }

        // Emit Bootstrap's last assistant-message text as a `final_text` frame
        // before the `end` frame. Lets the CLI show the actual answer, not just
        // the timing summary. Missing or empty → skip the frame.
        if let Some(text) = self.last_assistant_text(bootstrap_id) {
            if !text.is_empty() {
                let frame = json!({ "kind": "final_text", "text": text });
                let _ = write_ndjson(writer, &frame).await;
            }
        }

        let end = json!({
            "kind": "end",
            "exit_code": exit_code,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        });
        let _ = write_ndjson(writer, &end).await;
    }

    /// Pull the most recent assistant message's text from the session store
    /// for the given agent, concatenating any Text content blocks.
    /// Returns None if there's no history, no assistant message, or the load fails.
    fn last_assistant_text(&self, agent_id: aaos_core::AgentId) -> Option<String> {
        let history = self.session_store.load(&agent_id).ok()?;
        history.iter().rev().find_map(|msg| {
            if let aaos_llm::Message::Assistant { content } = msg {
                let text: String = content
                    .iter()
                    .filter_map(|block| match block {
                        aaos_llm::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            } else {
                None
            }
        })
    }

    /// agent.logs_streaming — attach to a single named agent's audit stream.
    /// Unlike `submit_streaming`, this handler:
    ///   * does NOT spawn or message anyone,
    ///   * does NOT walk parent/child chains — strict equality on `agent_id`,
    ///   * emits an end frame ONLY on the target's clean termination (anything
    ///     else keeps the stream open until the client disconnects or the
    ///     broadcast channel dies).
    async fn handle_logs_streaming<W: tokio::io::AsyncWrite + Unpin>(
        &self,
        params: &serde_json::Value,
        writer: &mut W,
    ) {
        use aaos_core::{AgentId, AuditEventKind};
        use tokio::sync::broadcast::error::RecvError;

        // Parse `agent_id` param.
        let agent_id_str = match params.get("agent_id").and_then(|a| a.as_str()) {
            Some(s) => s,
            None => {
                let err = json!({
                    "kind": "end",
                    "exit_code": 2,
                    "error": "missing 'agent_id' parameter",
                });
                let _ = write_ndjson(writer, &err).await;
                return;
            }
        };
        let target: AgentId = match serde_json::from_value(json!(agent_id_str)) {
            Ok(id) => id,
            Err(_) => {
                let err = json!({
                    "kind": "end",
                    "exit_code": 2,
                    "error": "invalid agent_id",
                });
                let _ = write_ndjson(writer, &err).await;
                return;
            }
        };
        if self.registry.get_info(target).is_err() {
            let err = json!({
                "kind": "end",
                "exit_code": 2,
                "error": "agent not found",
            });
            let _ = write_ndjson(writer, &err).await;
            return;
        }

        // Subscribe before we start reading so we don't miss any events
        // emitted while the client-side setup was in flight.
        let mut rx = self.broadcast_audit.subscribe();

        let started = std::time::Instant::now();
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut exit_code: i32 = 0;

        // The loop exits only in three ways:
        //   * `break` after forwarding the target's terminal event — falls
        //     through to the end-frame write below.
        //   * `return` on `RecvError::Closed` — channel died; no end frame,
        //     the client sees EOF and infers an abnormal close.
        //   * `return` on a write failure — client disconnected; nothing
        //     more to do.
        loop {
            match rx.recv().await {
                Ok(event) => {
                    // Filter: exact match on target. No descendant walk —
                    // `agentd logs <id>` shows only the named agent.
                    if event.agent_id != target {
                        continue;
                    }
                    // Aggregate usage (consistent with submit_streaming).
                    if let AuditEventKind::UsageReported {
                        input_tokens: i,
                        output_tokens: o,
                    } = &event.event
                    {
                        input_tokens = input_tokens.saturating_add(*i);
                        output_tokens = output_tokens.saturating_add(*o);
                        continue;
                    }

                    let frame = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                    let frame = json!({ "kind": "event", "event": frame });
                    if write_ndjson(writer, &frame).await.is_err() {
                        return;
                    }

                    match &event.event {
                        AuditEventKind::AgentExecutionCompleted { .. } => break,
                        AuditEventKind::AgentLoopStopped { reason, .. } => {
                            if reason == "error" || reason == "budget_exceeded" {
                                exit_code = 1;
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    let frame = json!({ "kind": "lag", "missed": n });
                    let _ = write_ndjson(writer, &frame).await;
                }
                Err(RecvError::Closed) => {
                    // Channel died under us. Not a clean termination — no
                    // end frame. Client sees EOF on the socket.
                    return;
                }
            }
        }

        let end = json!({
            "kind": "end",
            "exit_code": exit_code,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        });
        let _ = write_ndjson(writer, &end).await;
    }

    /// Walk from `event_agent` upward via `parent_agent` until root. Returns
    /// true if any ancestor (or the node itself) is `bootstrap`.
    fn event_in_subtree(
        &self,
        event_agent: aaos_core::AgentId,
        bootstrap: aaos_core::AgentId,
    ) -> bool {
        let mut cur = Some(event_agent);
        // Guard against pathological cycles (shouldn't happen, but a bad
        // parent_agent link would otherwise spin forever).
        for _ in 0..1024 {
            let Some(id) = cur else { return false };
            if id == bootstrap {
                return true;
            }
            cur = self
                .registry
                .get_info(id)
                .ok()
                .and_then(|info| info.parent_agent);
        }
        false
    }

    /// Ensure a Bootstrap agent is running. If one is already in the registry
    /// (by manifest name "bootstrap", in Running state), return its id.
    /// Otherwise load `/etc/aaos/manifests/bootstrap.yaml` and spawn it.
    pub(crate) async fn ensure_bootstrap_running(&self) -> anyhow::Result<aaos_core::AgentId> {
        for info in self.registry.list() {
            if info.name == "bootstrap" && info.state == AgentState::Running {
                return Ok(info.id);
            }
        }

        // Test-only override so the streaming-test harness can point at a
        // pre-seeded bootstrap manifest without needing a file at the
        // production install path.
        let manifest_path = std::env::var("AAOS_BOOTSTRAP_MANIFEST_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/etc/aaos/manifests/bootstrap.yaml"));

        // Read the YAML so we can reuse `spawn_from_yaml_with_id`, which
        // handles the full spawn-and-launch path for persistent agents
        // (identical to what `handle_agent_spawn` does for user-submitted
        // manifests). Using a fresh UUID here — if a stable-ID policy is
        // needed later it can flow in via env (matches main.rs's
        // load_or_create_bootstrap_id behavior).
        let yaml = std::fs::read_to_string(&manifest_path).map_err(|e| {
            anyhow::anyhow!(
                "cannot read bootstrap manifest at {}: {}",
                manifest_path.display(),
                e
            )
        })?;

        let id = aaos_core::AgentId::new();
        let resp = self
            .spawn_from_yaml_with_id(&yaml, serde_json::Value::Null, Some(id))
            .await;
        if let Some(err) = resp.error {
            return Err(anyhow::anyhow!("spawn bootstrap failed: {}", err.message));
        }
        Ok(id)
    }

    /// Deliver a goal message to an already-running agent via the router.
    /// Mirrors the persistent-agent branch of `handle_agent_run`.
    pub(crate) async fn route_goal_to(
        &self,
        target: aaos_core::AgentId,
        goal: &str,
    ) -> anyhow::Result<()> {
        let msg =
            aaos_ipc::McpMessage::new(target, target, "agent.run", json!({ "message": goal }));
        self.router
            .route(msg)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    /// Start listening on a Unix socket.
    pub async fn listen(self: Arc<Self>, socket_path: &Path) -> anyhow::Result<()> {
        // Remove stale socket
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)?;
        // Socket mode 0660: owner (aaos) + group (aaos) can read/write, others
        // can't connect. Operators opt into access via `adduser $USER aaos`.
        // Without this chmod the socket inherits the process umask and becomes
        // 0755-ish, which lets `stat` succeed but blocks `connect(2)` for
        // group members (connect requires write on the socket inode).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o660);
            if let Err(e) = std::fs::set_permissions(socket_path, perms) {
                tracing::warn!(error = %e, "failed to chmod socket to 0660; group members may not connect");
            }
        }
        tracing::info!(path = %socket_path.display(), "listening on unix socket");

        loop {
            let (stream, _) = listener.accept().await?;
            let server = self.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();

                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break, // Connection closed
                        Ok(_) => {
                            match serde_json::from_str::<crate::api::JsonRpcRequest>(&line) {
                                Ok(request) => {
                                    if STREAMING_METHODS.contains(&request.method.as_str()) {
                                        // Streaming handler owns the writer for the rest of
                                        // this connection. After it returns, we close.
                                        server.handle_streaming(&request, &mut writer).await;
                                        break;
                                    }
                                    let response = server.handle_request(&request).await;
                                    let mut resp_bytes = serde_json::to_vec(&response).unwrap();
                                    resp_bytes.push(b'\n');
                                    if writer.write_all(&resp_bytes).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    let response = JsonRpcResponse::error(
                                        serde_json::Value::Null,
                                        crate::api::PARSE_ERROR,
                                        e.to_string(),
                                    );
                                    let mut resp_bytes = serde_json::to_vec(&response).unwrap();
                                    resp_bytes.push(b'\n');
                                    if writer.write_all(&resp_bytes).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::JsonRpcRequest;

    /// Tests that mutate the process-global `AAOS_ROLES_DIR` env var must
    /// hold this mutex. Cargo runs lib tests multi-threaded by default;
    /// two tests writing different values of `AAOS_ROLES_DIR` race and
    /// observe each other's writes, which breaks both.
    static ROLES_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn make_request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            method: method.to_string(),
            params,
        }
    }

    #[tokio::test]
    async fn spawn_and_list() {
        let server = Server::new();
        let manifest = r#"
name: test-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
"#;
        let resp = server
            .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
            .await;
        assert!(resp.result.is_some());
        let agent_id = resp.result.unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = server
            .handle_request(&make_request("agent.list", json!({})))
            .await;
        let agents = resp.result.unwrap()["agents"].as_array().unwrap().clone();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["id"].as_str().unwrap(), agent_id);
    }

    #[tokio::test]
    async fn tool_list() {
        let server = Server::new();
        let resp = server
            .handle_request(&make_request("tool.list", json!({})))
            .await;
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        assert!(!tools.is_empty());
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"web_fetch"));
    }

    #[tokio::test]
    async fn unknown_method() {
        let server = Server::new();
        let resp = server
            .handle_request(&make_request("nonexistent", json!({})))
            .await;
        assert!(resp.error.is_some());
    }

    #[tokio::test]
    async fn tool_invoke_with_capability() {
        let server = Server::new();
        let manifest = r#"
name: tool-test
model: claude-haiku-4-5-20251001
system_prompt: "test"
capabilities:
  - "tool: echo"
"#;
        let resp = server
            .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
            .await;
        let agent_id = resp.result.unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = server
            .handle_request(&make_request(
                "tool.invoke",
                json!({"agent_id": agent_id, "tool": "echo", "input": {"message": "hello"}}),
            ))
            .await;
        assert!(resp.result.is_some());
        assert_eq!(resp.result.unwrap()["result"], json!({"message": "hello"}));
    }

    #[tokio::test]
    async fn tool_invoke_without_capability() {
        let server = Server::new();
        let manifest = r#"
name: no-tools
model: claude-haiku-4-5-20251001
system_prompt: "test"
capabilities:
  - web_search
"#;
        let resp = server
            .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
            .await;
        let agent_id = resp.result.unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = server
            .handle_request(&make_request(
                "tool.invoke",
                json!({"agent_id": agent_id, "tool": "echo", "input": {"message": "hello"}}),
            ))
            .await;
        assert!(resp.error.is_some());
    }

    use aaos_core::TokenUsage;
    use aaos_llm::{
        CompletionRequest, CompletionResponse, ContentBlock, LlmClient, LlmResult, LlmStopReason,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct MockLlm {
        responses: Mutex<Vec<LlmResult<CompletionResponse>>>,
    }

    impl MockLlm {
        fn text(text: &str) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(vec![Ok(CompletionResponse {
                    content: vec![ContentBlock::Text { text: text.into() }],
                    stop_reason: LlmStopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                })]),
            })
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        fn max_context_tokens(&self, _model: &str) -> u32 {
            200_000
        }

        async fn complete(&self, _req: CompletionRequest) -> LlmResult<CompletionResponse> {
            self.responses.lock().unwrap().remove(0)
        }
    }

    /// LLM client that never returns — its `complete` future hangs forever.
    /// Used by `submit_streaming_writes_events_then_end_frame` to pin a
    /// persistent agent into an idle state so directly-injected audit
    /// events are the only signal reaching the streaming handler.
    struct HangingLlm;

    #[async_trait]
    impl LlmClient for HangingLlm {
        fn max_context_tokens(&self, _model: &str) -> u32 {
            200_000
        }

        async fn complete(&self, _req: CompletionRequest) -> LlmResult<CompletionResponse> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn agent_spawn_and_run() {
        let server = Server::with_llm_client(MockLlm::text("I'm alive!"));
        let manifest = r#"
name: runner
model: claude-haiku-4-5-20251001
system_prompt: "You are helpful."
capabilities:
  - "tool: echo"
"#;
        let resp = server
            .handle_request(&make_request(
                "agent.spawn_and_run",
                json!({"manifest": manifest, "message": "Hello"}),
            ))
            .await;
        let result = resp.result.unwrap();
        assert!(result.get("agent_id").is_some());
        assert_eq!(result["response"], "I'm alive!");
        assert_eq!(result["stop_reason"], "complete");
        assert_eq!(result["iterations"], 1);
    }

    #[tokio::test]
    async fn agent_run_existing() {
        let server = Server::with_llm_client(MockLlm::text("Running!"));
        let manifest = r#"
name: existing
model: claude-haiku-4-5-20251001
system_prompt: "You are helpful."
"#;
        let resp = server
            .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
            .await;
        let agent_id = resp.result.unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = server
            .handle_request(&make_request(
                "agent.run",
                json!({"agent_id": agent_id, "message": "Do something"}),
            ))
            .await;
        let result = resp.result.unwrap();
        assert_eq!(result["response"], "Running!");
    }

    #[tokio::test]
    async fn tool_invoke_nonexistent_agent() {
        let server = Server::new();
        let resp = server
            .handle_request(&make_request(
                "tool.invoke",
                json!({"agent_id": "00000000-0000-0000-0000-000000000000", "tool": "echo", "input": {}}),
            ))
            .await;
        assert!(resp.error.is_some());
    }

    #[tokio::test]
    async fn approval_list_empty() {
        let server = Server::new();
        let resp = server
            .handle_request(&make_request("approval.list", json!({})))
            .await;
        let result = resp.result.unwrap();
        assert_eq!(result["pending"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn submit_streaming_writes_events_then_end_frame() {
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let _guard = ROLES_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("agentd.sock");
        let manifest_path = tmp.path().join("bootstrap.yaml");
        std::fs::write(
            &manifest_path,
            r#"
name: bootstrap
model: claude-haiku-4-5-20251001
system_prompt: "bootstrap test"
lifecycle: persistent
"#,
        )
        .unwrap();

        // Point ensure_bootstrap_running at our temp manifest.
        // SAFETY: test-only, and each test runs in its own tokio runtime but
        // the env var is process-global. The code reads it once per call so
        // races are benign.
        std::env::set_var(
            "AAOS_BOOTSTRAP_MANIFEST_PATH",
            manifest_path.to_string_lossy().to_string(),
        );

        // Isolate from host role catalog — without this, a host with a real
        // /etc/aaos/roles/ directory (e.g. a Debian box with the .deb
        // installed) causes load_role_catalog to build a PlanExecutor, and
        // handle_submit_streaming takes the PlanExecutor branch instead of
        // the Bootstrap branch this test asserts against. Surfaced by the
        // 2026-04-17 self-build run on a DO droplet where /etc/aaos/roles/
        // was populated.
        //
        // Point at a nonexistent subdir of the tempdir (not just an empty
        // dir): load_from_dir on an empty dir returns Ok(empty_catalog) and
        // still wires a PlanExecutor; we need the load to fail so
        // plan_executor stays None.
        let absent_roles = tmp.path().join("no-such-roles-dir");
        std::env::set_var("AAOS_ROLES_DIR", absent_roles.to_string_lossy().to_string());

        // Use a hanging LLM client so the persistent bootstrap agent launches
        // but never actually completes execution on its own. That gives our
        // directly-injected audit events deterministic ordering — only they
        // reach the subscriber, never racing against a real AgentExecutionCompleted
        // emitted by the agent loop.
        let hanging: Arc<dyn LlmClient> = Arc::new(HangingLlm);
        let server = Server::with_llm_client(hanging);
        let audit = server.broadcast_audit.clone();

        let server_for_listen = server.clone();
        let socket_path_for_listen = socket_path.clone();
        let listener_task = tokio::spawn(async move {
            let _ = server_for_listen.listen(&socket_path_for_listen).await;
        });

        // Wait for socket to appear.
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let mut client = UnixStream::connect(&socket_path).await.unwrap();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "agent.submit_streaming",
            "params": { "goal": "test goal" }
        });
        let mut line = serde_json::to_vec(&req).unwrap();
        line.push(b'\n');
        client.write_all(&line).await.unwrap();
        client.flush().await.unwrap();

        // Give the server a moment to subscribe and ensure_bootstrap_running,
        // then emit audit events tagged with the real bootstrap id.
        let registry = server.registry.clone();
        let emitter = tokio::spawn(async move {
            // Poll the registry for the bootstrap agent the server just spawned.
            let bid = {
                let mut found = None;
                for _ in 0..100 {
                    if let Some(info) = registry.list().into_iter().find(|i| i.name == "bootstrap")
                    {
                        found = Some(info.id);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                found.expect("bootstrap agent registered")
            };

            // Small extra delay so the server's subscribe() is definitely live.
            tokio::time::sleep(Duration::from_millis(50)).await;

            use aaos_core::{AuditEvent, AuditEventKind, AuditLog};
            audit.record(AuditEvent::new(
                bid,
                AuditEventKind::ToolInvoked {
                    tool: "file_write".into(),
                    input_hash: "h".into(),
                    args_preview: None,
                    execution_surface: aaos_core::ToolExecutionSurface::Daemon,
                },
            ));
            audit.record(AuditEvent::new(
                bid,
                AuditEventKind::UsageReported {
                    input_tokens: 100,
                    output_tokens: 50,
                },
            ));
            audit.record(AuditEvent::new(
                bid,
                AuditEventKind::AgentExecutionCompleted {
                    stop_reason: "done".into(),
                    total_iterations: 1,
                },
            ));
        });

        let (reader, _writer) = client.split();
        let mut lines = BufReader::new(reader).lines();
        let mut frames: Vec<serde_json::Value> = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(Some(text)) = lines.next_line().await {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    let is_end = v.get("kind").and_then(|k| k.as_str()) == Some("end");
                    frames.push(v);
                    if is_end {
                        break;
                    }
                }
            }
        })
        .await
        .expect("should receive end frame within 5s");

        emitter.await.unwrap();
        listener_task.abort();

        // Clean up env vars so other tests aren't affected.
        std::env::remove_var("AAOS_BOOTSTRAP_MANIFEST_PATH");
        std::env::remove_var("AAOS_ROLES_DIR");

        let end = frames.last().unwrap();
        assert_eq!(end["kind"], "end");
        assert_eq!(end["exit_code"], 0);
        assert_eq!(end["input_tokens"], 100);
        assert_eq!(end["output_tokens"], 50);
        assert!(end.get("elapsed_ms").is_some(), "elapsed_ms present");

        // UsageReported is aggregated, not forwarded.
        let has_usage = frames.iter().any(|f| {
            f.get("event")
                .and_then(|e| e.get("event"))
                .and_then(|inner| inner.get("kind"))
                .and_then(|k| k.as_str())
                == Some("usage_reported")
        });
        assert!(
            !has_usage,
            "UsageReported should be aggregated, not forwarded"
        );

        // ToolInvoked should be forwarded as an event frame.
        let has_tool = frames.iter().any(|f| {
            f.get("event")
                .and_then(|e| e.get("event"))
                .and_then(|inner| inner.get("kind"))
                .and_then(|k| k.as_str())
                == Some("tool_invoked")
        });
        assert!(has_tool, "ToolInvoked should be forwarded");
    }

    #[tokio::test]
    async fn logs_streaming_filters_to_single_agent() {
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("agentd.sock");

        // Two agents registered side-by-side in one server. We don't need
        // them to actually run any LLM loop — `handle_logs_streaming` only
        // consults `registry.get_info` for existence and then subscribes to
        // the broadcast audit sink. A non-persistent manifest with the
        // hanging LLM never spawns a background task, which keeps test
        // teardown tidy.
        let hanging: Arc<dyn LlmClient> = Arc::new(HangingLlm);
        let server = Server::with_llm_client(hanging);

        let target_manifest = aaos_core::AgentManifest::from_yaml(
            r#"
name: target
model: claude-haiku-4-5-20251001
system_prompt: "target"
"#,
        )
        .unwrap();
        let other_manifest = aaos_core::AgentManifest::from_yaml(
            r#"
name: other
model: claude-haiku-4-5-20251001
system_prompt: "other"
"#,
        )
        .unwrap();
        let target_id = server.registry.spawn(target_manifest).unwrap();
        let other_id = server.registry.spawn(other_manifest).unwrap();

        let server_for_listen = server.clone();
        let socket_path_for_listen = socket_path.clone();
        let listener_task = tokio::spawn(async move {
            let _ = server_for_listen.listen(&socket_path_for_listen).await;
        });

        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let mut client = UnixStream::connect(&socket_path).await.unwrap();
        let req = json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "agent.logs_streaming",
            "params": { "agent_id": target_id.to_string() }
        });
        let mut line = serde_json::to_vec(&req).unwrap();
        line.push(b'\n');
        client.write_all(&line).await.unwrap();
        client.flush().await.unwrap();

        let audit = server.broadcast_audit.clone();
        let emitter = tokio::spawn(async move {
            // Small delay so the server has subscribed before we start.
            tokio::time::sleep(Duration::from_millis(100)).await;
            use aaos_core::{AuditEvent, AuditEventKind, AuditLog};
            audit.record(AuditEvent::new(
                target_id,
                AuditEventKind::ToolInvoked {
                    tool: "web_fetch".into(),
                    input_hash: "h1".into(),
                    args_preview: None,
                    execution_surface: aaos_core::ToolExecutionSurface::Daemon,
                },
            ));
            audit.record(AuditEvent::new(
                other_id,
                AuditEventKind::ToolInvoked {
                    tool: "file_write".into(),
                    input_hash: "h2".into(),
                    args_preview: None,
                    execution_surface: aaos_core::ToolExecutionSurface::Daemon,
                },
            ));
            audit.record(AuditEvent::new(
                target_id,
                AuditEventKind::ToolInvoked {
                    tool: "file_read".into(),
                    input_hash: "h3".into(),
                    args_preview: None,
                    execution_surface: aaos_core::ToolExecutionSurface::Daemon,
                },
            ));
            audit.record(AuditEvent::new(
                target_id,
                AuditEventKind::AgentExecutionCompleted {
                    stop_reason: "done".into(),
                    total_iterations: 1,
                },
            ));
        });

        let (reader, _writer) = client.split();
        let mut lines = BufReader::new(reader).lines();
        let mut frames: Vec<serde_json::Value> = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(Some(text)) = lines.next_line().await {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    let is_end = v.get("kind").and_then(|k| k.as_str()) == Some("end");
                    frames.push(v);
                    if is_end {
                        break;
                    }
                }
            }
        })
        .await
        .expect("end frame within 5s");

        emitter.await.unwrap();
        listener_task.abort();

        // 3 event frames (2 ToolInvoked + 1 AgentExecutionCompleted, all for
        // target) + 1 end frame = 4 total. The other agent's event must NOT
        // appear.
        let event_frames: Vec<_> = frames
            .iter()
            .filter(|f| f.get("kind").and_then(|k| k.as_str()) == Some("event"))
            .collect();
        assert_eq!(
            event_frames.len(),
            3,
            "expected 3 event frames for target, got {}: {:?}",
            event_frames.len(),
            frames
        );

        for f in &event_frames {
            let aid = f["event"]["agent_id"].as_str().unwrap();
            assert_eq!(
                aid,
                target_id.to_string(),
                "event frame not filtered to target"
            );
        }

        let end = frames.last().unwrap();
        assert_eq!(end["kind"], "end");
        assert_eq!(end["exit_code"], 0);
    }

    #[tokio::test]
    async fn logs_streaming_missing_agent_id_emits_end_with_exit_2() {
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("agentd.sock");
        let server = Arc::new(Server::new());
        let server_for_listen = server.clone();
        let socket_path_for_listen = socket_path.clone();
        let listener_task = tokio::spawn(async move {
            let _ = server_for_listen.listen(&socket_path_for_listen).await;
        });
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let mut client = UnixStream::connect(&socket_path).await.unwrap();
        let req = json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "agent.logs_streaming",
            "params": {}
        });
        let mut line = serde_json::to_vec(&req).unwrap();
        line.push(b'\n');
        client.write_all(&line).await.unwrap();
        client.flush().await.unwrap();

        let (reader, _writer) = client.split();
        let mut lines = BufReader::new(reader).lines();
        let mut frames: Vec<serde_json::Value> = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(Some(text)) = lines.next_line().await {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    frames.push(v);
                    break;
                }
            }
        })
        .await
        .expect("frame within 5s");

        listener_task.abort();

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["kind"], "end");
        assert_eq!(frames[0]["exit_code"], 2);
        assert!(frames[0]["error"]
            .as_str()
            .unwrap_or("")
            .contains("agent_id"));
    }

    #[tokio::test]
    async fn persistent_agent_run_returns_trace_id() {
        let server = Server::with_llm_client(MockLlm::text("Persistent response"));
        let manifest = r#"
name: persistent-test
model: claude-haiku-4-5-20251001
system_prompt: "You are persistent."
lifecycle: persistent
"#;
        let resp = server
            .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
            .await;
        let agent_id = resp.result.unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = server
            .handle_request(&make_request(
                "agent.run",
                json!({"agent_id": agent_id, "message": "Hello persistent"}),
            ))
            .await;
        let result = resp.result.unwrap();
        assert!(result.get("trace_id").is_some());
        assert_eq!(result["status"], "delivered");
    }

    #[tokio::test]
    async fn run_subtask_inline_spawns_child_and_returns_response_text() {
        // End-to-end exercise of the Task 12 plumbing: the SubtaskRunner
        // closure calls into run_subtask_inline, which must parse the
        // manifest, spawn an ephemeral child via registry.spawn(), run
        // its LLM loop against the mock client, and return the final
        // assistant text via last_assistant_text().
        let server = Server::with_llm_client(MockLlm::text("subtask ok"));
        let yaml = r#"
name: t-subtask
model: claude-haiku-4-5-20251001
system_prompt: "test subtask"
"#;
        let result = server
            .run_subtask_inline(
                "t1",
                yaml,
                "hello",
                SubtaskExecutorOverrides::default(),
                None,
                std::path::PathBuf::from("/tmp/test-run-root"),
            )
            .await
            .expect("subtask should run to completion");
        assert_eq!(result.subtask_id, "t1");
        assert_eq!(result.response, "subtask ok");
        // Tokens aren't aggregated yet — this is expected zero today.
        assert_eq!(result.input_tokens, 0);
        assert_eq!(result.output_tokens, 0);
        // The scopeguard should have stopped the child; it must no
        // longer appear in the registry's running list.
        let still_running = server
            .registry
            .list()
            .into_iter()
            .any(|info| info.id == result.agent_id && info.state == AgentState::Running);
        assert!(!still_running, "scopeguard must stop the subtask child");
    }

    #[tokio::test]
    async fn submit_streaming_uses_plan_executor_when_catalog_loaded() {
        // A Server built with AAOS_ROLES_DIR pointing at a valid roles
        // dir must have `plan_executor` populated — that's the hook
        // `handle_submit_streaming` checks before taking the PlanExecutor
        // path. Without a loaded catalog the OnceLock stays empty and
        // the handler falls through to the legacy Bootstrap path.
        let _guard = ROLES_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp_roles = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_roles.path().join("generalist.yaml"),
            r#"
name: generalist
model: deepseek-chat
parameters:
  task_description:
    type: string
    required: true
    description: free-form
capabilities: []
system_prompt: "x"
message_template: "{task_description}"
budget: { max_input_tokens: 1000, max_output_tokens: 500 }
retry: { max_attempts: 1, on: [] }
"#,
        )
        .unwrap();

        std::env::set_var("AAOS_ROLES_DIR", temp_roles.path());
        let server = Server::with_llm_client(MockLlm::text("ok"));
        assert!(
            server.plan_executor.get().is_some(),
            "plan_executor should be built when AAOS_ROLES_DIR has valid roles"
        );
        std::env::remove_var("AAOS_ROLES_DIR");
    }
}

/// Write one NDJSON frame (JSON value + `\n`) to the connection writer.
async fn write_ndjson<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &serde_json::Value,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut line = serde_json::to_vec(frame).unwrap_or_else(|_| b"null".to_vec());
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// Discover skills from standard AgentSkills paths + AAOS_SKILLS_DIR env var.
fn discover_all_skills() -> Vec<aaos_core::Skill> {
    let mut all_skills = Vec::new();
    for skills_dir in &["/etc/aaos/skills", "/var/lib/aaos/skills"] {
        let path = std::path::Path::new(skills_dir);
        if path.is_dir() {
            all_skills.extend(aaos_core::discover_skills(path));
        }
    }
    if let Ok(extra) = std::env::var("AAOS_SKILLS_DIR") {
        for dir in extra.split(':') {
            let path = std::path::Path::new(dir);
            if path.is_dir() {
                all_skills.extend(aaos_core::discover_skills(path));
            }
        }
    }
    if !all_skills.is_empty() {
        tracing::info!(count = all_skills.len(), "skills loaded");
    }
    all_skills
}

/// Create the memory store backend: SQLite if AAOS_MEMORY_DB is set, in-memory otherwise.
fn create_memory_store(embedding_model: &str) -> Arc<dyn aaos_memory::MemoryStore> {
    if let Ok(db_path) = std::env::var("AAOS_MEMORY_DB") {
        match aaos_memory::SqliteMemoryStore::open(&PathBuf::from(&db_path)) {
            Ok(store) => {
                tracing::info!(path = %db_path, "persistent memory store (SQLite)");
                return Arc::new(store);
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %db_path, "failed to open SQLite memory, falling back to in-memory");
            }
        }
    }
    tracing::info!("using in-memory memory store (non-persistent)");
    Arc::new(aaos_memory::InMemoryMemoryStore::new(
        10_000,
        768,
        embedding_model,
    ))
}
