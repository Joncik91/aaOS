//! PlanExecutor — deterministic DAG walk that spawns children per role.
//!
//! Subtasks are spawned via a `SubtaskRunner` closure — the server
//! constructs one that closes over its AgentRegistry + services and passes
//! it in. This keeps the plan module decoupled from agentd's concrete
//! service wiring.
//!
//! Execution shape: `topo_batches` splits the plan into dependency-ordered
//! batches. Within a batch, all subtasks are spawned concurrently via
//! `futures::try_join_all`. Batches run sequentially.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aaos_core::{AgentId, AuditEvent, AuditEventKind, AuditLog, CoreError};

use crate::plan::{
    topo_batches, Plan, PlanResult, Planner, PlannerError, RoleCatalog, Substitutions, Subtask,
    SubtaskId, SubtaskResult,
};

/// Sentinel error-message string used by `race_deadline` to signal a TTL
/// wall-clock expiry back to `spawn_subtask`. Extracted to a const so a
/// typo on either side is caught at compile time. A future refactor
/// should promote this to a proper `CoreError::TtlExpired` variant.
const TTL_WALL_CLOCK_SENTINEL: &str = "ttl wall-clock exceeded";

/// Per-subtask executor overrides derived from the role's `budget` + `retry`
/// fields. The runner uses these to build a non-default `ExecutorConfig`
/// instead of swallowing the role author's intent (which was the primary
/// driver of the 2026-04-17 fetcher-stall bug — role.budget.max_output_tokens
/// never reached the LLM call).
#[derive(Debug, Clone, Copy)]
pub struct SubtaskExecutorOverrides {
    /// Cap on output tokens per LLM call. Usually role.budget.max_output_tokens.
    pub max_output_tokens: u32,
    /// Cap on LLM-loop iterations for this child.
    pub max_iterations: u32,
}

impl Default for SubtaskExecutorOverrides {
    fn default() -> Self {
        Self {
            max_output_tokens: 16_384,
            max_iterations: 50,
        }
    }
}

/// Closure that spawns a child from a rendered manifest + message, runs it
/// to completion, and returns the SubtaskResult. Provided by the server so
/// the executor doesn't have to re-wire services.
pub type SubtaskRunner = Arc<
    dyn Fn(
            String,                   // subtask_id (for audit correlation)
            String,                   // rendered manifest YAML
            String,                   // first message for the child
            SubtaskExecutorOverrides, // per-role budget + iteration caps
            Option<Instant>,          // wall-clock deadline (None = no wall-clock bound)
        ) -> Pin<Box<dyn Future<Output = Result<SubtaskResult, CoreError>> + Send>>
        + Send
        + Sync,
>;

/// Closure that runs a deterministic scaffold for roles whose work is
/// mechanical (no LLM loop). Dispatched by PlanExecutor when `role.scaffold`
/// is Some. Kind (e.g. "fetcher") selects which scaffold to run; resolved
/// params carry the same substituted values the LLM path would have seen.
///
/// Scaffolds produce a SubtaskResult with a real workspace-path response
/// (or similar mechanical output) and zero LLM token usage — since no LLM
/// ran. Errors are Terminal; correctable-class errors (bad params, unknown
/// role) are caught upstream in spawn_subtask.
pub type ScaffoldRunner = Arc<
    dyn Fn(
            String,            // subtask_id
            String,            // scaffold kind (e.g. "fetcher")
            serde_json::Value, // resolved params
        ) -> Pin<Box<dyn Future<Output = Result<SubtaskResult, CoreError>> + Send>>
        + Send
        + Sync,
>;

pub struct PlanExecutor {
    catalog: Arc<RoleCatalog>,
    planner: Arc<Planner>,
    runner: SubtaskRunner,
    scaffold_runner: Option<ScaffoldRunner>,
    audit_log: Arc<dyn AuditLog>,
    max_replans: u32,
    total_deadline: Duration,
    run_root_base: PathBuf,
}

impl PlanExecutor {
    pub fn new(
        catalog: Arc<RoleCatalog>,
        planner: Arc<Planner>,
        runner: SubtaskRunner,
        audit_log: Arc<dyn AuditLog>,
        run_root_base: PathBuf,
    ) -> Self {
        Self {
            catalog,
            planner,
            runner,
            scaffold_runner: None,
            audit_log,
            max_replans: 3,
            total_deadline: Duration::from_secs(600),
            run_root_base,
        }
    }

    /// Install a scaffold runner for roles with `scaffold: {kind: ...}`.
    /// Without one, all roles run through the LLM `runner` regardless of
    /// their `scaffold` field. The server wires this in after the Arc<Self>
    /// exists, same chicken-and-egg dance as `install_plan_executor_runner`.
    pub fn set_scaffold_runner(&mut self, runner: ScaffoldRunner) {
        self.scaffold_runner = Some(runner);
    }

    pub async fn run(&self, goal: &str, run_id: uuid::Uuid) -> Result<PlanResult, ExecutorError> {
        let started = Instant::now();
        let run_root = self.run_root_base.join(run_id.to_string());
        std::fs::create_dir_all(&run_root).map_err(|e| {
            ExecutorError::Terminal(CoreError::Ipc(format!(
                "create workspace {}: {}",
                run_root.display(),
                e
            )))
        })?;

        let mut plan = match self.planner.plan(goal, &self.catalog).await {
            Ok(p) => p,
            Err(PlannerError::Malformed(_)) => fallback_generalist_plan(goal, &self.catalog)?,
            Err(e) => return Err(ExecutorError::from(e)),
        };
        self.write_plan_json(&run_root, &plan)?;

        let mut replans_used: u32 = 0;
        loop {
            if started.elapsed() > self.total_deadline {
                return Err(ExecutorError::Terminal(CoreError::Ipc(
                    "planner deadline exhausted".into(),
                )));
            }
            match self.execute_plan(&plan, &run_root).await {
                Ok(result) => {
                    self.audit_log.record(AuditEvent::new(
                        AgentId::from_uuid(uuid::Uuid::nil()),
                        AuditEventKind::PlanProduced {
                            subtask_count: plan.subtasks.len() as u32,
                            replans_used,
                        },
                    ));
                    return Ok(result);
                }
                Err(ExecutorError::Correctable(reason)) if replans_used < self.max_replans => {
                    self.audit_log.record(AuditEvent::new(
                        AgentId::from_uuid(uuid::Uuid::nil()),
                        AuditEventKind::PlanReplanned {
                            reason: reason.clone(),
                        },
                    ));
                    plan = self
                        .planner
                        .replan(goal, &self.catalog, &plan, &reason)
                        .await
                        .map_err(ExecutorError::from)?;
                    self.write_plan_json(&run_root, &plan)?;
                    replans_used += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn write_plan_json(&self, run_root: &PathBuf, plan: &Plan) -> Result<(), ExecutorError> {
        let path = run_root.join("plan.json");
        let json = serde_json::to_string_pretty(plan).unwrap();
        std::fs::write(&path, json).map_err(|e| {
            ExecutorError::Terminal(CoreError::Ipc(format!("write {}: {}", path.display(), e)))
        })
    }

    pub async fn execute_plan(
        &self,
        plan: &Plan,
        run_root: &PathBuf,
    ) -> Result<PlanResult, ExecutorError> {
        use futures::future::join_all;

        let subs = Substitutions::new(run_root.clone());
        let batches = topo_batches(plan).map_err(ExecutorError::Correctable)?;

        // Pre-validate all subtasks BEFORE spawning anything.
        for s in &plan.subtasks {
            let role = self
                .catalog
                .get(&s.role)
                .ok_or_else(|| ExecutorError::Correctable(format!("unknown role: {}", s.role)))?;
            let resolved = subs.apply(&s.params);
            role.validate_params(&resolved)
                .map_err(ExecutorError::Correctable)?;
        }

        let mut results: HashMap<SubtaskId, SubtaskResult> = HashMap::new();

        for batch in batches {
            // join_all, not try_join_all: siblings finish so we can audit
            // every outcome. A single failure converts to Correctable AFTER
            // the batch drains, so the outer run() loop can replan with
            // full per-subtask context.
            let spawns = batch
                .iter()
                .map(|s| self.spawn_subtask(s, &subs))
                .collect::<Vec<_>>();
            let batch_results = join_all(spawns).await;

            let mut first_failure: Option<String> = None;
            for (subtask, result) in batch.iter().zip(batch_results) {
                match result {
                    Ok(r) => {
                        // Role-contract guard: any "file_write: {<param>}" grant
                        // without a trailing `/*` is a declared single-path
                        // output — the subtask is not complete unless that file
                        // exists. Catches run-12 shape where a builder agent
                        // emits "complete" having skipped the report write.
                        if let Some(reason) =
                            check_declared_outputs_exist(&self.catalog, subtask, &subs)
                        {
                            self.audit_log.record(AuditEvent::new(
                                r.agent_id,
                                AuditEventKind::SubtaskCompleted {
                                    subtask_id: subtask.id.clone(),
                                    success: false,
                                },
                            ));
                            if first_failure.is_none() {
                                first_failure = Some(format!(
                                    "subtask '{}' (role '{}') did not produce its declared output: {}",
                                    subtask.id, subtask.role, reason
                                ));
                            }
                            continue;
                        }
                        self.audit_log.record(AuditEvent::new(
                            r.agent_id,
                            AuditEventKind::SubtaskCompleted {
                                subtask_id: subtask.id.clone(),
                                success: true,
                            },
                        ));
                        results.insert(subtask.id.clone(), r);
                    }
                    Err(e) => {
                        self.audit_log.record(AuditEvent::new(
                            AgentId::from_uuid(uuid::Uuid::nil()),
                            AuditEventKind::SubtaskCompleted {
                                subtask_id: subtask.id.clone(),
                                success: false,
                            },
                        ));
                        if first_failure.is_none() {
                            first_failure = Some(format!(
                                "subtask '{}' (role '{}') failed: {}",
                                subtask.id, subtask.role, e
                            ));
                        }
                    }
                }
            }

            if let Some(reason) = first_failure {
                return Err(ExecutorError::Correctable(reason));
            }
        }

        Ok(PlanResult {
            plan: plan.clone(),
            results,
            final_output: plan.final_output.clone(),
        })
    }

    async fn spawn_subtask(
        &self,
        subtask: &Subtask,
        subs: &Substitutions,
    ) -> Result<SubtaskResult, ExecutorError> {
        let role = self
            .catalog
            .get(&subtask.role)
            .ok_or_else(|| ExecutorError::Correctable(format!("unknown role: {}", subtask.role)))?;
        let resolved_params = subs.apply(&subtask.params);

        // TTL hop check. If the subtask arrives with max_hops=0 we've
        // exhausted the budget — emit the audit event, skip execution, return
        // a Correctable error so the plan executor marks it failed + cascades.
        if let Some(ttl) = &subtask.ttl {
            if let Some(hops) = ttl.max_hops {
                if hops == 0 {
                    self.audit_log.record(AuditEvent::new(
                        AgentId::from_uuid(uuid::Uuid::nil()),
                        AuditEventKind::SubtaskTtlExpired {
                            subtask_id: subtask.id.clone(),
                            reason: "hops_exhausted".into(),
                        },
                    ));
                    return Err(ExecutorError::Correctable(format!(
                        "subtask '{}' TTL hops exhausted",
                        subtask.id
                    )));
                }
            }
        }

        let wall_clock_deadline: Option<Instant> = subtask
            .ttl
            .as_ref()
            .and_then(|t| t.max_wall_clock)
            .map(|d| Instant::now() + d);

        // Audit: subtask start. agent_id isn't known until the runner returns.
        self.audit_log.record(AuditEvent::new(
            AgentId::from_uuid(uuid::Uuid::nil()),
            AuditEventKind::SubtaskStarted {
                subtask_id: subtask.id.clone(),
                role: subtask.role.clone(),
            },
        ));

        // Scaffold dispatch: if the role opts into deterministic execution
        // AND the server has installed a scaffold_runner, skip the LLM loop
        // entirely. Used for mechanical roles (fetcher, etc.) where an LLM
        // can satisfy the surface contract without actually performing the
        // tool-call side effect. See docs/patterns.md:
        // "Prompt contracts can't enforce tool-call side effects".
        if let Some(scaffold) = &role.scaffold {
            if let Some(runner) = &self.scaffold_runner {
                let result = runner(subtask.id.clone(), scaffold.kind.clone(), resolved_params)
                    .await
                    .map_err(ExecutorError::Terminal)?;
                return Ok(result);
            }
            // Role asked for a scaffold but none is installed — surface
            // cleanly instead of silently falling back to the LLM path
            // (which would re-expose the bug the scaffold exists to fix).
            return Err(ExecutorError::Terminal(CoreError::Ipc(format!(
                "role '{}' declares scaffold kind '{}' but no scaffold runner is installed",
                subtask.role, scaffold.kind
            ))));
        }

        // LLM-powered role path: render the manifest + first message, pull
        // the role's budget + retry into per-subtask ExecutorConfig overrides,
        // dispatch through the LLM runner.
        let manifest_yaml = role.render_manifest(&resolved_params);
        let message = role.render_message(&resolved_params);

        // max_iterations = retry.max_attempts + 10, floor 10. The +10 is
        // headroom for the setup + verification turns that surround the
        // retry-eligible tool calls (plan-read, cargo_run check/test,
        // report write). See RoleRetry doc in plan/role.rs.
        let overrides = SubtaskExecutorOverrides {
            max_output_tokens: role.budget.max_output_tokens as u32,
            max_iterations: (role.retry.max_attempts + 10).max(10),
        };

        let subtask_id_owned = subtask.id.clone();
        let fut = (self.runner)(
            subtask_id_owned.clone(),
            manifest_yaml,
            message,
            overrides,
            wall_clock_deadline,
        );
        let raced = race_deadline(fut, wall_clock_deadline).await;

        match raced {
            Ok(r) => Ok(r),
            Err(CoreError::Ipc(ref m)) if m == TTL_WALL_CLOCK_SENTINEL => {
                self.audit_log.record(AuditEvent::new(
                    AgentId::from_uuid(uuid::Uuid::nil()),
                    AuditEventKind::SubtaskTtlExpired {
                        subtask_id: subtask_id_owned.clone(),
                        reason: "wall_clock_exceeded".into(),
                    },
                ));
                Err(ExecutorError::Correctable(format!(
                    "subtask '{}' exceeded wall-clock TTL",
                    subtask_id_owned
                )))
            }
            Err(e) => Err(ExecutorError::Terminal(e)),
        }
    }
}

/// Race `fut` against an optional wall-clock deadline.
///
/// If the deadline fires first, returns a sentinel `CoreError::Ipc("ttl
/// wall-clock exceeded")` and drops `fut` (cancelling any work it was
/// driving). If `deadline.is_none()`, behaves like `fut.await`. The caller
/// (`spawn_subtask`) translates the sentinel into a `SubtaskTtlExpired`
/// audit event + `Correctable` error.
async fn race_deadline<F, T>(fut: F, deadline: Option<Instant>) -> Result<T, CoreError>
where
    F: std::future::Future<Output = Result<T, CoreError>>,
{
    match deadline {
        None => fut.await,
        Some(d) => {
            tokio::select! {
                r = fut => r,
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(d)) => {
                    Err(CoreError::Ipc(TTL_WALL_CLOCK_SENTINEL.into()))
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("planner-correctable: {0}")]
    Correctable(String),
    #[error("terminal: {0}")]
    Terminal(#[from] CoreError),
}

impl From<PlannerError> for ExecutorError {
    fn from(e: PlannerError) -> Self {
        match e {
            PlannerError::Malformed(msg) => ExecutorError::Correctable(msg),
            PlannerError::LlmCall(msg) => {
                ExecutorError::Terminal(CoreError::Ipc(format!("planner LLM: {msg}")))
            }
        }
    }
}

/// Falls back to a single-subtask plan using the `generalist` role when the
/// planner fails to produce any valid plan on the initial call.
///
/// Returns the human-readable "<path> missing" reason if any of the role's
/// `file_write: {param}` grants (without a trailing `/*`) resolves to a path
/// that doesn't exist after the subtask ran. Returns None when everything
/// the role declared as output is on disk.
///
/// Rationale: a role that declares a single-path write grant is implicitly
/// contracted to produce that file. Prompts have been an insufficient lever
/// (see run 12 reflection — the "plan-complete checklist" did not fire).
/// This is an executor-enforced version of the same contract.
fn check_declared_outputs_exist(
    catalog: &RoleCatalog,
    subtask: &Subtask,
    subs: &Substitutions,
) -> Option<String> {
    use std::path::Path;

    let role = catalog.get(&subtask.role)?;
    let resolved_params = subs.apply(&subtask.params);

    for grant in &role.capabilities {
        let rest = match grant.strip_prefix("file_write:") {
            Some(r) => r.trim(),
            None => continue,
        };
        // Skip directory-writable grants (trailing /* or /**).
        if rest.ends_with("/*") || rest.ends_with("/**") {
            continue;
        }
        // Only act on grants that reference a role parameter like `{report}`.
        let param_name = match rest.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(n) => n,
            None => continue,
        };
        let path_str = match resolved_params.get(param_name).and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        if !Path::new(path_str).exists() {
            return Some(format!(
                "'{path_str}' (from {{{param_name}}}) was not written"
            ));
        }
    }
    None
}

/// The generalist role is the broad-capability escape hatch for novel goals
/// that don't match any specific role. With this fallback wired in, the
/// planner is never a hard blocker: every goal produces some kind of
/// execution, even if less efficient than a role-matched plan.
///
/// Returns `Terminal` with a clear diagnostic if the catalog has no
/// `generalist` role — the operator needs to add one.
pub fn fallback_generalist_plan(goal: &str, catalog: &RoleCatalog) -> Result<Plan, ExecutorError> {
    if catalog.get("generalist").is_none() {
        return Err(ExecutorError::Terminal(CoreError::Ipc(
            "no 'generalist' role in catalog and planner failed to match".into(),
        )));
    }
    Ok(Plan {
        subtasks: vec![Subtask {
            id: "generalist".into(),
            role: "generalist".into(),
            params: serde_json::json!({ "task_description": goal }),
            depends_on: vec![],
            ttl: None,
        }],
        final_output: "/".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{ParameterSchema, ParameterType, Role, RoleBudget, RoleRetry, Subtask};
    use aaos_core::InMemoryAuditLog;
    use std::collections::HashMap as StdHashMap;

    fn fetcher_catalog() -> RoleCatalog {
        let r = Role {
            name: "fetcher".into(),
            model: "deepseek-chat".into(),
            parameters: StdHashMap::from([
                (
                    "url".into(),
                    ParameterSchema {
                        param_type: ParameterType::String,
                        required: true,
                        description: "".into(),
                    },
                ),
                (
                    "workspace".into(),
                    ParameterSchema {
                        param_type: ParameterType::Path,
                        required: true,
                        description: "".into(),
                    },
                ),
            ]),
            capabilities: vec![],
            system_prompt: "x".into(),
            message_template: "fetch {url} to {workspace}".into(),
            budget: RoleBudget {
                max_input_tokens: 1000,
                max_output_tokens: 500,
            },
            retry: RoleRetry {
                max_attempts: 1,
                on: vec![],
            },
            priority: 128,
            scaffold: None,
        };
        let mut cat = RoleCatalog::default();
        cat.roles_mut().insert("fetcher".into(), r);
        cat
    }

    fn stub_runner() -> SubtaskRunner {
        Arc::new(|id, _manifest, _msg, _overrides, _deadline| {
            Box::pin(async move {
                Ok(SubtaskResult {
                    subtask_id: id,
                    agent_id: AgentId::new(),
                    response: "stub".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                })
            })
        })
    }

    #[tokio::test]
    async fn execute_plan_returns_correctable_for_unknown_role() {
        let cat = Arc::new(fetcher_catalog());
        let bad_plan = Plan {
            subtasks: vec![Subtask {
                id: "a".into(),
                role: "wizard".into(),
                params: serde_json::json!({}),
                depends_on: vec![],
                ttl: None,
            }],
            final_output: "/out".into(),
        };
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let audit: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let exec = PlanExecutor::new(cat, planner, stub_runner(), audit, std::env::temp_dir());
        let tmp = tempfile::tempdir().unwrap();
        let err = exec
            .execute_plan(&bad_plan, &tmp.path().to_path_buf())
            .await
            .unwrap_err();
        assert!(matches!(err, ExecutorError::Correctable(_)));
    }

    #[tokio::test]
    async fn execute_plan_returns_correctable_for_bad_params() {
        let cat = Arc::new(fetcher_catalog());
        let bad_plan = Plan {
            subtasks: vec![Subtask {
                id: "a".into(),
                role: "fetcher".into(),
                params: serde_json::json!({"url": "https://x.com"}),
                depends_on: vec![],
                ttl: None,
            }],
            final_output: "/out".into(),
        };
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let audit: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let exec = PlanExecutor::new(cat, planner, stub_runner(), audit, std::env::temp_dir());
        let tmp = tempfile::tempdir().unwrap();
        let err = exec
            .execute_plan(&bad_plan, &tmp.path().to_path_buf())
            .await
            .unwrap_err();
        assert!(matches!(err, ExecutorError::Correctable(_)));
    }

    #[tokio::test]
    async fn execute_plan_walks_dag_with_runner() {
        let cat = Arc::new(fetcher_catalog());
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = counter.clone();
        let runner: SubtaskRunner = Arc::new(move |id, _m, _msg, _overrides, _deadline| {
            let c = counter_clone.clone();
            Box::pin(async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(SubtaskResult {
                    subtask_id: id,
                    agent_id: AgentId::new(),
                    response: "ok".into(),
                    input_tokens: 10,
                    output_tokens: 5,
                })
            })
        });
        let audit_concrete = Arc::new(InMemoryAuditLog::new());
        let audit: Arc<dyn AuditLog> = audit_concrete.clone();
        let exec = PlanExecutor::new(cat, planner, runner, audit, std::env::temp_dir());

        let plan = Plan {
            subtasks: vec![
                Subtask {
                    id: "hn".into(),
                    role: "fetcher".into(),
                    params: serde_json::json!({
                        "url": "https://news.ycombinator.com/",
                        "workspace": "{run}/hn.html"
                    }),
                    depends_on: vec![],
                    ttl: None,
                },
                Subtask {
                    id: "lob".into(),
                    role: "fetcher".into(),
                    params: serde_json::json!({
                        "url": "https://lobste.rs/",
                        "workspace": "{run}/lob.html"
                    }),
                    depends_on: vec![],
                    ttl: None,
                },
            ],
            final_output: "/out".into(),
        };
        let tmp = tempfile::tempdir().unwrap();
        let r = exec
            .execute_plan(&plan, &tmp.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(r.results.len(), 2);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);

        let events = audit_concrete.events();
        let started_count = events
            .iter()
            .filter(|e| matches!(e.event, AuditEventKind::SubtaskStarted { .. }))
            .count();
        let completed_count = events
            .iter()
            .filter(|e| matches!(e.event, AuditEventKind::SubtaskCompleted { .. }))
            .count();
        assert_eq!(started_count, 2);
        assert_eq!(completed_count, 2);
    }

    fn generalist_catalog() -> RoleCatalog {
        let r = Role {
            name: "generalist".into(),
            model: "deepseek-chat".into(),
            parameters: StdHashMap::from([(
                "task_description".into(),
                ParameterSchema {
                    param_type: ParameterType::String,
                    required: true,
                    description: "".into(),
                },
            )]),
            capabilities: vec![],
            system_prompt: "generalist".into(),
            message_template: "{task_description}".into(),
            budget: RoleBudget {
                max_input_tokens: 100_000,
                max_output_tokens: 10_000,
            },
            retry: RoleRetry {
                max_attempts: 1,
                on: vec![],
            },
            priority: 128,
            scaffold: None,
        };
        let mut cat = RoleCatalog::default();
        cat.roles_mut().insert("generalist".into(), r);
        cat
    }

    #[test]
    fn fallback_plan_targets_generalist() {
        let cat = generalist_catalog();
        let fb = fallback_generalist_plan("do the thing", &cat).unwrap();
        assert_eq!(fb.subtasks.len(), 1);
        assert_eq!(fb.subtasks[0].role, "generalist");
        assert_eq!(
            fb.subtasks[0]
                .params
                .get("task_description")
                .and_then(|v| v.as_str()),
            Some("do the thing")
        );
    }

    #[test]
    fn fallback_errors_when_generalist_missing() {
        let cat = fetcher_catalog(); // no generalist role
        assert!(fallback_generalist_plan("do it", &cat).is_err());
    }

    struct MockLlm;
    #[async_trait::async_trait]
    impl aaos_llm::LlmClient for MockLlm {
        async fn complete(
            &self,
            _req: aaos_llm::CompletionRequest,
        ) -> aaos_llm::LlmResult<aaos_llm::CompletionResponse> {
            unimplemented!("planner LLM not invoked in these tests")
        }
        fn max_context_tokens(&self, _model: &str) -> u32 {
            100_000
        }
    }

    /// Stateful MockLlm that returns a pre-scripted sequence of responses.
    /// Used by the replan test so the first Planner call returns a plan
    /// whose subtask will fail, and the second call returns a plan whose
    /// subtask succeeds.
    struct ScriptedLlm {
        replies: std::sync::Mutex<Vec<String>>,
    }
    impl ScriptedLlm {
        fn new(replies: Vec<String>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies),
            }
        }
    }
    #[async_trait::async_trait]
    impl aaos_llm::LlmClient for ScriptedLlm {
        async fn complete(
            &self,
            _req: aaos_llm::CompletionRequest,
        ) -> aaos_llm::LlmResult<aaos_llm::CompletionResponse> {
            let mut q = self.replies.lock().unwrap();
            let text = q.remove(0);
            Ok(aaos_llm::CompletionResponse {
                content: vec![aaos_llm::ContentBlock::Text { text }],
                stop_reason: aaos_llm::LlmStopReason::EndTurn,
                usage: aaos_core::TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
            })
        }
        fn max_context_tokens(&self, _model: &str) -> u32 {
            100_000
        }
    }

    // ---- Replan on subtask failure ----

    #[tokio::test]
    async fn subtask_failure_surfaces_as_correctable_with_context() {
        let cat = Arc::new(fetcher_catalog());
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        // Runner that always fails — simulates a fetch that blew up.
        let runner: SubtaskRunner = Arc::new(|_id, _m, _msg, _o, _deadline| {
            Box::pin(async move { Err(CoreError::Ipc("HTTP 404".into())) })
        });
        let audit: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let exec = PlanExecutor::new(cat, planner, runner, audit, std::env::temp_dir());

        let plan = Plan {
            subtasks: vec![Subtask {
                id: "hn".into(),
                role: "fetcher".into(),
                params: serde_json::json!({
                    "url": "https://example.invalid/",
                    "workspace": "{run}/hn.html"
                }),
                depends_on: vec![],
                ttl: None,
            }],
            final_output: "/out".into(),
        };
        let tmp = tempfile::tempdir().unwrap();
        let err = exec
            .execute_plan(&plan, &tmp.path().to_path_buf())
            .await
            .unwrap_err();
        match err {
            ExecutorError::Correctable(msg) => {
                assert!(msg.contains("subtask 'hn'"), "got: {}", msg);
                assert!(msg.contains("role 'fetcher'"), "got: {}", msg);
                assert!(msg.contains("HTTP 404"), "got: {}", msg);
            }
            other => panic!("expected Correctable, got {:?}", other),
        }
    }

    fn reporter_catalog() -> RoleCatalog {
        // Role that declares a single-path output via `file_write: {report}`.
        let r = Role {
            name: "reporter".into(),
            model: "deepseek-chat".into(),
            parameters: StdHashMap::from([(
                "report".into(),
                ParameterSchema {
                    param_type: ParameterType::Path,
                    required: true,
                    description: "".into(),
                },
            )]),
            capabilities: vec!["file_write: {report}".into()],
            system_prompt: "x".into(),
            message_template: "write a report to {report}".into(),
            budget: RoleBudget {
                max_input_tokens: 1000,
                max_output_tokens: 500,
            },
            retry: RoleRetry {
                max_attempts: 1,
                on: vec![],
            },
            priority: 128,
            scaffold: None,
        };
        let mut cat = RoleCatalog::default();
        cat.roles_mut().insert("reporter".into(), r);
        cat
    }

    #[tokio::test]
    async fn declared_output_missing_fails_subtask() {
        // Runner returns Ok — simulating the run-12 failure mode where the
        // agent says "complete" with the report unwritten.
        let cat = Arc::new(reporter_catalog());
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let audit: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let exec = PlanExecutor::new(cat, planner, stub_runner(), audit, std::env::temp_dir());

        let tmp = tempfile::tempdir().unwrap();
        let report_path = tmp.path().join("report.md");
        let plan = Plan {
            subtasks: vec![Subtask {
                id: "r".into(),
                role: "reporter".into(),
                params: serde_json::json!({ "report": report_path.to_str().unwrap() }),
                depends_on: vec![],
                ttl: None,
            }],
            final_output: "/out".into(),
        };
        let err = exec
            .execute_plan(&plan, &tmp.path().to_path_buf())
            .await
            .unwrap_err();
        match err {
            ExecutorError::Correctable(msg) => {
                assert!(msg.contains("did not produce"), "got: {msg}");
                assert!(msg.contains("{report}"), "got: {msg}");
            }
            other => panic!("expected Correctable, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn declared_output_present_passes_subtask() {
        let cat = Arc::new(reporter_catalog());
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let audit: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let exec = PlanExecutor::new(cat, planner, stub_runner(), audit, std::env::temp_dir());

        let tmp = tempfile::tempdir().unwrap();
        let report_path = tmp.path().join("report.md");
        std::fs::write(&report_path, "all good").unwrap();

        let plan = Plan {
            subtasks: vec![Subtask {
                id: "r".into(),
                role: "reporter".into(),
                params: serde_json::json!({ "report": report_path.to_str().unwrap() }),
                depends_on: vec![],
                ttl: None,
            }],
            final_output: "/out".into(),
        };
        let result = exec.execute_plan(&plan, &tmp.path().to_path_buf()).await;
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
    }

    #[tokio::test]
    async fn failed_subtask_emits_subtask_completed_success_false() {
        let cat = Arc::new(fetcher_catalog());
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let runner: SubtaskRunner = Arc::new(|_id, _m, _msg, _o, _deadline| {
            Box::pin(async move { Err(CoreError::Ipc("boom".into())) })
        });
        let audit_concrete = Arc::new(InMemoryAuditLog::new());
        let audit: Arc<dyn AuditLog> = audit_concrete.clone();
        let exec = PlanExecutor::new(cat, planner, runner, audit, std::env::temp_dir());

        let plan = Plan {
            subtasks: vec![Subtask {
                id: "a".into(),
                role: "fetcher".into(),
                params: serde_json::json!({
                    "url": "https://x/",
                    "workspace": "{run}/a.html"
                }),
                depends_on: vec![],
                ttl: None,
            }],
            final_output: "/out".into(),
        };
        let tmp = tempfile::tempdir().unwrap();
        let _ = exec.execute_plan(&plan, &tmp.path().to_path_buf()).await;
        let events = audit_concrete.events();
        let failed = events.iter().find(|e| {
            matches!(
                &e.event,
                AuditEventKind::SubtaskCompleted { subtask_id, success: false } if subtask_id == "a"
            )
        });
        assert!(
            failed.is_some(),
            "expected SubtaskCompleted{{success:false}} for 'a' — events: {:?}",
            events.iter().map(|e| &e.event).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn parallel_batch_one_failure_preserves_sibling_audit() {
        // Two fetchers in a single batch: one succeeds, one fails. The
        // batch returns Correctable (so the run loop can replan), but the
        // successful sibling must still produce SubtaskCompleted{success:true}
        // and the failing one must produce SubtaskCompleted{success:false}.
        let cat = Arc::new(fetcher_catalog());
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let runner: SubtaskRunner = Arc::new(|id, _m, _msg, _o, _deadline| {
            Box::pin(async move {
                if id == "bad" {
                    Err(CoreError::Ipc("HTTP 500".into()))
                } else {
                    Ok(SubtaskResult {
                        subtask_id: id,
                        agent_id: AgentId::new(),
                        response: "ok".into(),
                        input_tokens: 0,
                        output_tokens: 0,
                    })
                }
            })
        });
        let audit_concrete = Arc::new(InMemoryAuditLog::new());
        let audit: Arc<dyn AuditLog> = audit_concrete.clone();
        let exec = PlanExecutor::new(cat, planner, runner, audit, std::env::temp_dir());

        let plan = Plan {
            subtasks: vec![
                Subtask {
                    id: "good".into(),
                    role: "fetcher".into(),
                    params: serde_json::json!({
                        "url": "https://ok/",
                        "workspace": "{run}/good.html"
                    }),
                    depends_on: vec![],
                    ttl: None,
                },
                Subtask {
                    id: "bad".into(),
                    role: "fetcher".into(),
                    params: serde_json::json!({
                        "url": "https://fail/",
                        "workspace": "{run}/bad.html"
                    }),
                    depends_on: vec![],
                    ttl: None,
                },
            ],
            final_output: "/out".into(),
        };
        let tmp = tempfile::tempdir().unwrap();
        let err = exec
            .execute_plan(&plan, &tmp.path().to_path_buf())
            .await
            .unwrap_err();
        assert!(matches!(err, ExecutorError::Correctable(_)));

        let events = audit_concrete.events();
        let good_ok = events.iter().any(|e| matches!(
            &e.event,
            AuditEventKind::SubtaskCompleted { subtask_id, success: true } if subtask_id == "good"
        ));
        let bad_fail = events.iter().any(|e| matches!(
            &e.event,
            AuditEventKind::SubtaskCompleted { subtask_id, success: false } if subtask_id == "bad"
        ));
        assert!(good_ok, "expected success event for 'good'");
        assert!(bad_fail, "expected failure event for 'bad'");
    }

    #[tokio::test]
    async fn run_replans_after_subtask_failure_and_succeeds() {
        // Scripted planner: first reply produces a plan with subtask id
        // "try1" which the runner is wired to fail; second reply (after
        // replan) produces a plan with id "try2" which succeeds.
        let cat = Arc::new(fetcher_catalog());

        let plan_try1 = r#"{"subtasks":[{"id":"try1","role":"fetcher","params":{"url":"https://fail/","workspace":"{run}/a.html"},"depends_on":[]}],"final_output":"/out"}"#;
        let plan_try2 = r#"{"subtasks":[{"id":"try2","role":"fetcher","params":{"url":"https://ok/","workspace":"{run}/a.html"},"depends_on":[]}],"final_output":"/out"}"#;
        let scripted = Arc::new(ScriptedLlm::new(vec![plan_try1.into(), plan_try2.into()]));
        let planner = Arc::new(Planner::new(scripted, "deepseek-chat".into()));

        let runner: SubtaskRunner = Arc::new(|id, _m, _msg, _o, _deadline| {
            Box::pin(async move {
                if id == "try1" {
                    Err(CoreError::Ipc("HTTP 404".into()))
                } else {
                    Ok(SubtaskResult {
                        subtask_id: id,
                        agent_id: AgentId::new(),
                        response: "ok".into(),
                        input_tokens: 0,
                        output_tokens: 0,
                    })
                }
            })
        });

        let audit_concrete = Arc::new(InMemoryAuditLog::new());
        let audit: Arc<dyn AuditLog> = audit_concrete.clone();
        let tmp = tempfile::tempdir().unwrap();
        let exec = PlanExecutor::new(cat, planner, runner, audit, tmp.path().to_path_buf());

        let result = exec
            .run("fetch something", uuid::Uuid::new_v4())
            .await
            .expect("run should succeed via replan");
        assert_eq!(result.results.len(), 1);
        assert!(result.results.contains_key("try2"));

        let events = audit_concrete.events();
        let replanned = events
            .iter()
            .any(|e| matches!(&e.event, AuditEventKind::PlanReplanned { .. }));
        assert!(replanned, "expected PlanReplanned audit event");
    }

    #[tokio::test]
    async fn hop_exhaustion_fails_subtask_before_launch() {
        use aaos_core::TaskTtl;

        // Same catalog pattern as other tests — fetcher_catalog() already
        // provides a "fetcher" role. We just need a subtask with max_hops=0.
        let cat = Arc::new(fetcher_catalog());
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let audit: Arc<InMemoryAuditLog> = Arc::new(InMemoryAuditLog::new());
        let audit_trait: Arc<dyn AuditLog> = audit.clone();
        let exec = PlanExecutor::new(
            cat,
            planner,
            stub_runner(),
            audit_trait,
            std::env::temp_dir(),
        );

        let plan = Plan {
            subtasks: vec![Subtask {
                id: "expired".into(),
                role: "fetcher".into(),
                params: serde_json::json!({"url": "https://x.com", "workspace": "/tmp/x"}),
                depends_on: vec![],
                ttl: Some(TaskTtl {
                    max_hops: Some(0),
                    max_wall_clock: None,
                }),
            }],
            final_output: "expired".into(),
        };

        let tmp = tempfile::tempdir().unwrap();
        let result = exec.execute_plan(&plan, &tmp.path().to_path_buf()).await;
        assert!(
            matches!(&result, Err(ExecutorError::Correctable(_))),
            "expected hop-exhausted subtask to produce Correctable failure; got {:?}",
            result.as_ref().err()
        );

        let expired: Vec<_> = audit
            .events()
            .into_iter()
            .filter(|e| {
                matches!(&e.event, AuditEventKind::SubtaskTtlExpired { subtask_id, reason }
                    if subtask_id == "expired" && reason == "hops_exhausted")
            })
            .collect();
        assert_eq!(
            expired.len(),
            1,
            "expected exactly one SubtaskTtlExpired event with reason=hops_exhausted"
        );

        let started = audit
            .events()
            .into_iter()
            .filter(|e| matches!(&e.event, AuditEventKind::SubtaskStarted { subtask_id, .. } if subtask_id == "expired"))
            .count();
        assert_eq!(
            started, 0,
            "SubtaskStarted must not fire for a TTL-exhausted subtask (invariant would regress if hop check were moved after the Started record)"
        );
    }

    #[tokio::test]
    async fn wall_clock_expiry_kills_running_subtask() {
        use aaos_core::{AuditEventKind, TaskTtl};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc as StdArc;
        use std::time::Duration as StdDuration;

        let was_cancelled = StdArc::new(AtomicBool::new(false));
        let wc = was_cancelled.clone();

        // Slow stub: sleeps 5s, observes its own cancellation via Drop side effect.
        let runner: SubtaskRunner = Arc::new(move |id, _m, _msg, _o, _deadline| {
            let wc = wc.clone();
            Box::pin(async move {
                struct DropFlag(StdArc<AtomicBool>);
                impl Drop for DropFlag {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::SeqCst);
                    }
                }
                let _flag = DropFlag(wc);
                tokio::time::sleep(StdDuration::from_secs(5)).await;
                Ok(SubtaskResult {
                    subtask_id: id,
                    agent_id: AgentId::new(),
                    response: "never".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                })
            })
        });

        let cat = Arc::new(fetcher_catalog());
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let audit: Arc<InMemoryAuditLog> = Arc::new(InMemoryAuditLog::new());
        let audit_trait: Arc<dyn AuditLog> = audit.clone();
        let exec = PlanExecutor::new(cat, planner, runner, audit_trait, std::env::temp_dir());

        let plan = Plan {
            subtasks: vec![Subtask {
                id: "slow".into(),
                role: "fetcher".into(),
                params: serde_json::json!({"url": "https://x.com", "workspace": "/tmp/x"}),
                depends_on: vec![],
                ttl: Some(TaskTtl {
                    max_hops: None,
                    max_wall_clock: Some(StdDuration::from_millis(500)),
                }),
            }],
            final_output: "slow".into(),
        };

        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let result = exec.execute_plan(&plan, &tmp.path().to_path_buf()).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "wall-clock expiry must fail the plan");
        assert!(
            elapsed >= StdDuration::from_millis(400) && elapsed < StdDuration::from_secs(3),
            "expected kill between 400ms and 3s; got {elapsed:?}"
        );
        assert!(
            was_cancelled.load(Ordering::SeqCst),
            "subtask future must have been cancelled (dropped)"
        );

        let expired: Vec<_> = audit
            .events()
            .into_iter()
            .filter(|e| {
                matches!(&e.event, AuditEventKind::SubtaskTtlExpired { subtask_id, reason }
                if subtask_id == "slow" && reason == "wall_clock_exceeded")
            })
            .collect();
        assert_eq!(expired.len(), 1);
    }

    #[tokio::test]
    async fn dependent_cascades_after_wall_clock_expiry() {
        use aaos_core::{AuditEventKind, TaskTtl};
        use std::time::Duration as StdDuration;

        let runner: SubtaskRunner = Arc::new(|id, _m, _msg, _o, _deadline| {
            Box::pin(async move {
                if id == "slow" {
                    tokio::time::sleep(StdDuration::from_secs(5)).await;
                }
                Ok(SubtaskResult {
                    subtask_id: id,
                    agent_id: AgentId::new(),
                    response: "ok".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                })
            })
        });

        let cat = Arc::new(fetcher_catalog());
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let audit: Arc<InMemoryAuditLog> = Arc::new(InMemoryAuditLog::new());
        let audit_trait: Arc<dyn AuditLog> = audit.clone();
        let exec = PlanExecutor::new(cat, planner, runner, audit_trait, std::env::temp_dir());

        let plan = Plan {
            subtasks: vec![
                Subtask {
                    id: "slow".into(),
                    role: "fetcher".into(),
                    params: serde_json::json!({"url": "https://x.com", "workspace": "/tmp/x"}),
                    depends_on: vec![],
                    ttl: Some(TaskTtl {
                        max_hops: None,
                        max_wall_clock: Some(StdDuration::from_millis(500)),
                    }),
                },
                Subtask {
                    id: "dependent".into(),
                    role: "fetcher".into(),
                    params: serde_json::json!({"url": "https://y.com", "workspace": "/tmp/y"}),
                    depends_on: vec!["slow".into()],
                    ttl: None,
                },
            ],
            final_output: "dependent".into(),
        };

        let tmp = tempfile::tempdir().unwrap();
        let _ = exec.execute_plan(&plan, &tmp.path().to_path_buf()).await;

        let dependent_started = audit.events().into_iter().any(|e| {
            matches!(&e.event, AuditEventKind::SubtaskStarted { subtask_id, .. } if subtask_id == "dependent")
        });
        assert!(
            !dependent_started,
            "dependent must not launch after its dep failed via TTL"
        );
    }
}
