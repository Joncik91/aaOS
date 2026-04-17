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

        let plan: Plan = serde_json::from_str(&json)
            .map_err(|e| PlannerError::Malformed(format!("JSON parse: {e}")))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{ParameterSchema, ParameterType, Role, RoleBudget, RoleRetry, Subtask};
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
                },
                Subtask {
                    id: "a".into(),
                    role: "fetcher".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                },
            ],
            final_output: "/out".into(),
        };
        assert!(validate_plan_structure(&p, &cat).is_err());
    }
}
