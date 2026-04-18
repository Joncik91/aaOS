use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aaos_core::{
    AgentId, AuditEvent, AuditEventKind, AuditLog, Capability, CapabilityHandle,
    CapabilityRegistry, CapabilityToken, CoreError, Result,
};
use serde_json::Value;

use crate::context::InvocationContext;
use crate::registry::ToolRegistry;

/// Handles tool invocations with capability enforcement and audit logging.
///
/// Every tool call goes through the invocation layer, which:
/// 1. Checks the agent's capability token permits the tool
/// 2. Validates input against the tool's schema
/// 3. Invokes the tool
/// 4. Logs the invocation to the audit trail
pub struct ToolInvocation {
    registry: Arc<ToolRegistry>,
    audit_log: Arc<dyn AuditLog>,
    capability_registry: Arc<CapabilityRegistry>,
    repeat_counts: Mutex<HashMap<(AgentId, String, u64), u32>>,
}

impl ToolInvocation {
    pub fn new(
        registry: Arc<ToolRegistry>,
        audit_log: Arc<dyn AuditLog>,
        capability_registry: Arc<CapabilityRegistry>,
    ) -> Self {
        Self {
            registry,
            audit_log,
            capability_registry,
            repeat_counts: Mutex::new(HashMap::new()),
        }
    }

    /// Invoke a tool on behalf of an agent, enforcing capabilities.
    pub async fn invoke(
        &self,
        agent_id: AgentId,
        tool_name: &str,
        input: Value,
        token_handles: &[CapabilityHandle],
    ) -> Result<Value> {
        // Check capability
        let required = Capability::ToolInvoke {
            tool_name: tool_name.to_string(),
        };
        let has_permission = token_handles
            .iter()
            .any(|h| self.capability_registry.permits(*h, agent_id, &required));

        if !has_permission {
            self.audit_log.record(AuditEvent::new(
                agent_id,
                AuditEventKind::CapabilityDenied {
                    capability: required,
                    reason: format!("agent lacks tool:{tool_name} capability"),
                },
            ));
            return Err(CoreError::CapabilityDenied {
                agent_id,
                capability: Capability::ToolInvoke {
                    tool_name: tool_name.to_string(),
                },
                reason: "tool invocation not permitted".into(),
            });
        }

        // Get the tool
        let tool = self.registry.get(tool_name)?;

        // Log invocation
        let input_hash_u64 = md5_hash(&input);
        let input_hash = format!("{:x}", input_hash_u64);
        let args_preview = preview_value(&input);

        // Track repeat counts. Map grows while agentd runs; cap at
        // REPEAT_COUNTS_MAX entries and evict arbitrary entries when full
        // to bound memory. Eviction is coarse but correctness-preserving:
        // an evicted entry just means a cold tool call won't see the
        // repeat-guard hint until it crosses the threshold again.
        const REPEAT_COUNTS_MAX: usize = 1024;
        let repeat_key = (agent_id, tool_name.to_string(), input_hash_u64);
        let attempt_count: u32 = {
            let mut counts = self.repeat_counts.lock().unwrap_or_else(|e| e.into_inner());
            if counts.len() >= REPEAT_COUNTS_MAX && !counts.contains_key(&repeat_key) {
                // Drop a quarter of the map to amortize eviction cost.
                let drop_n = REPEAT_COUNTS_MAX / 4;
                let victims: Vec<_> = counts.keys().take(drop_n).cloned().collect();
                for k in victims {
                    counts.remove(&k);
                }
            }
            let entry = counts.entry(repeat_key.clone()).or_insert(0);
            *entry += 1;
            *entry
        };
        let threshold: u32 = std::env::var("AAOS_TOOL_REPEAT_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        let is_repeat = attempt_count >= threshold;
        if is_repeat {
            self.audit_log.record(AuditEvent::new(
                agent_id,
                AuditEventKind::ToolRepeat {
                    tool: tool_name.to_string(),
                    input_hash: input_hash.clone(),
                    attempt_count,
                },
            ));
        }

        self.audit_log.record(AuditEvent::new(
            agent_id,
            AuditEventKind::ToolInvoked {
                tool: tool_name.to_string(),
                input_hash,
                args_preview: Some(args_preview),
            },
        ));

        // Filter handles relevant to this tool
        let filtered_handles: Vec<CapabilityHandle> = token_handles
            .iter()
            .filter(|h| {
                // We need to check what capability this handle's token represents.
                // Use inspect() — available in test/debug builds.
                // For production, we pass all handles and let the tool's capability
                // check via permits() filter appropriately.
                // Actually, the original filtering was by token capability type.
                // With handles, we can't inspect the token's capability type in production.
                // The solution: pass ALL handles; the tool's own permits() call will
                // correctly deny handles that don't match the requested capability type.
                let _ = h;
                true
            })
            .cloned()
            .collect();

        let ctx = InvocationContext {
            agent_id,
            tokens: filtered_handles,
            capability_registry: self.capability_registry.clone(),
        };

        // Invoke with context
        let mut result = tool.invoke(input, &ctx).await;

        // Log result (with a bounded preview for operator observability).
        let result_preview = match &result {
            Ok(v) => Some(preview_value(v)),
            Err(e) => Some(preview_str(&e.to_string())),
        };
        self.audit_log.record(AuditEvent::new(
            agent_id,
            AuditEventKind::ToolResult {
                tool: tool_name.to_string(),
                success: result.is_ok(),
                result_preview,
            },
        ));

        // Inject repeat guard hint if threshold reached
        if is_repeat {
            // Phase F-b sub-project 2: emit a dedicated audit event so the
            // plan executor can detect the signal from the broadcast
            // stream without introspecting tool-result JSON. The existing
            // `_repeat_guard` hint stays — it's LLM-visible and is what
            // actually nudges the agent.
            self.audit_log.record(AuditEvent::new(
                agent_id,
                AuditEventKind::ToolRepeatGuardFired {
                    agent_id,
                    tool: tool_name.to_string(),
                    attempt_count,
                },
            ));

            let hint = format!(
                "You have called `{}` with these exact arguments {} times in this subtask. The previous attempts returned the same result. Try different arguments or a different tool.",
                tool_name, attempt_count
            );
            if let Ok(ref mut v) = result {
                if let Some(obj) = v.as_object_mut() {
                    use serde_json::json;
                    obj.insert(
                        "_repeat_guard".to_string(),
                        json!({ "attempt_count": attempt_count, "hint": hint }),
                    );
                }
            }
            // For Err, we cannot mutate the CoreError in place. The audit
            // event already fired above; the LLM will see the error string
            // on its own. We intentionally don't rewrite the error to
            // include the hint — the audit event + streaming output is the
            // operator-side signal. The LLM's recovery path for errors
            // happens already via the existing tool.invoke() error return.
        }

        // Observability: surface tool failures in the daemon log so operators
        // can diagnose without replaying the LLM's tool_call response text.
        // The audit event only carries success/false — not the actual error
        // string — by design (audit events are a stable schema). The tracing
        // log is the right place for free-form diagnostic detail.
        if let Err(ref e) = result {
            tracing::warn!(
                tool = tool_name,
                agent_id = %agent_id,
                error = %e,
                "tool invocation failed",
            );
        }

        result
    }
}

/// Simple hash for audit logging (not cryptographic).
fn md5_hash(value: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.to_string().hash(&mut hasher);
    hasher.finish()
}

const PREVIEW_CAP: usize = 200;

/// Produces a bounded, operator-readable summary of a JSON value for audit
/// events. Strings get a direct byte-capped excerpt; objects and arrays
/// get their compact JSON representation byte-capped. Null collapses to
/// "null". Keeps the preview stable enough to be useful without leaking
/// large payloads into every audit consumer.
fn preview_value(v: &Value) -> String {
    match v {
        Value::String(s) => preview_str(s),
        Value::Null => "null".into(),
        _ => preview_str(&v.to_string()),
    }
}

/// Truncates a string at `PREVIEW_CAP` bytes, respecting UTF-8 boundaries
/// (never splits a codepoint). Appends a trailing `…` marker when truncated.
fn preview_str(s: &str) -> String {
    if s.len() <= PREVIEW_CAP {
        return s.to_string();
    }
    let mut end = PREVIEW_CAP;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
impl ToolInvocation {
    /// Test-only: peek the current repeat count for a key. Returns 0
    /// if the key has never been seen.
    pub fn test_repeat_count(
        &self,
        agent_id: aaos_core::AgentId,
        tool: &str,
        input: &Value,
    ) -> u32 {
        let hash = md5_hash(input);
        let counts = self.repeat_counts.lock().unwrap_or_else(|e| e.into_inner());
        counts
            .get(&(agent_id, tool.to_string(), hash))
            .copied()
            .unwrap_or(0)
    }

    /// Test-only: the current size of the repeat-counts map.
    #[cfg(test)]
    pub fn test_repeat_counts_len(&self) -> usize {
        self.repeat_counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::EchoTool;
    use aaos_core::{CapabilityRegistry, Constraints, InMemoryAuditLog};
    use std::sync::Arc;

    fn setup() -> (
        ToolInvocation,
        AgentId,
        Vec<CapabilityHandle>,
        Arc<InMemoryAuditLog>,
        Arc<CapabilityRegistry>,
    ) {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(EchoTool));
        let log = Arc::new(InMemoryAuditLog::new());
        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "echo".into(),
            },
            Constraints::default(),
        );
        let cap_registry = Arc::new(CapabilityRegistry::new());
        let handle = cap_registry.insert(agent_id, token);
        let invocation = ToolInvocation::new(registry, log.clone(), cap_registry.clone());
        (invocation, agent_id, vec![handle], log, cap_registry)
    }

    #[tokio::test]
    async fn invoke_with_capability() {
        let (invocation, agent_id, handles, log, _cap_registry) = setup();
        let result = invocation
            .invoke(
                agent_id,
                "echo",
                serde_json::json!({"message": "hi"}),
                &handles,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({"message": "hi"}));
        assert!(log.len() >= 2); // invocation + result
    }

    #[tokio::test]
    async fn invoke_without_capability() {
        let (invocation, agent_id, _handles, log, _cap_registry) = setup();
        let result = invocation
            .invoke(agent_id, "echo", serde_json::json!({}), &[]) // no handles
            .await;
        assert!(result.is_err());
        // Should have logged the denial
        assert!(!log.is_empty());
    }

    // ---- preview helpers ----

    #[test]
    fn preview_str_short_passes_through() {
        assert_eq!(super::preview_str("hello"), "hello");
    }

    #[test]
    fn preview_str_long_truncates_with_marker() {
        let long = "a".repeat(500);
        let out = super::preview_str(&long);
        assert!(out.ends_with('…'));
        assert!(
            out.len() <= super::PREVIEW_CAP + 4,
            "utf-8 marker adds up to 3 bytes"
        );
    }

    #[test]
    fn preview_str_respects_utf8_boundary() {
        // 100 chars, each 3 bytes (kanji), total 300 bytes — forces
        // truncation mid-cap and must not split a codepoint.
        let s: String = "日".repeat(100);
        let out = super::preview_str(&s);
        // Round-trip through String ensures no invalid utf-8 slipped through.
        assert!(out.chars().count() > 0);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn preview_value_handles_string() {
        let v = serde_json::json!("simple string");
        assert_eq!(super::preview_value(&v), "simple string");
    }

    #[test]
    fn preview_value_handles_object() {
        let v = serde_json::json!({"url": "https://example.com", "method": "GET"});
        let out = super::preview_value(&v);
        assert!(out.contains("example.com"));
    }

    #[test]
    fn preview_value_handles_null() {
        assert_eq!(super::preview_value(&serde_json::Value::Null), "null");
    }

    #[tokio::test]
    async fn invoke_populates_args_and_result_preview() {
        let (invocation, agent_id, handles, log, _cap_registry) = setup();
        let _ = invocation
            .invoke(
                agent_id,
                "echo",
                serde_json::json!({"message": "hello-audit"}),
                &handles,
            )
            .await
            .unwrap();
        let events = log.events();
        let invoked = events.iter().find_map(|e| match &e.event {
            AuditEventKind::ToolInvoked { args_preview, .. } => args_preview.clone(),
            _ => None,
        });
        let result = events.iter().find_map(|e| match &e.event {
            AuditEventKind::ToolResult { result_preview, .. } => result_preview.clone(),
            _ => None,
        });
        assert!(
            invoked
                .as_ref()
                .map(|s| s.contains("hello-audit"))
                .unwrap_or(false),
            "ToolInvoked args_preview should contain the input message; got {:?}",
            invoked
        );
        assert!(
            result.is_some(),
            "ToolResult must carry a preview even on success"
        );
    }

    #[tokio::test]
    async fn invoke_nonexistent_tool() {
        let (invocation, agent_id, _handles, _log, cap_registry) = setup();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "*".into(),
            },
            Constraints::default(),
        );
        let handle = cap_registry.insert(agent_id, token);
        let result = invocation
            .invoke(agent_id, "nonexistent", serde_json::json!({}), &[handle])
            .await;
        assert!(result.is_err());
    }

    // ---- repeat-guard tests (run 11 feature) ----

    #[tokio::test]
    async fn first_two_calls_have_no_repeat_guard() {
        let (invocation, agent_id, handles, _log, _) = setup();
        let input = serde_json::json!({"message": "hello"});
        for _ in 0..2 {
            let result = invocation
                .invoke(agent_id, "echo", input.clone(), &handles)
                .await
                .unwrap();
            assert!(
                result.get("_repeat_guard").is_none(),
                "first two calls must not carry a repeat_guard; got {:?}",
                result
            );
        }
    }

    #[tokio::test]
    async fn third_call_injects_repeat_guard() {
        let (invocation, agent_id, handles, _log, _) = setup();
        let input = serde_json::json!({"message": "same"});
        let _ = invocation
            .invoke(agent_id, "echo", input.clone(), &handles)
            .await;
        let _ = invocation
            .invoke(agent_id, "echo", input.clone(), &handles)
            .await;
        let third = invocation
            .invoke(agent_id, "echo", input.clone(), &handles)
            .await
            .unwrap();
        let guard = third
            .get("_repeat_guard")
            .expect("third call must carry _repeat_guard");
        assert_eq!(guard["attempt_count"], 3);
        assert!(
            guard["hint"].as_str().unwrap().contains("echo"),
            "hint should name the tool; got {:?}",
            guard["hint"]
        );
    }

    #[tokio::test]
    async fn repeat_guard_emits_audit_event_on_third_call() {
        use aaos_core::AuditEventKind;

        let (invocation, agent_id, handles, log, _) = setup();
        let input = serde_json::json!({"message": "audit-test"});

        // Invoke the same tool with identical args 3 times
        for _ in 0..3 {
            let _ = invocation
                .invoke(agent_id, "echo", input.clone(), &handles)
                .await;
        }

        // Filter for ToolRepeatGuardFired events with tool=="echo" and attempt_count >= 3
        let repeat_events: Vec<_> = log
            .events()
            .into_iter()
            .filter(|e| {
                matches!(
                    &e.event,
                    AuditEventKind::ToolRepeatGuardFired {
                        tool,
                        attempt_count,
                        ..
                    } if tool == "echo" && *attempt_count >= 3
                )
            })
            .collect();

        assert!(
            !repeat_events.is_empty(),
            "expected at least one ToolRepeatGuardFired event at attempt 3+"
        );
    }

    #[tokio::test]
    async fn different_input_hash_resets_counter() {
        let (invocation, agent_id, handles, _log, _) = setup();
        let a = serde_json::json!({"message": "a"});
        let b = serde_json::json!({"message": "b"});
        let _ = invocation
            .invoke(agent_id, "echo", a.clone(), &handles)
            .await;
        let _ = invocation
            .invoke(agent_id, "echo", a.clone(), &handles)
            .await;
        let _ = invocation
            .invoke(agent_id, "echo", b.clone(), &handles)
            .await;
        assert_eq!(invocation.test_repeat_count(agent_id, "echo", &a), 2);
        assert_eq!(invocation.test_repeat_count(agent_id, "echo", &b), 1);
    }

    #[tokio::test]
    async fn different_agent_resets_counter() {
        let (invocation, agent_a, handles_a, _log, cap_registry) = setup();
        let input = serde_json::json!({"message": "x"});
        // Run agent_a three times so it trips the guard.
        for _ in 0..3 {
            let _ = invocation
                .invoke(agent_a, "echo", input.clone(), &handles_a)
                .await;
        }
        // Agent B: fresh agent, fresh grant, fresh counter.
        let agent_b = AgentId::new();
        let token_b = CapabilityToken::issue(
            agent_b,
            Capability::ToolInvoke {
                tool_name: "echo".into(),
            },
            Constraints::default(),
        );
        let handle_b = cap_registry.insert(agent_b, token_b);
        assert_eq!(invocation.test_repeat_count(agent_b, "echo", &input), 0);
        let first_for_b = invocation
            .invoke(agent_b, "echo", input.clone(), &[handle_b])
            .await
            .unwrap();
        assert!(
            first_for_b.get("_repeat_guard").is_none(),
            "agent B's first call must not see agent A's count"
        );
        assert_eq!(invocation.test_repeat_count(agent_b, "echo", &input), 1);
    }

    #[tokio::test]
    async fn repeat_counts_map_is_bounded() {
        // Hammer the invocation with 2000 distinct inputs and confirm the
        // map never exceeds the REPEAT_COUNTS_MAX cap of 1024.
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(EchoTool));
        let audit_log: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let cap_registry = Arc::new(CapabilityRegistry::new());
        let invocation = ToolInvocation::new(registry, audit_log, cap_registry.clone());

        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "echo".into(),
            },
            Constraints::default(),
        );
        let handle = cap_registry.insert(agent_id, token);

        for i in 0..2000u32 {
            let input = serde_json::json!({ "message": format!("msg-{i}") });
            let _ = invocation
                .invoke(agent_id, "echo", input, &[handle])
                .await
                .unwrap();
        }

        let len = invocation.test_repeat_counts_len();
        assert!(len <= 1024, "map size {len} exceeded cap of 1024");
        // Eviction in quarters means the map can dip as low as 3/4 of cap
        // right after a cleanup; it should be populated, not empty.
        assert!(len > 0, "map should not be empty after 2000 calls");
    }
}
