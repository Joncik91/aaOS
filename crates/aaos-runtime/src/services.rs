use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use aaos_core::{
    AgentId, AgentServices, ApprovalResult, ApprovalService, AuditEvent, AuditEventKind, AuditLog,
    Capability, CoreError, Result, TokenUsage, ToolDefinition,
};
use aaos_ipc::{McpMessage, MessageRouter};
use aaos_tools::{ToolInvocation, ToolRegistry};

use crate::registry::AgentRegistry;

/// In-process implementation of AgentServices.
///
/// Delegates to existing registry and tool subsystems with the same
/// capability checks and audit logging that the future socket implementation will use.
pub struct InProcessAgentServices {
    registry: Arc<AgentRegistry>,
    tool_invocation: Arc<ToolInvocation>,
    tool_registry: Arc<ToolRegistry>,
    audit_log: Arc<dyn AuditLog>,
    router: Arc<MessageRouter>,
    approval_service: Arc<dyn ApprovalService>,
}

impl InProcessAgentServices {
    pub fn new(
        registry: Arc<AgentRegistry>,
        tool_invocation: Arc<ToolInvocation>,
        tool_registry: Arc<ToolRegistry>,
        audit_log: Arc<dyn AuditLog>,
        router: Arc<MessageRouter>,
        approval_service: Arc<dyn ApprovalService>,
    ) -> Self {
        Self {
            registry,
            tool_invocation,
            tool_registry,
            audit_log,
            router,
            approval_service,
        }
    }
}

#[async_trait]
impl AgentServices for InProcessAgentServices {
    async fn invoke_tool(&self, agent_id: AgentId, tool: &str, input: Value) -> Result<Value> {
        // Check if this tool requires approval
        if let Ok(manifest) = self.registry.get_manifest(agent_id) {
            if manifest.approval_required.contains(&tool.to_string()) {
                let result = self
                    .approval_service
                    .request(
                        agent_id,
                        manifest.name.clone(),
                        format!("Agent '{}' wants to invoke tool '{}'", manifest.name, tool),
                        Some(tool.to_string()),
                        Some(input.clone()),
                    )
                    .await?;

                match result {
                    ApprovalResult::Approved => {
                        self.audit_log.record(AuditEvent::new(
                            agent_id,
                            AuditEventKind::HumanApprovalGranted,
                        ));
                    }
                    ApprovalResult::Denied { reason } => {
                        self.audit_log.record(AuditEvent::new(
                            agent_id,
                            AuditEventKind::HumanApprovalDenied {
                                reason: reason.clone(),
                            },
                        ));
                        return Err(CoreError::CapabilityDenied {
                            agent_id,
                            capability: Capability::ToolInvoke {
                                tool_name: tool.to_string(),
                            },
                            reason: format!("human denied: {reason}"),
                        });
                    }
                    ApprovalResult::Timeout => {
                        return Err(CoreError::CapabilityDenied {
                            agent_id,
                            capability: Capability::ToolInvoke {
                                tool_name: tool.to_string(),
                            },
                            reason: "approval timed out".into(),
                        });
                    }
                }
            }
        }

        let tokens = self.registry.get_tokens(agent_id)?;
        self.tool_invocation
            .invoke(agent_id, tool, input, &tokens)
            .await
    }

    async fn send_message(&self, agent_id: AgentId, message: Value) -> Result<Value> {
        let recipient_str = message
            .get("recipient")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Ipc("missing 'recipient' in message".into()))?;
        let recipient: AgentId = serde_json::from_value(serde_json::json!(recipient_str))
            .map_err(|e| CoreError::Ipc(format!("invalid recipient: {e}")))?;
        let method = message
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("notify")
            .to_string();
        let params = message
            .get("params")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        let mcp_msg = McpMessage::new(agent_id, recipient, method, params);
        self.router.route(mcp_msg).await?;
        Ok(serde_json::json!({"status": "delivered"}))
    }

    async fn request_approval(
        &self,
        agent_id: AgentId,
        description: String,
        _timeout: Duration,
    ) -> Result<ApprovalResult> {
        let name = self
            .registry
            .get_manifest(agent_id)
            .map(|m| m.name)
            .unwrap_or_else(|_| "unknown".into());
        self.approval_service
            .request(agent_id, name, description, None, None)
            .await
    }

    async fn report_usage(&self, agent_id: AgentId, usage: TokenUsage) -> Result<()> {
        self.audit_log.record(AuditEvent::new(
            agent_id,
            AuditEventKind::UsageReported {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            },
        ));
        Ok(())
    }

    async fn list_tools(&self, agent_id: AgentId) -> Result<Vec<ToolDefinition>> {
        let tokens = self.registry.get_tokens(agent_id)?;
        let all_tools = self.tool_registry.list();

        let filtered = all_tools
            .into_iter()
            .filter(|tool_def| {
                let required = Capability::ToolInvoke {
                    tool_name: tool_def.name.clone(),
                };
                tokens.iter().any(|t| t.permits(&required))
            })
            .collect();

        Ok(filtered)
    }

    async fn send_and_wait(
        &self,
        agent_id: AgentId,
        recipient: AgentId,
        method: String,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let tokens = self.registry.get_tokens(agent_id)?;
        let required = Capability::MessageSend {
            target_agents: vec![recipient.to_string()],
        };
        if !tokens.iter().any(|t| t.permits(&required)) {
            return Err(CoreError::CapabilityDenied {
                agent_id,
                capability: required,
                reason: "send_and_wait not permitted".into(),
            });
        }

        let msg = McpMessage::new(agent_id, recipient, method, params);
        let trace_id = msg.metadata.trace_id;

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.router.register_pending(trace_id, tx);

        if let Err(e) = self.router.route(msg).await {
            return Err(e);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => {
                if let Some(result) = response.result {
                    Ok(result)
                } else if let Some(error) = response.error {
                    Err(CoreError::Ipc(error.message))
                } else {
                    Ok(serde_json::json!({}))
                }
            }
            Ok(Err(_)) => {
                Err(CoreError::Ipc("responder dropped".into()))
            }
            Err(_) => {
                Err(CoreError::Timeout(timeout))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentManifest, InMemoryAuditLog, NoOpApprovalService};
    use aaos_tools::EchoTool;

    fn setup() -> (InProcessAgentServices, AgentId, Arc<InMemoryAuditLog>) {
        let audit_log = Arc::new(InMemoryAuditLog::new());
        let router = Arc::new(aaos_ipc::MessageRouter::new(audit_log.clone(), |_, _| true));
        let registry = Arc::new(AgentRegistry::new(audit_log.clone()));
        registry.set_router(router.clone());
        let tool_registry = Arc::new(ToolRegistry::new());
        tool_registry.register(Arc::new(EchoTool));

        let tool_invocation = Arc::new(ToolInvocation::new(
            tool_registry.clone(),
            audit_log.clone(),
        ));

        let manifest = AgentManifest::from_yaml(
            r#"
name: test-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
capabilities:
  - "tool: echo"
  - web_search
"#,
        )
        .unwrap();
        let agent_id = registry.spawn(manifest).unwrap();

        let approval: Arc<dyn ApprovalService> = Arc::new(NoOpApprovalService);
        let services = InProcessAgentServices::new(
            registry,
            tool_invocation,
            tool_registry,
            audit_log.clone(),
            router,
            approval,
        );

        (services, agent_id, audit_log)
    }

    #[tokio::test]
    async fn invoke_tool_with_capability() {
        let (services, agent_id, _log) = setup();
        let result = services
            .invoke_tool(agent_id, "echo", serde_json::json!({"message": "hello"}))
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({"message": "hello"}));
    }

    #[tokio::test]
    async fn invoke_tool_without_capability() {
        let (services, agent_id, _log) = setup();
        let result = services
            .invoke_tool(agent_id, "nonexistent", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn invoke_tool_nonexistent_agent() {
        let (services, _agent_id, _log) = setup();
        let result = services
            .invoke_tool(AgentId::new(), "echo", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_tools_filters_by_capability() {
        let (services, agent_id, _log) = setup();
        let tools = services.list_tools(agent_id).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
    }

    #[tokio::test]
    async fn report_usage_creates_audit_event() {
        let (services, agent_id, log) = setup();
        let initial_count = log.len();
        services
            .report_usage(
                agent_id,
                TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                },
            )
            .await
            .unwrap();
        assert_eq!(log.len(), initial_count + 1);
    }

    #[tokio::test]
    async fn request_approval_auto_approves() {
        let (services, agent_id, _log) = setup();
        let result = services
            .request_approval(agent_id, "test action".into(), Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(result, ApprovalResult::Approved);
    }

    #[tokio::test]
    async fn invoke_tool_approval_auto_approves() {
        // Agent with approval_required but NoOpApprovalService -> auto-approves
        let audit_log = Arc::new(InMemoryAuditLog::new());
        let router = Arc::new(aaos_ipc::MessageRouter::new(audit_log.clone(), |_, _| true));
        let registry = Arc::new(AgentRegistry::new(audit_log.clone()));
        registry.set_router(router.clone());
        let tool_registry = Arc::new(ToolRegistry::new());
        tool_registry.register(Arc::new(EchoTool));
        let tool_invocation = Arc::new(ToolInvocation::new(
            tool_registry.clone(),
            audit_log.clone(),
        ));

        let manifest = AgentManifest::from_yaml(
            r#"
name: test-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
capabilities:
  - "tool: echo"
approval_required:
  - echo
"#,
        )
        .unwrap();
        let agent_id = registry.spawn(manifest).unwrap();

        let approval: Arc<dyn ApprovalService> = Arc::new(NoOpApprovalService);
        let services = InProcessAgentServices::new(
            registry,
            tool_invocation,
            tool_registry,
            audit_log.clone(),
            router,
            approval,
        );

        // Should auto-approve and succeed
        let result = services
            .invoke_tool(agent_id, "echo", serde_json::json!({"message": "hello"}))
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({"message": "hello"}));
    }
}
