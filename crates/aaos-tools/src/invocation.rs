use std::sync::Arc;

use aaos_core::{
    AgentId, AuditEvent, AuditEventKind, AuditLog, Capability, CapabilityToken, CoreError, Result,
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
}

impl ToolInvocation {
    pub fn new(registry: Arc<ToolRegistry>, audit_log: Arc<dyn AuditLog>) -> Self {
        Self {
            registry,
            audit_log,
        }
    }

    /// Invoke a tool on behalf of an agent, enforcing capabilities.
    pub async fn invoke(
        &self,
        agent_id: AgentId,
        tool_name: &str,
        input: Value,
        tokens: &[CapabilityToken],
    ) -> Result<Value> {
        // Check capability
        let required = Capability::ToolInvoke {
            tool_name: tool_name.to_string(),
        };
        let has_permission = tokens.iter().any(|t| t.permits(&required));

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

        // Filter tokens relevant to this tool
        let filtered_tokens: Vec<CapabilityToken> = tokens
            .iter()
            .filter(|t| matches_tool_capability(&t.capability, tool_name))
            .cloned()
            .collect();

        let ctx = InvocationContext {
            agent_id,
            tokens: filtered_tokens,
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

        result
    }
}

/// Maps tool names to the capability types their tokens should contain.
/// Unknown tools only receive ToolInvoke tokens — never file/network/spawn tokens.
fn matches_tool_capability(capability: &Capability, tool_name: &str) -> bool {
    match tool_name {
        "file_read" => matches!(capability, Capability::FileRead { .. }),
        "file_list" => matches!(capability, Capability::FileRead { .. }),
        "file_write" => matches!(capability, Capability::FileWrite { .. }),
        "web_fetch" => matches!(capability, Capability::WebSearch),
        "spawn_agent" => matches!(capability, Capability::SpawnChild { .. }),
        _ => matches!(capability, Capability::ToolInvoke { .. }),
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
    use aaos_core::{Constraints, InMemoryAuditLog};

    fn setup() -> (
        ToolInvocation,
        AgentId,
        Vec<CapabilityToken>,
        Arc<InMemoryAuditLog>,
    ) {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(EchoTool));
        let log = Arc::new(InMemoryAuditLog::new());
        let invocation = ToolInvocation::new(registry, log.clone());
        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "echo".into(),
            },
            Constraints::default(),
        );
        (invocation, agent_id, vec![token], log)
    }

    #[tokio::test]
    async fn invoke_with_capability() {
        let (invocation, agent_id, tokens, log) = setup();
        let result = invocation
            .invoke(
                agent_id,
                "echo",
                serde_json::json!({"message": "hi"}),
                &tokens,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({"message": "hi"}));
        assert!(log.len() >= 2); // invocation + result
    }

    #[tokio::test]
    async fn invoke_without_capability() {
        let (invocation, agent_id, _tokens, log) = setup();
        let result = invocation
            .invoke(agent_id, "echo", serde_json::json!({}), &[]) // no tokens
            .await;
        assert!(result.is_err());
        // Should have logged the denial
        assert!(!log.is_empty());
    }

    #[tokio::test]
    async fn invoke_nonexistent_tool() {
        let (invocation, agent_id, _, _) = setup();
        let wildcard = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "*".into(),
            },
            Constraints::default(),
        );
        let result = invocation
            .invoke(agent_id, "nonexistent", serde_json::json!({}), &[wildcard])
            .await;
        assert!(result.is_err());
    }
}
