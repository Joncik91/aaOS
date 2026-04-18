//! Computed orchestration — role catalog, planner, deterministic executor.
//!
//! Agents today are described by static YAML manifests that Bootstrap has
//! to improvise against at runtime. The plan module replaces that with a
//! typed two-phase boot: a Planner emits a structured Plan; a PlanExecutor
//! walks the DAG, instantiating children from per-role scaffolds. The LLM
//! reasons about content inside each child; orchestration is pure code.

pub mod escalation;
pub mod executor;
pub mod placeholders;
pub mod planner;
pub mod role;

pub use escalation::{default_escalation_signals, EscalationSignal};
pub use executor::{
    ExecutorError, PlanExecutor, ScaffoldRunner, SubtaskExecutorOverrides, SubtaskRunner,
};
pub use placeholders::Substitutions;
pub use planner::{validate_plan_structure, Planner, PlannerError};
pub use role::{
    ParameterSchema, ParameterType, Role, RoleBudget, RoleCatalog, RoleCatalogError, RoleRetry,
    RoleScaffold,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type SubtaskId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Subtask {
    pub id: SubtaskId,
    pub role: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub depends_on: Vec<SubtaskId>,
    /// Optional per-subtask TTL. Populated by the planner from role defaults
    /// + env, or left None for no bound. Existing serialized plans without
    /// this field deserialize to None (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<aaos_core::TaskTtl>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    pub subtasks: Vec<Subtask>,
    pub final_output: String,
}

#[derive(Debug, Clone)]
pub struct SubtaskResult {
    pub subtask_id: SubtaskId,
    pub agent_id: aaos_core::AgentId,
    pub response: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct PlanResult {
    pub plan: Plan,
    pub results: std::collections::HashMap<SubtaskId, SubtaskResult>,
    pub final_output: String,
}

/// Topologically sort subtasks into batches where each batch contains only
/// subtasks whose dependencies are all in earlier batches. Subtasks in the
/// same batch can execute concurrently.
pub fn topo_batches(plan: &Plan) -> Result<Vec<Vec<Subtask>>, String> {
    use std::collections::{HashMap, HashSet};

    let mut pending: HashMap<SubtaskId, Subtask> = plan
        .subtasks
        .iter()
        .map(|s| (s.id.clone(), s.clone()))
        .collect();

    // Validate dependencies reference known ids.
    for s in &plan.subtasks {
        for d in &s.depends_on {
            if !pending.contains_key(d) {
                return Err(format!("subtask '{}' depends on unknown id '{}'", s.id, d));
            }
        }
    }

    let mut done: HashSet<SubtaskId> = HashSet::new();
    let mut batches: Vec<Vec<Subtask>> = Vec::new();

    while !pending.is_empty() {
        let ready: Vec<SubtaskId> = pending
            .values()
            .filter(|s| s.depends_on.iter().all(|d| done.contains(d)))
            .map(|s| s.id.clone())
            .collect();

        if ready.is_empty() {
            let remaining: Vec<&str> = pending.keys().map(|s| s.as_str()).collect();
            return Err(format!("dependency cycle among: {}", remaining.join(", ")));
        }

        let mut batch: Vec<Subtask> = ready.iter().map(|id| pending.remove(id).unwrap()).collect();
        batch.sort_by(|a, b| a.id.cmp(&b.id));
        for s in &batch {
            done.insert(s.id.clone());
        }
        batches.push(batch);
    }

    Ok(batches)
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use serde_json::json;

    fn subtask(id: &str, deps: &[&str]) -> Subtask {
        Subtask {
            id: id.into(),
            role: "role".into(),
            params: json!({}),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            ttl: None,
        }
    }

    fn plan_of(ss: Vec<Subtask>) -> Plan {
        Plan {
            subtasks: ss,
            final_output: "/out".into(),
        }
    }

    #[test]
    fn single_task_single_batch() {
        let p = plan_of(vec![subtask("a", &[])]);
        let b = topo_batches(&p).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].len(), 1);
        assert_eq!(b[0][0].id, "a");
    }

    #[test]
    fn two_independent_same_batch() {
        let p = plan_of(vec![subtask("b", &[]), subtask("a", &[])]);
        let b = topo_batches(&p).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].len(), 2);
        assert_eq!(b[0][0].id, "a");
        assert_eq!(b[0][1].id, "b");
    }

    #[test]
    fn linear_chain_separate_batches() {
        let p = plan_of(vec![
            subtask("c", &["b"]),
            subtask("b", &["a"]),
            subtask("a", &[]),
        ]);
        let b = topo_batches(&p).unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!(b[0][0].id, "a");
        assert_eq!(b[1][0].id, "b");
        assert_eq!(b[2][0].id, "c");
    }

    #[test]
    fn fan_in_structure() {
        // Two fetchers, then one writer depending on both.
        let p = plan_of(vec![
            subtask("write", &["hn", "lob"]),
            subtask("hn", &[]),
            subtask("lob", &[]),
        ]);
        let b = topo_batches(&p).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].len(), 2); // parallel fetchers
        assert_eq!(b[1].len(), 1); // writer
        assert_eq!(b[1][0].id, "write");
    }

    #[test]
    fn unknown_dependency_errors() {
        let p = plan_of(vec![subtask("a", &["ghost"])]);
        let err = topo_batches(&p).unwrap_err();
        assert!(err.contains("unknown id"), "err: {}", err);
        assert!(err.contains("ghost"), "err: {}", err);
    }

    #[test]
    fn cycle_errors() {
        let p = plan_of(vec![subtask("a", &["b"]), subtask("b", &["a"])]);
        let err = topo_batches(&p).unwrap_err();
        assert!(err.contains("cycle"), "err: {}", err);
    }

    #[test]
    fn subtask_ttl_serde_roundtrip_and_default() {
        use aaos_core::TaskTtl;
        use std::time::Duration;

        // Default: no ttl — back-compat for serialized plans without the field.
        let s: Subtask =
            serde_json::from_str(r#"{"id":"a","role":"writer","params":{},"depends_on":[]}"#)
                .unwrap();
        assert!(s.ttl.is_none(), "missing ttl must deserialize as None");

        // With ttl.
        let s2 = Subtask {
            id: "b".into(),
            role: "writer".into(),
            params: serde_json::json!({}),
            depends_on: vec![],
            ttl: Some(TaskTtl {
                max_hops: Some(3),
                max_wall_clock: Some(Duration::from_secs(30)),
            }),
        };
        let json = serde_json::to_string(&s2).unwrap();
        let back: Subtask = serde_json::from_str(&json).unwrap();
        assert_eq!(s2, back);

        // Verify skip_serializing_if: None ttl must not appear in output JSON.
        let s_none = Subtask {
            id: "c".into(),
            role: "writer".into(),
            params: serde_json::json!({}),
            depends_on: vec![],
            ttl: None,
        };
        let json_none = serde_json::to_string(&s_none).unwrap();
        assert!(
            !json_none.contains("\"ttl\""),
            "None ttl must be omitted from serialized JSON, got: {}",
            json_none
        );
    }
}
