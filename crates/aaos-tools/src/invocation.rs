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
        self.audit_log.record(AuditEvent::new(
            agent_id,
            AuditEventKind::ToolInvoked {
                tool: tool_name.to_string(),
                input_hash,
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

        // Log result
        self.audit_log.record(AuditEvent::new(
            agent_id,
            AuditEventKind::ToolResult {
                tool: tool_name.to_string(),
                success: result.is_ok(),
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
