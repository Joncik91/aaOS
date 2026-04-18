//! Planner: turns a goal into a Plan via one LLM call.
//!
//! The Planner is intentionally single-turn and structured-output-only. When
//! a subtask in the emitted Plan fails in a "correctable" way (unknown role,
//! missing param, malformed structure), the PlanExecutor calls `replan()` to
//! re-invoke with the failure reason, up to 3 times total.

use std::sync::Arc;

use aaos_core::AgentId;
use aaos_llm::{CompletionRequest, ContentBlock, LlmClient, Message};

use crate::plan::{Plan, RoleCatalog};

pub struct Planner {
    llm: Arc<dyn LlmClient>,
    model: String,
}

impl Planner {
    pub fn new(llm: Arc<dyn LlmClient>, model: String) -> Self {
        Self { llm, model }
    }

    /// Produce an initial Plan for the goal. Returns the parsed Plan after
    /// structural validation. Malformed LLM output surfaces as
    /// PlannerError::Malformed; the caller (PlanExecutor) decides whether to
    /// retry, fall back to the generalist role, or give up.
    pub async fn plan(&self, goal: &str, catalog: &RoleCatalog) -> Result<Plan, PlannerError> {
        let prompt = self.build_prompt(goal, catalog, None, None);
        self.call_and_parse(&prompt, catalog).await
    }

    /// Ask the planner to revise. `previous` is the Plan that failed,
    /// `failure_reason` is a human-friendly description of why.
    pub async fn replan(
        &self,
        goal: &str,
        catalog: &RoleCatalog,
        previous: &Plan,
        failure_reason: &str,
    ) -> Result<Plan, PlannerError> {
        let prompt = self.build_prompt(goal, catalog, Some(previous), Some(failure_reason));
        self.call_and_parse(&prompt, catalog).await
    }

    fn build_prompt(
        &self,
        goal: &str,
        catalog: &RoleCatalog,
        previous: Option<&Plan>,
        failure_reason: Option<&str>,
    ) -> String {
        // Catalog dump now includes parameter TYPES so the LLM picks the
        // right shape (path vs string vs string_list) per field.
        let mut role_lines = String::new();
        for name in catalog.names() {
            let r = catalog.get(name).unwrap();
            let params: Vec<String> = r
                .parameters
                .iter()
                .map(|(k, s)| {
                    let type_tag = match s.param_type {
                        crate::plan::ParameterType::String => "str",
                        crate::plan::ParameterType::Path => "path",
                        crate::plan::ParameterType::StringList => "str[]",
                    };
                    let req = if s.required { "" } else { "?" };
                    format!("{k}{req}: {type_tag}")
                })
                .collect();
            role_lines.push_str(&format!(
                "  {name}: {desc} Params: {{ {params} }}.\n",
                desc = r.system_prompt.lines().next().unwrap_or(""),
                params = params.join(", ")
            ));
        }

        let mut prompt = format!(
            "You produce a Plan for an agent runtime. Output ONLY valid JSON.\n\
             \n\
             Plan JSON schema:\n\
             {{\n\
             \t\"subtasks\": [ {{\"id\": string, \"role\": string, \"params\": object, \"depends_on\": [string]}} ],\n\
             \t\"final_output\": string (path)\n\
             }}\n\
             \n\
             ## Path rules — read this carefully\n\
             \n\
             1. `{{run}}` is a DIRECTORY — the per-run workspace root. Never\n\
                use it as a file path on its own.\n\
             2. Every `path`-typed param that names a FILE must be a full\n\
                path with a filename + extension. Examples:\n\
                  GOOD: `\"workspace\": \"{{run}}/hn.html\"`\n\
                  GOOD: `\"workspace\": \"{{run}}/fetched/raw.txt\"`\n\
                  BAD:  `\"workspace\": \"{{run}}\"`            (directory, not a file)\n\
                  BAD:  `\"workspace\": \"{{run}}/\"`           (trailing slash)\n\
             3. For `string_list` params that hold input paths (e.g. the\n\
                writer's `inputs`), each entry is a full file path and may\n\
                reference another subtask's output by copying that subtask's\n\
                `workspace` verbatim.\n\
             4. **Operator-stated absolute paths stay verbatim.** If the\n\
                GOAL says \"write to /data/report.md\", use `\"output\":\n\
                \"/data/report.md\"` and `\"final_output\": \"/data/report.md\"`.\n\
                Do NOT prefix with `{{run}}`. Operator-stated paths are\n\
                treated as literal filesystem locations the operator\n\
                already controls; runtime subtask paths use `{{run}}`.\n\
             5. `final_output` must match whatever path the writer subtask\n\
                declared as its `output` param. They are the same file.\n\
             \n\
             ## Decomposition rules\n\
             \n\
             - One subtask per independent unit of work. Don't invent\n\
               subtasks that add a redundant layer. If the goal is\n\
               \"fetch X and summarize it\", two subtasks suffice (fetcher +\n\
               writer) — no analyzer step in between. Analyzers are for\n\
               goals that genuinely call out analysis as a separate\n\
               deliverable.\n\
             - Prefer parallelism when subtasks are independent: list each\n\
               in `subtasks` with empty `depends_on`, not as a chain.\n\
             - Use `generalist` only when no specific role fits. Don't\n\
               wrap a writer's work in a generalist step.\n\
             - **Handoff rule.** If a subtask B has `depends_on: [A]`\n\
               and reads A's output, then A MUST have an output-path\n\
               param set (`workspace`, `output`, or equivalent path\n\
               param declared in the role catalog). B's input path\n\
               must copy A's output path verbatim. Never plan a chain\n\
               where the upstream has no declared output path — the\n\
               downstream will not find its input.\n\
             \n\
             ROLE CATALOG:\n\
             {roles}\n\
             GOAL: {goal}\n\n",
            roles = role_lines,
            goal = goal,
        );

        if let (Some(prev), Some(reason)) = (previous, failure_reason) {
            prompt.push_str(
                "PREVIOUS PLAN FAILED. Diagnose the failure below and emit a\n\
                 revised plan that avoids it. Do NOT re-emit the failing\n\
                 subtask verbatim — change what needs to change (a different\n\
                 URL, a different role, a different path shape, or dropping\n\
                 a subtask that cannot succeed). If the failure looks\n\
                 unrecoverable (e.g. the target resource does not exist),\n\
                 produce a plan that reports the failure cleanly instead of\n\
                 retrying the same operation.\n\n",
            );
            prompt.push_str(&format!(
                "Previous plan: {}\n",
                serde_json::to_string(prev).unwrap()
            ));
            prompt.push_str(&format!("Failure reason: {}\n\n", reason));
        }

        prompt.push_str("Emit ONLY the Plan JSON, nothing else.");
        prompt
    }

    async fn call_and_parse(
        &self,
        prompt: &str,
        catalog: &RoleCatalog,
    ) -> Result<Plan, PlannerError> {
        let req = CompletionRequest {
            agent_id: AgentId::new(),
            model: self.model.clone(),
            system: String::new(),
            messages: vec![Message::User {
                content: prompt.to_string(),
            }],
            tools: vec![],
            max_tokens: 4000,
        };
        let resp = self
            .llm
            .complete(req)
            .await
            .map_err(|e| PlannerError::LlmCall(e.to_string()))?;

        let text = resp
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .ok_or_else(|| PlannerError::Malformed("no text block in LLM response".into()))?;

        let json = extract_json(&text).ok_or_else(|| {
            PlannerError::Malformed(format!("no JSON in response: {}", truncate(&text, 200)))
        })?;

        let mut plan: Plan = serde_json::from_str(&json)
            .map_err(|e| PlannerError::Malformed(format!("JSON parse: {e}")))?;

        apply_ttl_fallback(&mut plan, default_task_ttl());

        validate_plan_structure(&plan, catalog)?;

        Ok(plan)
    }
}

pub fn validate_plan_structure(plan: &Plan, catalog: &RoleCatalog) -> Result<(), PlannerError> {
    if plan.final_output.is_empty() {
        return Err(PlannerError::Malformed("final_output missing".into()));
    }
    if plan.subtasks.is_empty() {
        return Err(PlannerError::Malformed("subtasks empty".into()));
    }
    let mut seen = std::collections::HashSet::new();
    for s in &plan.subtasks {
        if !seen.insert(s.id.clone()) {
            return Err(PlannerError::Malformed(format!(
                "duplicate subtask id: {}",
                s.id
            )));
        }
        if catalog.get(&s.role).is_none() {
            return Err(PlannerError::Malformed(format!("unknown role: {}", s.role)));
        }
    }
    for s in &plan.subtasks {
        for d in &s.depends_on {
            if !seen.contains(d) {
                return Err(PlannerError::Malformed(format!(
                    "subtask '{}' depends on unknown id '{}'",
                    s.id, d
                )));
            }
        }
    }
    Ok(())
}

pub fn extract_json(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let bytes = text.as_bytes();
    for i in start..bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}...", &s[..n])
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlannerError {
    #[error("LLM call failed: {0}")]
    LlmCall(String),
    #[error("malformed plan from LLM: {0}")]
    Malformed(String),
}

/// Fill in `ttl: None` on any subtask in `plan` with `fallback`, if
/// `fallback` is Some. Subtasks that already carry an explicit ttl
/// are left untouched. Called by the planner after parsing the
/// LLM-produced JSON; pulled out for direct unit testing.
pub fn apply_ttl_fallback(plan: &mut Plan, fallback: Option<aaos_core::TaskTtl>) {
    let Some(fallback) = fallback else {
        return;
    };
    for s in plan.subtasks.iter_mut() {
        if s.ttl.is_none() {
            s.ttl = Some(fallback.clone());
        }
    }
}

/// Compute each subtask's depth (longest path from any root in the DAG)
/// and subtract it from its `ttl.max_hops`, saturating at 0. Subtasks with
/// `ttl: None` or `max_hops: None` are left untouched.
///
/// Rationale: the plan is pre-expanded at planner time (full DAG up front),
/// so "hops remaining" at launch equals initial_max_hops minus the subtask's
/// depth from roots. A root subtask (depth 0) keeps its full budget; a
/// depth-N subtask sees initial - N. When the result would be 0 or less, we
/// store `max_hops = 0` so `spawn_subtask`'s refuse-on-zero check fires
/// cleanly and emits `SubtaskTtlExpired { reason: "hops_exhausted" }`.
///
/// This is the single call site that the `TaskTtl::decrement_hops` method
/// in aaos-core was intended to support; we do the decrement in bulk here
/// rather than per-spawn because the plan's per-subtask `ttl` field doesn't
/// carry across spawns (each subtask gets its own filled-in copy from
/// `apply_ttl_fallback`, so a per-spawn decrement has no place to store
/// state for the child).
pub fn apply_depth_to_hops(plan: &mut Plan) {
    use std::collections::HashMap;

    // Build id -> depends_on map keyed by index so we can fill depths in
    // topo order without mutating the subtasks list we're iterating.
    let id_to_idx: HashMap<String, usize> = plan
        .subtasks
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.clone(), i))
        .collect();

    // depth[i] = longest path from any root to subtask i. Compute by
    // repeatedly relaxing until stable. This is O(subtasks^2) worst case
    // but plans are tiny (< 20 subtasks in practice), and it naturally
    // tolerates any subtask ordering in `plan.subtasks` without requiring
    // a pre-sort. Unknown deps would have been caught by topo_batches; we
    // silently skip them here to avoid a second error surface.
    let n = plan.subtasks.len();
    let mut depth: Vec<u32> = vec![0; n];
    let mut changed = true;
    let mut iterations = 0;
    while changed && iterations <= n {
        changed = false;
        iterations += 1;
        for (i, s) in plan.subtasks.iter().enumerate() {
            let mut max_dep_depth: Option<u32> = None;
            for d in &s.depends_on {
                if let Some(&j) = id_to_idx.get(d) {
                    let cand = depth[j] + 1;
                    max_dep_depth = Some(max_dep_depth.map_or(cand, |m| m.max(cand)));
                }
            }
            let new_depth = max_dep_depth.unwrap_or(0);
            if new_depth > depth[i] {
                depth[i] = new_depth;
                changed = true;
            }
        }
    }

    for (i, s) in plan.subtasks.iter_mut().enumerate() {
        let Some(ttl) = s.ttl.as_mut() else { continue };
        let Some(hops) = ttl.max_hops else { continue };
        ttl.max_hops = Some(hops.saturating_sub(depth[i]));
    }
}

/// Build a TaskTtl from environment defaults. Returns None if both
/// `AAOS_DEFAULT_TASK_TTL_HOPS` and `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S`
/// are unset. Called by the planner when a subtask arrives from the LLM
/// without an explicit `ttl` field. A single-env-var setup returns a
/// TaskTtl with the other field left as None (honoring "unset = no
/// bound on that axis").
pub fn default_task_ttl() -> Option<aaos_core::TaskTtl> {
    default_task_ttl_with_env(
        std::env::var("AAOS_DEFAULT_TASK_TTL_HOPS").ok(),
        std::env::var("AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S").ok(),
    )
}

/// Helper for testing: accepts optional env var values directly.
fn default_task_ttl_with_env(
    hops_str: Option<String>,
    clock_str: Option<String>,
) -> Option<aaos_core::TaskTtl> {
    let max_hops = hops_str.and_then(|v| v.parse::<u32>().ok());
    let max_wall_clock = clock_str
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs);

    if max_hops.is_none() && max_wall_clock.is_none() {
        return None;
    }
    Some(aaos_core::TaskTtl {
        max_hops,
        max_wall_clock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{
        default_escalation_signals, ParameterSchema, ParameterType, Role, RoleBudget, RoleRetry,
        Subtask,
    };
    use std::collections::HashMap;

    fn catalog_with_fetcher() -> RoleCatalog {
        let r = Role {
            name: "fetcher".into(),
            model: "deepseek-chat".into(),
            parameters: HashMap::from([
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
            system_prompt: "fetcher".into(),
            message_template: "fetch {url} to {workspace}".into(),
            budget: RoleBudget {
                max_input_tokens: 20000,
                max_output_tokens: 2000,
            },
            retry: RoleRetry {
                max_attempts: 1,
                on: vec![],
            },
            priority: 128,
            model_ladder: vec![],
            escalate_on: default_escalation_signals(),
            scaffold: None,
        };
        let mut cat = RoleCatalog::default();
        cat.roles_mut().insert("fetcher".into(), r);
        cat
    }

    #[test]
    fn extract_json_from_pure_json() {
        let j = extract_json(r#"{"a":1}"#).unwrap();
        assert_eq!(j, r#"{"a":1}"#);
    }

    #[test]
    fn extract_json_from_prose_wrapper() {
        let j = extract_json("Here's the plan:\n{\"a\":1}\nThat's it.").unwrap();
        assert_eq!(j, r#"{"a":1}"#);
    }

    #[test]
    fn validate_ok_on_good_plan() {
        let cat = catalog_with_fetcher();
        let p = Plan {
            subtasks: vec![Subtask {
                id: "a".into(),
                role: "fetcher".into(),
                params: serde_json::json!({}),
                depends_on: vec![],
                ttl: None,
            }],
            final_output: "/out".into(),
        };
        validate_plan_structure(&p, &cat).unwrap();
    }

    #[test]
    fn validate_rejects_unknown_role() {
        let cat = catalog_with_fetcher();
        let p = Plan {
            subtasks: vec![Subtask {
                id: "a".into(),
                role: "wizard".into(),
                params: serde_json::json!({}),
                depends_on: vec![],
                ttl: None,
            }],
            final_output: "/out".into(),
        };
        let err = validate_plan_structure(&p, &cat).unwrap_err();
        assert!(matches!(err, PlannerError::Malformed(_)));
    }

    #[test]
    fn validate_rejects_empty_subtasks() {
        let cat = catalog_with_fetcher();
        let p = Plan {
            subtasks: vec![],
            final_output: "/out".into(),
        };
        assert!(validate_plan_structure(&p, &cat).is_err());
    }

    #[test]
    fn validate_rejects_dup_id() {
        let cat = catalog_with_fetcher();
        let p = Plan {
            subtasks: vec![
                Subtask {
                    id: "a".into(),
                    role: "fetcher".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                },
                Subtask {
                    id: "a".into(),
                    role: "fetcher".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                },
            ],
            final_output: "/out".into(),
        };
        assert!(validate_plan_structure(&p, &cat).is_err());
    }

    #[test]
    fn default_ttl_from_env_populates_both_fields() {
        use std::time::Duration;

        let t = default_task_ttl_with_env(Some("5".into()), Some("30".into()));
        let ttl = t.expect("both env vars set => Some(TaskTtl)");
        assert_eq!(ttl.max_hops, Some(5));
        assert_eq!(ttl.max_wall_clock, Some(Duration::from_secs(30)));
    }

    #[test]
    fn default_ttl_returns_none_when_no_env() {
        let result = default_task_ttl_with_env(None, None);
        assert!(result.is_none());
    }

    #[test]
    fn apply_ttl_fallback_fills_only_missing_ttls() {
        use aaos_core::TaskTtl;
        use std::time::Duration;

        let fallback = TaskTtl {
            max_hops: Some(5),
            max_wall_clock: Some(Duration::from_secs(60)),
        };
        let explicit = TaskTtl {
            max_hops: Some(2),
            max_wall_clock: None,
        };

        let mut plan = Plan {
            subtasks: vec![
                Subtask {
                    id: "has_ttl".into(),
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: Some(explicit.clone()),
                },
                Subtask {
                    id: "no_ttl".into(),
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                },
            ],
            final_output: "out".into(),
        };

        apply_ttl_fallback(&mut plan, Some(fallback.clone()));

        assert_eq!(
            plan.subtasks[0].ttl.as_ref(),
            Some(&explicit),
            "explicit ttl must be preserved, not overwritten"
        );
        assert_eq!(
            plan.subtasks[1].ttl.as_ref(),
            Some(&fallback),
            "missing ttl must be filled with fallback"
        );
    }

    #[test]
    fn apply_ttl_fallback_with_none_is_noop() {
        let mut plan = Plan {
            subtasks: vec![Subtask {
                id: "no_ttl".into(),
                role: "writer".into(),
                params: serde_json::json!({}),
                depends_on: vec![],
                ttl: None,
            }],
            final_output: "out".into(),
        };

        apply_ttl_fallback(&mut plan, None);

        assert!(
            plan.subtasks[0].ttl.is_none(),
            "None fallback must leave subtask ttls untouched"
        );
    }

    #[test]
    fn apply_depth_to_hops_chain_decrements_along_longest_path() {
        // A -> B -> C chain, max_hops=3 on all. Depths: 0, 1, 2. After
        // decrement: 3, 2, 1.
        let ttl = aaos_core::TaskTtl {
            max_hops: Some(3),
            max_wall_clock: None,
        };
        let mut plan = Plan {
            subtasks: vec![
                Subtask {
                    id: "a".into(),
                    role: "x".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: Some(ttl.clone()),
                },
                Subtask {
                    id: "b".into(),
                    role: "x".into(),
                    params: serde_json::json!({}),
                    depends_on: vec!["a".into()],
                    ttl: Some(ttl.clone()),
                },
                Subtask {
                    id: "c".into(),
                    role: "x".into(),
                    params: serde_json::json!({}),
                    depends_on: vec!["b".into()],
                    ttl: Some(ttl.clone()),
                },
            ],
            final_output: "c".into(),
        };

        apply_depth_to_hops(&mut plan);

        assert_eq!(plan.subtasks[0].ttl.as_ref().unwrap().max_hops, Some(3));
        assert_eq!(plan.subtasks[1].ttl.as_ref().unwrap().max_hops, Some(2));
        assert_eq!(plan.subtasks[2].ttl.as_ref().unwrap().max_hops, Some(1));
    }

    #[test]
    fn apply_depth_to_hops_saturates_at_zero() {
        // max_hops=1, chain of 3 — C's depth=2 > 1, should saturate to 0.
        let ttl = aaos_core::TaskTtl {
            max_hops: Some(1),
            max_wall_clock: None,
        };
        let mut plan = Plan {
            subtasks: vec![
                Subtask {
                    id: "a".into(),
                    role: "x".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: Some(ttl.clone()),
                },
                Subtask {
                    id: "b".into(),
                    role: "x".into(),
                    params: serde_json::json!({}),
                    depends_on: vec!["a".into()],
                    ttl: Some(ttl.clone()),
                },
                Subtask {
                    id: "c".into(),
                    role: "x".into(),
                    params: serde_json::json!({}),
                    depends_on: vec!["b".into()],
                    ttl: Some(ttl.clone()),
                },
            ],
            final_output: "c".into(),
        };

        apply_depth_to_hops(&mut plan);

        assert_eq!(plan.subtasks[0].ttl.as_ref().unwrap().max_hops, Some(1));
        assert_eq!(plan.subtasks[1].ttl.as_ref().unwrap().max_hops, Some(0));
        assert_eq!(plan.subtasks[2].ttl.as_ref().unwrap().max_hops, Some(0));
    }

    #[test]
    fn apply_depth_to_hops_uses_longest_path_not_min() {
        // Diamond: A at 0, B depends on A (depth 1), C depends on A (depth 1),
        // D depends on B AND C (depth 2). max_hops=5 -> 5,4,4,3.
        let ttl = aaos_core::TaskTtl {
            max_hops: Some(5),
            max_wall_clock: None,
        };
        let mut plan = Plan {
            subtasks: vec![
                Subtask {
                    id: "a".into(),
                    role: "x".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: Some(ttl.clone()),
                },
                Subtask {
                    id: "b".into(),
                    role: "x".into(),
                    params: serde_json::json!({}),
                    depends_on: vec!["a".into()],
                    ttl: Some(ttl.clone()),
                },
                Subtask {
                    id: "c".into(),
                    role: "x".into(),
                    params: serde_json::json!({}),
                    depends_on: vec!["a".into()],
                    ttl: Some(ttl.clone()),
                },
                Subtask {
                    id: "d".into(),
                    role: "x".into(),
                    params: serde_json::json!({}),
                    depends_on: vec!["b".into(), "c".into()],
                    ttl: Some(ttl.clone()),
                },
            ],
            final_output: "d".into(),
        };

        apply_depth_to_hops(&mut plan);

        assert_eq!(plan.subtasks[0].ttl.as_ref().unwrap().max_hops, Some(5));
        assert_eq!(plan.subtasks[1].ttl.as_ref().unwrap().max_hops, Some(4));
        assert_eq!(plan.subtasks[2].ttl.as_ref().unwrap().max_hops, Some(4));
        assert_eq!(plan.subtasks[3].ttl.as_ref().unwrap().max_hops, Some(3));
    }

    #[test]
    fn apply_depth_to_hops_leaves_none_ttl_untouched() {
        let mut plan = Plan {
            subtasks: vec![Subtask {
                id: "a".into(),
                role: "x".into(),
                params: serde_json::json!({}),
                depends_on: vec![],
                ttl: None,
            }],
            final_output: "a".into(),
        };
        apply_depth_to_hops(&mut plan);
        assert!(plan.subtasks[0].ttl.is_none());
    }
}
