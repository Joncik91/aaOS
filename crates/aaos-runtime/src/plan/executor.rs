//! PlanExecutor — deterministic DAG walk that spawns children per role.
//!
//! This is the skeleton. `execute_plan` currently validates structure/roles/
//! params only — spawning subtasks is wired in Task 9 via a SubtaskRunner
//! closure supplied by the server. The validation paths here produce the
//! Correctable errors the replan loop depends on, so testing them early
//! (before spawning complexity) pins the error-routing contract.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aaos_core::CoreError;

use crate::plan::{
    topo_batches, Plan, PlanResult, Planner, PlannerError, RoleCatalog, Substitutions,
    SubtaskId, SubtaskResult,
};

pub struct PlanExecutor {
    catalog: Arc<RoleCatalog>,
    planner: Arc<Planner>,
    max_replans: u32,
    total_deadline: Duration,
    run_root_base: PathBuf,
}

impl PlanExecutor {
    pub fn new(
        catalog: Arc<RoleCatalog>,
        planner: Arc<Planner>,
        run_root_base: PathBuf,
    ) -> Self {
        Self {
            catalog,
            planner,
            max_replans: 3,
            total_deadline: Duration::from_secs(600),
            run_root_base,
        }
    }

    pub async fn run(&self, goal: &str, run_id: uuid::Uuid) -> Result<PlanResult, ExecutorError> {
        let started = Instant::now();
        let run_root = self.run_root_base.join(run_id.to_string());
        std::fs::create_dir_all(&run_root)
            .map_err(|e| ExecutorError::Terminal(CoreError::Ipc(format!(
                "create workspace {}: {}",
                run_root.display(),
                e
            ))))?;

        let mut plan = self
            .planner
            .plan(goal, &self.catalog)
            .await
            .map_err(ExecutorError::from)?;
        self.write_plan_json(&run_root, &plan)?;

        let mut replans_used = 0;
        loop {
            if started.elapsed() > self.total_deadline {
                return Err(ExecutorError::Terminal(CoreError::Ipc(
                    "planner deadline exhausted".into(),
                )));
            }
            match self.execute_plan(&plan, &run_root).await {
                Ok(result) => return Ok(result),
                Err(ExecutorError::Correctable(reason)) if replans_used < self.max_replans => {
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
        std::fs::write(&path, json)
            .map_err(|e| ExecutorError::Terminal(CoreError::Ipc(format!(
                "write {}: {}",
                path.display(),
                e
            ))))
    }

    pub async fn execute_plan(
        &self,
        plan: &Plan,
        run_root: &PathBuf,
    ) -> Result<PlanResult, ExecutorError> {
        let subs = Substitutions::new(run_root.clone());

        // Structural check.
        let batches = topo_batches(plan).map_err(ExecutorError::Correctable)?;

        // Role + param validation before any spawn.
        for s in &plan.subtasks {
            let role = self
                .catalog
                .get(&s.role)
                .ok_or_else(|| ExecutorError::Correctable(format!("unknown role: {}", s.role)))?;
            let resolved = subs.apply(&s.params);
            role.validate_params(&resolved).map_err(ExecutorError::Correctable)?;
        }

        // Stub: spawn is Task 9. Empty results for now.
        let _ = batches;
        let _: HashMap<SubtaskId, SubtaskResult> = HashMap::new();
        Ok(PlanResult {
            plan: plan.clone(),
            results: HashMap::new(),
            final_output: plan.final_output.clone(),
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{
        ParameterSchema, ParameterType, Role, RoleBudget, RoleRetry, Subtask,
    };
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
        };
        let mut cat = RoleCatalog::default();
        cat.roles_mut().insert("fetcher".into(), r);
        cat
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
        let planner = Arc::new(Planner::new(
            Arc::new(MockLlm),
            "deepseek-chat".into(),
        ));
        let exec = PlanExecutor::new(cat, planner, std::env::temp_dir());
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
                params: serde_json::json!({"url": "https://x.com"}), // missing workspace
                depends_on: vec![],
            }],
            final_output: "/out".into(),
        };
        let planner = Arc::new(Planner::new(
            Arc::new(MockLlm),
            "deepseek-chat".into(),
        ));
        let exec = PlanExecutor::new(cat, planner, std::env::temp_dir());
        let tmp = tempfile::tempdir().unwrap();
        let err = exec
            .execute_plan(&bad_plan, &tmp.path().to_path_buf())
            .await
            .unwrap_err();
        assert!(matches!(err, ExecutorError::Correctable(_)));
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
