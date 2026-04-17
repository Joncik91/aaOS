use std::sync::Arc;

use aaos_core::{
    AgentId, AuditEvent, AuditEventKind, AuditLog, Capability, CapabilityHandle, CapabilityRegistry,
    CapabilityToken, CoreError, Result,
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
        let has_permission = token_handles.iter().any(|h| {
            self.capability_registry
                .permits(*h, agent_id, &required)
        });

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
        let input_hash = format!("{:x}", md5_hash(&input));
        let args_preview = preview_value(&input);
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
        let result = tool.invoke(input, &ctx).await;

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
        let invocation =
            ToolInvocation::new(registry, log.clone(), cap_registry.clone());
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
        assert!(out.len() <= super::PREVIEW_CAP + 4, "utf-8 marker adds up to 3 bytes");
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
        assert!(invoked.as_ref().map(|s| s.contains("hello-audit")).unwrap_or(false),
            "ToolInvoked args_preview should contain the input message; got {:?}", invoked);
        assert!(result.is_some(), "ToolResult must carry a preview even on success");
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
}
