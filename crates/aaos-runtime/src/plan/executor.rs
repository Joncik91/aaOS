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
    topo_batches, Plan, PlanResult, Planner, PlannerError, RoleCatalog, Subtask,
    SubtaskId, SubtaskResult, Substitutions,
};

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
            String,                     // subtask_id (for audit correlation)
            String,                     // rendered manifest YAML
            String,                     // first message for the child
            SubtaskExecutorOverrides,   // per-role budget + iteration caps
        ) -> Pin<
            Box<dyn Future<Output = Result<SubtaskResult, CoreError>> + Send>,
        > + Send
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
            String,                     // subtask_id
            String,                     // scaffold kind (e.g. "fetcher")
            serde_json::Value,          // resolved params
        ) -> Pin<
            Box<dyn Future<Output = Result<SubtaskResult, CoreError>> + Send>,
        > + Send
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
            ExecutorError::Terminal(CoreError::Ipc(format!(
                "write {}: {}",
                path.display(),
                e
            )))
        })
    }

    pub async fn execute_plan(
        &self,
        plan: &Plan,
        run_root: &PathBuf,
    ) -> Result<PlanResult, ExecutorError> {
        use futures::future::try_join_all;

        let subs = Substitutions::new(run_root.clone());
        let batches = topo_batches(plan).map_err(ExecutorError::Correctable)?;

        // Pre-validate all subtasks BEFORE spawning anything.
        for s in &plan.subtasks {
            let role = self
                .catalog
                .get(&s.role)
                .ok_or_else(|| ExecutorError::Correctable(format!("unknown role: {}", s.role)))?;
            let resolved = subs.apply(&s.params);
            role.validate_params(&resolved).map_err(ExecutorError::Correctable)?;
        }

        let mut results: HashMap<SubtaskId, SubtaskResult> = HashMap::new();

        for batch in batches {
            let spawns = batch
                .iter()
                .map(|s| self.spawn_subtask(s, &subs))
                .collect::<Vec<_>>();
            let batch_results = try_join_all(spawns).await?;
            for (subtask, result) in batch.iter().zip(batch_results) {
                self.audit_log.record(AuditEvent::new(
                    result.agent_id,
                    AuditEventKind::SubtaskCompleted {
                        subtask_id: subtask.id.clone(),
                        success: true,
                    },
                ));
                results.insert(subtask.id.clone(), result);
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
                let result = runner(
                    subtask.id.clone(),
                    scaffold.kind.clone(),
                    resolved_params,
                )
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

        let overrides = SubtaskExecutorOverrides {
            max_output_tokens: role.budget.max_output_tokens as u32,
            max_iterations: (role.retry.max_attempts + 10).max(10),
        };

        let result = (self.runner)(subtask.id.clone(), manifest_yaml, message, overrides)
            .await
            .map_err(ExecutorError::Terminal)?;
        Ok(result)
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
        }],
        final_output: "/".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{
        ParameterSchema, ParameterType, Role, RoleBudget, RoleRetry, Subtask,
    };
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
            scaffold: None,
        };
        let mut cat = RoleCatalog::default();
        cat.roles_mut().insert("fetcher".into(), r);
        cat
    }

    fn stub_runner() -> SubtaskRunner {
        Arc::new(|id, _manifest, _msg, _overrides| {
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
            }],
            final_output: "/out".into(),
        };
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let audit: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let exec = PlanExecutor::new(
            cat,
            planner,
            stub_runner(),
            audit,
            std::env::temp_dir(),
        );
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
            }],
            final_output: "/out".into(),
        };
        let planner = Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into()));
        let audit: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let exec = PlanExecutor::new(
            cat,
            planner,
            stub_runner(),
            audit,
            std::env::temp_dir(),
        );
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
        let runner: SubtaskRunner = Arc::new(move |id, _m, _msg, _overrides| {
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
                },
                Subtask {
                    id: "lob".into(),
                    role: "fetcher".into(),
                    params: serde_json::json!({
                        "url": "https://lobste.rs/",
                        "workspace": "{run}/lob.html"
                    }),
                    depends_on: vec![],
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
            fb.subtasks[0].params.get("task_description").and_then(|v| v.as_str()),
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
}
