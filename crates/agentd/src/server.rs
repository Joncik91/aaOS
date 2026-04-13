use std::path::Path;
use std::sync::Arc;

use aaos_core::{AgentManifest, AgentServices, ApprovalService, AuditLog, InMemoryAuditLog};
use aaos_ipc::{MessageRouter, SchemaValidator};
use aaos_llm::{AgentExecutor, ExecutorConfig, LlmClient};
use aaos_runtime::{AgentRegistry, AgentState, InProcessAgentServices};
use aaos_tools::{EchoTool, ToolInvocation, ToolRegistry};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::api::{JsonRpcResponse, INTERNAL_ERROR, METHOD_NOT_FOUND};

/// The core daemon server holding all subsystems.
#[allow(dead_code)]
pub struct Server {
    pub registry: Arc<AgentRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub tool_invocation: Arc<ToolInvocation>,
    pub router: Arc<MessageRouter>,
    pub validator: Arc<SchemaValidator>,
    pub audit_log: Arc<dyn AuditLog>,
    pub approval_queue: Arc<crate::approval::ApprovalQueue>,
    pub llm_client: Option<Arc<dyn LlmClient>>,
    pub session_store: Arc<dyn aaos_runtime::SessionStore>,
    pub memory_store: Arc<dyn aaos_memory::MemoryStore>,
    pub embedding_source: Arc<dyn aaos_memory::EmbeddingSource>,
}

impl Server {
    /// Create a new server with default configuration.
    pub fn new() -> Self {
        let audit_log: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let approval_queue = Arc::new(crate::approval::ApprovalQueue::new());
        let registry = Arc::new(AgentRegistry::new(audit_log.clone()));
        let tool_registry = Arc::new(ToolRegistry::new());
        let validator = Arc::new(SchemaValidator::new());

        // Register built-in tools
        tool_registry.register(Arc::new(EchoTool));
        tool_registry.register(Arc::new(aaos_tools::WebFetchTool::new()));
        tool_registry.register(Arc::new(aaos_tools::FileReadTool));
        tool_registry.register(Arc::new(aaos_tools::FileWriteTool));

        // Memory subsystem (default: in-memory store + mock embeddings)
        let embedding_source: Arc<dyn aaos_memory::EmbeddingSource> =
            Arc::new(aaos_memory::MockEmbeddingSource::new(768));
        let memory_store: Arc<dyn aaos_memory::MemoryStore> = Arc::new(
            aaos_memory::InMemoryMemoryStore::new(10_000, 768, embedding_source.model_name()),
        );

        // Register memory tools
        tool_registry.register(Arc::new(aaos_tools::MemoryStoreTool::new(
            memory_store.clone(),
            embedding_source.clone(),
            audit_log.clone(),
            4096,
        )));
        tool_registry.register(Arc::new(aaos_tools::MemoryQueryTool::new(
            memory_store.clone(),
            embedding_source.clone(),
            audit_log.clone(),
        )));
        tool_registry.register(Arc::new(aaos_tools::MemoryDeleteTool::new(
            memory_store.clone(),
            audit_log.clone(),
        )));

        let tool_invocation = Arc::new(ToolInvocation::new(
            tool_registry.clone(),
            audit_log.clone(),
        ));

        // Create message router with capability checking via the registry
        let registry_clone = registry.clone();
        let router = Arc::new(MessageRouter::new(
            audit_log.clone(),
            move |agent_id, cap| {
                registry_clone
                    .check_capability(agent_id, cap)
                    .unwrap_or(false)
            },
        ));

        // Set router on registry for spawn/stop registration
        registry.set_router(router.clone());

        let session_store: Arc<dyn aaos_runtime::SessionStore> =
            Arc::new(aaos_runtime::InMemorySessionStore::new());

        Self {
            registry,
            tool_registry,
            tool_invocation,
            router,
            validator,
            audit_log,
            approval_queue,
            llm_client: None,
            session_store,
            memory_store,
            embedding_source,
        }
    }

    /// Create a server with a specific LLM client (for testing).
    #[allow(dead_code)]
    pub fn with_llm_client(llm_client: Arc<dyn LlmClient>) -> Self {
        let mut server = Self::new();
        // Register SpawnAgentTool with the LLM client
        let spawn_tool = Arc::new(crate::spawn_tool::SpawnAgentTool::new(
            llm_client.clone(),
            server.registry.clone(),
            server.tool_registry.clone(),
            server.tool_invocation.clone(),
            server.audit_log.clone(),
            server.router.clone(),
            server.approval_queue.clone() as Arc<dyn ApprovalService>,
        ));
        server.tool_registry.register(spawn_tool);
        server.llm_client = Some(llm_client);
        server
    }

    /// Create a server with a specific LLM client and custom memory/embedding sources.
    #[allow(dead_code)]
    pub fn with_memory(
        llm_client: Arc<dyn LlmClient>,
        memory_store: Arc<dyn aaos_memory::MemoryStore>,
        embedding_source: Arc<dyn aaos_memory::EmbeddingSource>,
    ) -> Self {
        let audit_log: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let approval_queue = Arc::new(crate::approval::ApprovalQueue::new());
        let registry = Arc::new(AgentRegistry::new(audit_log.clone()));
        let tool_registry = Arc::new(ToolRegistry::new());
        let validator = Arc::new(SchemaValidator::new());

        // Register built-in tools
        tool_registry.register(Arc::new(EchoTool));
        tool_registry.register(Arc::new(aaos_tools::WebFetchTool::new()));
        tool_registry.register(Arc::new(aaos_tools::FileReadTool));
        tool_registry.register(Arc::new(aaos_tools::FileWriteTool));

        // Register memory tools with the provided sources
        tool_registry.register(Arc::new(aaos_tools::MemoryStoreTool::new(
            memory_store.clone(),
            embedding_source.clone(),
            audit_log.clone(),
            4096,
        )));
        tool_registry.register(Arc::new(aaos_tools::MemoryQueryTool::new(
            memory_store.clone(),
            embedding_source.clone(),
            audit_log.clone(),
        )));
        tool_registry.register(Arc::new(aaos_tools::MemoryDeleteTool::new(
            memory_store.clone(),
            audit_log.clone(),
        )));

        let tool_invocation = Arc::new(ToolInvocation::new(
            tool_registry.clone(),
            audit_log.clone(),
        ));

        let registry_clone = registry.clone();
        let router = Arc::new(MessageRouter::new(
            audit_log.clone(),
            move |agent_id, cap| {
                registry_clone
                    .check_capability(agent_id, cap)
                    .unwrap_or(false)
            },
        ));
        registry.set_router(router.clone());

        let session_store: Arc<dyn aaos_runtime::SessionStore> =
            Arc::new(aaos_runtime::InMemorySessionStore::new());

        // Register SpawnAgentTool with the LLM client
        let spawn_tool = Arc::new(crate::spawn_tool::SpawnAgentTool::new(
            llm_client.clone(),
            registry.clone(),
            tool_registry.clone(),
            tool_invocation.clone(),
            audit_log.clone(),
            router.clone(),
            approval_queue.clone() as Arc<dyn ApprovalService>,
        ));
        tool_registry.register(spawn_tool);

        Self {
            registry,
            tool_registry,
            tool_invocation,
            router,
            validator,
            audit_log,
            approval_queue,
            llm_client: Some(llm_client),
            session_store,
            memory_store,
            embedding_source,
        }
    }

    /// Handle a JSON-RPC request and return a response.
    pub async fn handle_request(&self, request: &crate::api::JsonRpcRequest) -> JsonRpcResponse {
        match request.method.as_str() {
            "agent.spawn" => {
                self.handle_agent_spawn(&request.params, request.id.clone())
                    .await
            }
            "agent.stop" => self.handle_agent_stop(&request.params, request.id.clone()).await,
            "agent.list" => self.handle_agent_list(request.id.clone()),
            "agent.status" => self.handle_agent_status(&request.params, request.id.clone()),
            "tool.list" => self.handle_tool_list(request.id.clone()),
            "tool.invoke" => {
                self.handle_tool_invoke(&request.params, request.id.clone())
                    .await
            }
            "agent.run" => {
                self.handle_agent_run(&request.params, request.id.clone())
                    .await
            }
            "agent.spawn_and_run" => {
                self.handle_agent_spawn_and_run(&request.params, request.id.clone())
                    .await
            }
            "approval.list" => self.handle_approval_list(request.id.clone()),
            "approval.respond" => self.handle_approval_respond(&request.params, request.id.clone()),
            _ => JsonRpcResponse::error(request.id.clone(), METHOD_NOT_FOUND, "method not found"),
        }
    }

    async fn handle_agent_spawn(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let manifest_yaml = match params.get("manifest").and_then(|m| m.as_str()) {
            Some(yaml) => yaml,
            None => match params.get("manifest_path").and_then(|p| p.as_str()) {
                Some(path) => match std::fs::read_to_string(path) {
                    Ok(content) => return self.spawn_from_yaml(&content, id).await,
                    Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                },
                None => {
                    return JsonRpcResponse::error(
                        id,
                        INTERNAL_ERROR,
                        "missing 'manifest' or 'manifest_path' parameter",
                    )
                }
            },
        };
        self.spawn_from_yaml(manifest_yaml, id).await
    }

    async fn spawn_from_yaml(&self, yaml: &str, id: serde_json::Value) -> JsonRpcResponse {
        let manifest = match AgentManifest::from_yaml(yaml) {
            Ok(m) => m,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };

        let is_persistent = manifest.lifecycle == aaos_core::Lifecycle::Persistent;

        let agent_id = match self.registry.spawn(manifest.clone()) {
            Ok(id) => id,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };

        // For persistent agents: start the background message loop
        if is_persistent {
            if let Some(llm) = &self.llm_client {
                let services: Arc<dyn AgentServices> = Arc::new(InProcessAgentServices::new(
                    self.registry.clone(),
                    self.tool_invocation.clone(),
                    self.tool_registry.clone(),
                    self.audit_log.clone(),
                    self.router.clone(),
                    self.approval_queue.clone() as Arc<dyn ApprovalService>,
                ));
                let executor = AgentExecutor::new(
                    llm.clone(),
                    services,
                    ExecutorConfig::default(),
                );

                // Construct ContextManager for summarization support
                let model_for_summarization = manifest.memory.summarization_model.clone()
                    .unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string());
                let threshold = manifest.memory.summarization_threshold.unwrap_or(0.7);
                let model_max = llm.max_context_tokens(&manifest.model);
                let budget = match aaos_core::TokenBudget::from_config(
                    &manifest.memory.context_window, model_max,
                ) {
                    Ok(b) => b,
                    Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR,
                        format!("invalid context_window: {e}")),
                };
                let context_manager = Some(Arc::new(aaos_runtime::ContextManager::new(
                    llm.clone(), budget, model_for_summarization, threshold,
                )));

                if let Err(e) = self.registry.start_persistent_loop(
                    agent_id,
                    executor,
                    self.session_store.clone(),
                    self.router.clone(),
                    context_manager,
                ) {
                    return JsonRpcResponse::error(id, INTERNAL_ERROR,
                        format!("failed to start persistent loop: {e}"));
                }
            } else {
                return JsonRpcResponse::error(id, INTERNAL_ERROR,
                    "persistent agents require an LLM client");
            }
        }

        JsonRpcResponse::success(id, json!({"agent_id": agent_id}))
    }

    async fn handle_agent_stop(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let agent_id_str = match params.get("agent_id").and_then(|a| a.as_str()) {
            Some(s) => s,
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'agent_id' parameter")
            }
        };
        let agent_id: aaos_core::AgentId = match serde_json::from_value(json!(agent_id_str)) {
            Ok(id) => id,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };
        match self.registry.stop(agent_id).await {
            Ok(()) => JsonRpcResponse::success(id, json!({"ok": true})),
            Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        }
    }

    fn handle_agent_list(&self, id: serde_json::Value) -> JsonRpcResponse {
        let agents: Vec<_> = self
            .registry
            .list()
            .into_iter()
            .map(|info| {
                json!({
                    "id": info.id,
                    "name": info.name,
                    "model": info.model,
                    "state": format!("{}", info.state),
                    "capability_count": info.capability_count,
                })
            })
            .collect();
        JsonRpcResponse::success(id, json!({"agents": agents}))
    }

    fn handle_agent_status(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let agent_id_str = match params.get("agent_id").and_then(|a| a.as_str()) {
            Some(s) => s,
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'agent_id' parameter")
            }
        };
        let agent_id: aaos_core::AgentId = match serde_json::from_value(json!(agent_id_str)) {
            Ok(id) => id,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };
        match self.registry.get_info(agent_id) {
            Ok(info) => JsonRpcResponse::success(
                id,
                json!({
                    "id": info.id,
                    "name": info.name,
                    "model": info.model,
                    "state": format!("{}", info.state),
                    "capability_count": info.capability_count,
                }),
            ),
            Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        }
    }

    fn handle_tool_list(&self, id: serde_json::Value) -> JsonRpcResponse {
        let tools: Vec<_> = self.tool_registry.list();
        JsonRpcResponse::success(id, json!({"tools": tools}))
    }

    async fn handle_tool_invoke(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let agent_id_str = match params.get("agent_id").and_then(|a| a.as_str()) {
            Some(s) => s,
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'agent_id' parameter")
            }
        };
        let agent_id: aaos_core::AgentId = match serde_json::from_value(json!(agent_id_str)) {
            Ok(id) => id,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };

        // Validate agent exists and is running
        match self.registry.get_info(agent_id) {
            Ok(info) => {
                if info.state != AgentState::Running {
                    return JsonRpcResponse::error(
                        id,
                        INTERNAL_ERROR,
                        format!("agent is not running (state: {})", info.state),
                    );
                }
            }
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        }

        let tool_name = match params.get("tool").and_then(|t| t.as_str()) {
            Some(s) => s,
            None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'tool' parameter"),
        };
        let input = params.get("input").cloned().unwrap_or(json!({}));

        // Get tokens and invoke
        match self.registry.get_tokens(agent_id) {
            Ok(tokens) => {
                match self
                    .tool_invocation
                    .invoke(agent_id, tool_name, input, &tokens)
                    .await
                {
                    Ok(result) => JsonRpcResponse::success(id, json!({"result": result})),
                    Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                }
            }
            Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        }
    }

    async fn handle_agent_run(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let agent_id_str = match params.get("agent_id").and_then(|a| a.as_str()) {
            Some(s) => s,
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'agent_id' parameter")
            }
        };
        let agent_id: aaos_core::AgentId = match serde_json::from_value(json!(agent_id_str)) {
            Ok(id) => id,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };
        let message = match params.get("message").and_then(|m| m.as_str()) {
            Some(s) => s,
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'message' parameter")
            }
        };

        // Validate agent exists and is running, get manifest
        let manifest = match self.registry.get_info(agent_id) {
            Ok(info) => {
                if info.state != aaos_runtime::AgentState::Running {
                    return JsonRpcResponse::error(
                        id,
                        INTERNAL_ERROR,
                        format!("agent is not running (state: {})", info.state),
                    );
                }
                match self.registry.get_manifest(agent_id) {
                    Ok(m) => m,
                    Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                }
            }
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        };

        if manifest.lifecycle == aaos_core::Lifecycle::Persistent {
            let msg = aaos_ipc::McpMessage::new(
                agent_id, agent_id, "agent.run",
                json!({"message": message}),
            );
            let trace_id = msg.metadata.trace_id;

            match self.router.route(msg).await {
                Ok(()) => JsonRpcResponse::success(id, json!({
                    "trace_id": trace_id.to_string(),
                    "status": "delivered",
                })),
                Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        } else {
            self.execute_agent(agent_id, &manifest, message, id).await
        }
    }

    async fn handle_agent_spawn_and_run(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let message = match params.get("message").and_then(|m| m.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'message' parameter")
            }
        };

        // Spawn first
        let spawn_resp = self.handle_agent_spawn(params, json!(null)).await;
        let agent_id_str = match spawn_resp.result {
            Some(ref v) => match v.get("agent_id").and_then(|a| a.as_str()) {
                Some(s) => s.to_string(),
                None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "spawn failed"),
            },
            None => {
                return JsonRpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    spawn_resp
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "spawn failed".into()),
                )
            }
        };
        let agent_id: aaos_core::AgentId = serde_json::from_value(json!(agent_id_str)).unwrap();

        let manifest = self.registry.get_manifest(agent_id).unwrap();
        let mut result = self.execute_agent(agent_id, &manifest, &message, id).await;

        // Inject agent_id into the result
        if let Some(ref mut v) = result.result {
            v["agent_id"] = json!(agent_id_str);
        }
        result
    }

    async fn execute_agent(
        &self,
        agent_id: aaos_core::AgentId,
        manifest: &aaos_core::AgentManifest,
        message: &str,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let llm = match &self.llm_client {
            Some(client) => client.clone(),
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "no LLM client configured");
            }
        };

        // Emit execution started audit event
        self.audit_log.record(aaos_core::AuditEvent::new(
            agent_id,
            aaos_core::AuditEventKind::AgentExecutionStarted {
                message_preview: message.chars().take(100).collect(),
            },
        ));

        let services: Arc<dyn AgentServices> = Arc::new(InProcessAgentServices::new(
            self.registry.clone(),
            self.tool_invocation.clone(),
            self.tool_registry.clone(),
            self.audit_log.clone(),
            self.router.clone(),
            self.approval_queue.clone() as Arc<dyn ApprovalService>,
        ));

        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());
        let result = executor.run(agent_id, manifest, message).await;

        // Emit execution completed audit event
        self.audit_log.record(aaos_core::AuditEvent::new(
            agent_id,
            aaos_core::AuditEventKind::AgentExecutionCompleted {
                stop_reason: result.stop_reason.to_string(),
                total_iterations: result.iterations,
            },
        ));

        JsonRpcResponse::success(
            id,
            json!({
                "response": result.response,
                "usage": {
                    "input_tokens": result.usage.input_tokens,
                    "output_tokens": result.usage.output_tokens,
                },
                "iterations": result.iterations,
                "stop_reason": result.stop_reason.to_string(),
            }),
        )
    }

    fn handle_approval_list(&self, id: serde_json::Value) -> JsonRpcResponse {
        let pending = self.approval_queue.list();
        JsonRpcResponse::success(id, json!({"pending": pending}))
    }

    fn handle_approval_respond(
        &self,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> JsonRpcResponse {
        let approval_id = match params.get("id").and_then(|v| v.as_str()) {
            Some(s) => match uuid::Uuid::parse_str(s) {
                Ok(uid) => uid,
                Err(e) => {
                    return JsonRpcResponse::error(id, INTERNAL_ERROR, format!("invalid id: {e}"))
                }
            },
            None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'id' parameter"),
        };

        let decision = match params.get("decision").and_then(|v| v.as_str()) {
            Some("approve") => aaos_core::ApprovalResult::Approved,
            Some("deny") => {
                let reason = params
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("denied by human")
                    .to_string();
                aaos_core::ApprovalResult::Denied { reason }
            }
            Some(other) => {
                return JsonRpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    format!("invalid decision: {other}. Use 'approve' or 'deny'"),
                )
            }
            None => {
                return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'decision' parameter")
            }
        };

        match self.approval_queue.respond(approval_id, decision) {
            Ok(()) => JsonRpcResponse::success(id, json!({"ok": true})),
            Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        }
    }

    /// Start listening on a Unix socket.
    pub async fn listen(self: Arc<Self>, socket_path: &Path) -> anyhow::Result<()> {
        // Remove stale socket
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)?;
        tracing::info!(path = %socket_path.display(), "listening on unix socket");

        loop {
            let (stream, _) = listener.accept().await?;
            let server = self.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();

                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break, // Connection closed
                        Ok(_) => {
                            let response =
                                match serde_json::from_str::<crate::api::JsonRpcRequest>(&line) {
                                    Ok(request) => server.handle_request(&request).await,
                                    Err(e) => JsonRpcResponse::error(
                                        serde_json::Value::Null,
                                        crate::api::PARSE_ERROR,
                                        e.to_string(),
                                    ),
                                };
                            let mut resp_bytes = serde_json::to_vec(&response).unwrap();
                            resp_bytes.push(b'\n');
                            if writer.write_all(&resp_bytes).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::JsonRpcRequest;

    fn make_request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            method: method.to_string(),
            params,
        }
    }

    #[tokio::test]
    async fn spawn_and_list() {
        let server = Server::new();
        let manifest = r#"
name: test-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
"#;
        let resp = server
            .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
            .await;
        assert!(resp.result.is_some());
        let agent_id = resp.result.unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = server
            .handle_request(&make_request("agent.list", json!({})))
            .await;
        let agents = resp.result.unwrap()["agents"].as_array().unwrap().clone();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["id"].as_str().unwrap(), agent_id);
    }

    #[tokio::test]
    async fn tool_list() {
        let server = Server::new();
        let resp = server
            .handle_request(&make_request("tool.list", json!({})))
            .await;
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        assert!(!tools.is_empty());
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"web_fetch"));
    }

    #[tokio::test]
    async fn unknown_method() {
        let server = Server::new();
        let resp = server
            .handle_request(&make_request("nonexistent", json!({})))
            .await;
        assert!(resp.error.is_some());
    }

    #[tokio::test]
    async fn tool_invoke_with_capability() {
        let server = Server::new();
        let manifest = r#"
name: tool-test
model: claude-haiku-4-5-20251001
system_prompt: "test"
capabilities:
  - "tool: echo"
"#;
        let resp = server
            .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
            .await;
        let agent_id = resp.result.unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = server
            .handle_request(&make_request(
                "tool.invoke",
                json!({"agent_id": agent_id, "tool": "echo", "input": {"message": "hello"}}),
            ))
            .await;
        assert!(resp.result.is_some());
        assert_eq!(resp.result.unwrap()["result"], json!({"message": "hello"}));
    }

    #[tokio::test]
    async fn tool_invoke_without_capability() {
        let server = Server::new();
        let manifest = r#"
name: no-tools
model: claude-haiku-4-5-20251001
system_prompt: "test"
capabilities:
  - web_search
"#;
        let resp = server
            .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
            .await;
        let agent_id = resp.result.unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = server
            .handle_request(&make_request(
                "tool.invoke",
                json!({"agent_id": agent_id, "tool": "echo", "input": {"message": "hello"}}),
            ))
            .await;
        assert!(resp.error.is_some());
    }

    use aaos_core::TokenUsage;
    use aaos_llm::{
        CompletionRequest, CompletionResponse, ContentBlock, LlmClient, LlmResult, LlmStopReason,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct MockLlm {
        responses: Mutex<Vec<LlmResult<CompletionResponse>>>,
    }

    impl MockLlm {
        fn text(text: &str) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(vec![Ok(CompletionResponse {
                    content: vec![ContentBlock::Text { text: text.into() }],
                    stop_reason: LlmStopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                })]),
            })
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        fn max_context_tokens(&self, _model: &str) -> u32 {
            200_000
        }

        async fn complete(&self, _req: CompletionRequest) -> LlmResult<CompletionResponse> {
            self.responses.lock().unwrap().remove(0)
        }
    }

    #[tokio::test]
    async fn agent_spawn_and_run() {
        let server = Server::with_llm_client(MockLlm::text("I'm alive!"));
        let manifest = r#"
name: runner
model: claude-haiku-4-5-20251001
system_prompt: "You are helpful."
capabilities:
  - "tool: echo"
"#;
        let resp = server
            .handle_request(&make_request(
                "agent.spawn_and_run",
                json!({"manifest": manifest, "message": "Hello"}),
            ))
            .await;
        let result = resp.result.unwrap();
        assert!(result.get("agent_id").is_some());
        assert_eq!(result["response"], "I'm alive!");
        assert_eq!(result["stop_reason"], "complete");
        assert_eq!(result["iterations"], 1);
    }

    #[tokio::test]
    async fn agent_run_existing() {
        let server = Server::with_llm_client(MockLlm::text("Running!"));
        let manifest = r#"
name: existing
model: claude-haiku-4-5-20251001
system_prompt: "You are helpful."
"#;
        let resp = server
            .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
            .await;
        let agent_id = resp.result.unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = server
            .handle_request(&make_request(
                "agent.run",
                json!({"agent_id": agent_id, "message": "Do something"}),
            ))
            .await;
        let result = resp.result.unwrap();
        assert_eq!(result["response"], "Running!");
    }

    #[tokio::test]
    async fn tool_invoke_nonexistent_agent() {
        let server = Server::new();
        let resp = server
            .handle_request(&make_request(
                "tool.invoke",
                json!({"agent_id": "00000000-0000-0000-0000-000000000000", "tool": "echo", "input": {}}),
            ))
            .await;
        assert!(resp.error.is_some());
    }

    #[tokio::test]
    async fn approval_list_empty() {
        let server = Server::new();
        let resp = server
            .handle_request(&make_request("approval.list", json!({})))
            .await;
        let result = resp.result.unwrap();
        assert_eq!(result["pending"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn persistent_agent_run_returns_trace_id() {
        let server = Server::with_llm_client(MockLlm::text("Persistent response"));
        let manifest = r#"
name: persistent-test
model: claude-haiku-4-5-20251001
system_prompt: "You are persistent."
lifecycle: persistent
"#;
        let resp = server
            .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
            .await;
        let agent_id = resp.result.unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = server
            .handle_request(&make_request(
                "agent.run",
                json!({"agent_id": agent_id, "message": "Hello persistent"}),
            ))
            .await;
        let result = resp.result.unwrap();
        assert!(result.get("trace_id").is_some());
        assert_eq!(result["status"], "delivered");
    }
}
