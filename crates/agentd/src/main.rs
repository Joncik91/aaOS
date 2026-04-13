use std::path::PathBuf;
use std::sync::Arc;

use aaos_llm::{AnthropicClient, AnthropicConfig};
use clap::Parser;
use serde_json::json;
use tracing_subscriber::EnvFilter;

use agentd::api::JsonRpcRequest;
use agentd::config::{Cli, Command};
use agentd::server::Server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("agentd=info".parse()?))
        .init();

    // Bootstrap mode: when AAOS_BOOTSTRAP_MANIFEST and AAOS_BOOTSTRAP_GOAL are set,
    // skip CLI parsing and run a single bootstrap agent to completion.
    if let (Ok(manifest_path), Ok(goal)) = (
        std::env::var("AAOS_BOOTSTRAP_MANIFEST"),
        std::env::var("AAOS_BOOTSTRAP_GOAL"),
    ) {
        return run_bootstrap(PathBuf::from(manifest_path), goal).await;
    }

    let cli = Cli::parse();

    match cli.command {
        Command::Run { socket, .. } => {
            tracing::info!("starting agentd");

            // Try to configure LLM client from environment
            let server = match AnthropicConfig::from_env() {
                Ok(config) => {
                    tracing::info!(base_url = %config.base_url, "Anthropic LLM client configured");
                    let client = Arc::new(AnthropicClient::new(config));
                    Arc::new(Server::with_llm_client(client))
                }
                Err(e) => {
                    tracing::warn!("No LLM client configured: {e}. agent.run will be unavailable.");
                    Arc::new(Server::new())
                }
            };

            server.listen(&socket).await?;
        }
        Command::Spawn { manifest, socket } => {
            let manifest = aaos_core::AgentManifest::from_file(&manifest)?;
            tracing::info!(name = %manifest.name, "spawning agent (client mode not yet implemented)");
            tracing::info!(socket = %socket.display(), "would connect to daemon");
            // TODO: implement client connection to daemon socket
            println!("Agent manifest loaded: {}", manifest.name);
            println!("Client mode not yet implemented — use 'agentd run' to start the daemon.");
        }
        Command::List { socket } => {
            tracing::info!(socket = %socket.display(), "listing agents (client mode not yet implemented)");
            println!("Client mode not yet implemented — use 'agentd run' to start the daemon.");
        }
        Command::Status { agent_id, socket } => {
            tracing::info!(agent_id = %agent_id, socket = %socket.display(), "status query (client mode not yet implemented)");
            println!("Client mode not yet implemented — use 'agentd run' to start the daemon.");
        }
        Command::Stop { agent_id, socket } => {
            tracing::info!(agent_id = %agent_id, socket = %socket.display(), "stop request (client mode not yet implemented)");
            println!("Client mode not yet implemented — use 'agentd run' to start the daemon.");
        }
    }

    Ok(())
}

/// Build a JSON-RPC request helper.
fn rpc(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: method.to_string(),
        params,
    }
}

/// Bootstrap mode: spawn the Bootstrap Agent as a persistent agent, send the initial
/// goal, then start the Unix socket listener so additional goals can arrive via
/// `agent.run`. The container stays alive until explicitly stopped.
async fn run_bootstrap(manifest_path: PathBuf, goal: String) -> anyhow::Result<()> {
    tracing::info!("aaOS bootstrap mode (persistent)");
    tracing::info!(manifest = %manifest_path.display(), "loading bootstrap manifest");
    tracing::info!(goal = %goal, "bootstrap goal");

    // Require an LLM client in bootstrap mode
    let config = AnthropicConfig::from_env()
        .map_err(|e| anyhow::anyhow!("bootstrap mode requires ANTHROPIC_API_KEY: {e}"))?;
    let llm_client: Arc<dyn aaos_llm::LlmClient> = Arc::new(AnthropicClient::new(config));

    // Use StdoutAuditLog for container observability (logs to stdout as JSON)
    let audit_log: Arc<dyn aaos_core::AuditLog> = Arc::new(aaos_core::StdoutAuditLog);
    let server = Arc::new(Server::with_llm_and_audit(llm_client.clone(), audit_log.clone()));

    // Load the bootstrap manifest
    let manifest = aaos_core::AgentManifest::from_file(&manifest_path)?;
    tracing::info!(name = %manifest.name, model = %manifest.model, "bootstrap agent loaded");

    // Read manifest as YAML string for the spawn RPC
    let manifest_yaml = std::fs::read_to_string(&manifest_path)?;

    // Step 1: Spawn the bootstrap agent as persistent via the server's agent.spawn handler.
    // This sets up the persistent loop with context manager, session store, etc.
    let spawn_resp = server
        .handle_request(&rpc("agent.spawn", json!({ "manifest": manifest_yaml })))
        .await;

    let agent_id_str = spawn_resp
        .result
        .as_ref()
        .and_then(|v| v.get("agent_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            let err_msg = spawn_resp
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "unknown spawn error".into());
            anyhow::anyhow!("failed to spawn bootstrap agent: {err_msg}")
        })?
        .to_string();

    tracing::info!(agent_id = %agent_id_str, "bootstrap agent spawned as persistent");

    // Step 2: Send the initial goal via agent.run (routed through the persistent loop).
    // Create a workspace directory for the initial goal.
    let workspace_id = uuid::Uuid::new_v4();
    let workspace_path = format!("/data/workspace/{workspace_id}");
    let _ = std::fs::create_dir_all(&workspace_path);
    let augmented_goal = format!(
        "[Workspace for this task: {workspace_path}. Tell all child agents to use this directory for intermediate files.]\n\n{goal}"
    );

    tracing::info!("sending initial goal to bootstrap agent");
    let run_resp = server
        .handle_request(&rpc(
            "agent.run",
            json!({
                "agent_id": agent_id_str,
                "message": augmented_goal,
            }),
        ))
        .await;

    if let Some(err) = &run_resp.error {
        tracing::error!(error = %err.message, "failed to send initial goal");
        return Err(anyhow::anyhow!("failed to send initial goal: {}", err.message));
    }
    tracing::info!("initial goal delivered to persistent bootstrap agent");

    // Step 3: Start the Unix socket listener so additional goals can be sent via agent.run.
    let socket_path = PathBuf::from("/var/run/agentd.sock");
    tracing::info!(socket = %socket_path.display(), "starting Unix socket listener for additional goals");
    server.listen(&socket_path).await?;

    Ok(())
}
