use std::path::PathBuf;
use std::sync::Arc;

use aaos_llm::{
    AnthropicClient, AnthropicConfig, InferenceSchedulingConfig, OpenAiCompatConfig,
    OpenAiCompatibleClient, ScheduledLlmClient,
};
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
            let server = if let Ok(config) = OpenAiCompatConfig::deepseek_from_env() {
                tracing::info!(base_url = %config.base_url, "DeepSeek LLM client configured");
                let raw: Arc<dyn aaos_llm::LlmClient> = Arc::new(OpenAiCompatibleClient::new(config));
                let sched_config = InferenceSchedulingConfig::from_env();
                let client: Arc<dyn aaos_llm::LlmClient> =
                    Arc::new(ScheduledLlmClient::new(raw, sched_config));
                Arc::new(Server::with_llm_client(client))
            } else if let Ok(config) = AnthropicConfig::from_env() {
                tracing::info!(base_url = %config.base_url, "Anthropic LLM client configured");
                let raw: Arc<dyn aaos_llm::LlmClient> = Arc::new(AnthropicClient::new(config));
                let sched_config = InferenceSchedulingConfig::from_env();
                let client: Arc<dyn aaos_llm::LlmClient> =
                    Arc::new(ScheduledLlmClient::new(raw, sched_config));
                Arc::new(Server::with_llm_client(client))
            } else {
                tracing::warn!("No LLM client configured. agent.run will be unavailable.");
                Arc::new(Server::new())
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

/// Wipe persistent Bootstrap memory and the stable-ID file. Used when
/// AAOS_RESET_MEMORY=1 is set — the next boot starts with a fresh Bootstrap identity.
fn reset_persistent_memory() {
    let mem_db = std::env::var("AAOS_MEMORY_DB")
        .unwrap_or_else(|_| "/var/lib/aaos/memory/memories.db".to_string());
    let id_path = "/var/lib/aaos/bootstrap_id";
    for p in [&mem_db, &format!("{mem_db}-wal"), &format!("{mem_db}-shm"), &id_path.to_string()] {
        match std::fs::remove_file(p) {
            Ok(()) => tracing::warn!(path = %p, "AAOS_RESET_MEMORY: deleted"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %p, error = %e, "failed to delete"),
        }
    }
}

/// Resolve the Bootstrap Agent's stable ID.
/// Priority: `AAOS_BOOTSTRAP_ID` env var > `/var/lib/aaos/bootstrap_id` file > new UUID (persisted).
/// A stable ID lets the Bootstrap Agent's episodic memory persist across container restarts.
fn load_or_create_bootstrap_id() -> aaos_core::AgentId {
    if let Ok(s) = std::env::var("AAOS_BOOTSTRAP_ID") {
        if let Ok(uuid) = uuid::Uuid::parse_str(s.trim()) {
            return aaos_core::AgentId::from_uuid(uuid);
        }
        tracing::warn!(value = %s, "AAOS_BOOTSTRAP_ID set but not a valid UUID; falling back to file");
    }

    let path = std::path::PathBuf::from("/var/lib/aaos/bootstrap_id");
    if let Ok(contents) = std::fs::read_to_string(&path) {
        if let Ok(uuid) = uuid::Uuid::parse_str(contents.trim()) {
            tracing::info!(path = %path.display(), "bootstrap id loaded from disk");
            return aaos_core::AgentId::from_uuid(uuid);
        }
        tracing::warn!(path = %path.display(), "bootstrap id file corrupt; regenerating");
    }

    let uuid = uuid::Uuid::new_v4();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, uuid.to_string()) {
        tracing::warn!(error = %e, path = %path.display(), "failed to persist bootstrap id (memory will not survive restart)");
    } else {
        tracing::info!(path = %path.display(), uuid = %uuid, "bootstrap id created and persisted");
    }
    aaos_core::AgentId::from_uuid(uuid)
}

/// Bootstrap mode: spawn the Bootstrap Agent as a persistent agent, send the initial
/// goal, then start the Unix socket listener so additional goals can arrive via
/// `agent.run`. The container stays alive until explicitly stopped.
async fn run_bootstrap(manifest_path: PathBuf, goal: String) -> anyhow::Result<()> {
    tracing::info!("aaOS bootstrap mode (persistent)");
    tracing::info!(manifest = %manifest_path.display(), "loading bootstrap manifest");
    tracing::info!(goal = %goal, "bootstrap goal");

    // Explicit memory reset: wipe persistent Bootstrap memory + stable ID file before starting.
    // Used when prior runs poisoned memory with bad strategies or sensitive content.
    if std::env::var("AAOS_RESET_MEMORY").ok().as_deref() == Some("1") {
        reset_persistent_memory();
    }

    // Prefer DeepSeek if DEEPSEEK_API_KEY is set, fall back to Anthropic.
    let raw_client: Arc<dyn aaos_llm::LlmClient> =
        if let Ok(config) = OpenAiCompatConfig::deepseek_from_env() {
            tracing::info!(base_url = %config.base_url, "using DeepSeek LLM client");
            Arc::new(OpenAiCompatibleClient::new(config))
        } else if let Ok(config) = AnthropicConfig::from_env() {
            tracing::info!(base_url = %config.base_url, "using Anthropic LLM client");
            Arc::new(AnthropicClient::new(config))
        } else {
            return Err(anyhow::anyhow!(
                "bootstrap mode requires DEEPSEEK_API_KEY or ANTHROPIC_API_KEY"
            ));
        };

    // Wrap in inference scheduler for concurrency control
    let sched_config = InferenceSchedulingConfig::from_env();
    tracing::info!(
        max_concurrent = sched_config.max_concurrent,
        min_delay_ms = sched_config.min_delay_ms,
        "inference scheduling configured"
    );
    let llm_client: Arc<dyn aaos_llm::LlmClient> =
        Arc::new(ScheduledLlmClient::new(raw_client, sched_config));

    // Use StdoutAuditLog for container observability (logs to stdout as JSON)
    let audit_log: Arc<dyn aaos_core::AuditLog> = Arc::new(aaos_core::StdoutAuditLog);
    let server = Arc::new(Server::with_llm_and_audit(llm_client.clone(), audit_log.clone()));

    // Load the bootstrap manifest
    let manifest = aaos_core::AgentManifest::from_file(&manifest_path)?;
    tracing::info!(name = %manifest.name, model = %manifest.model, "bootstrap agent loaded");

    // Read manifest as YAML string for the spawn RPC
    let manifest_yaml = std::fs::read_to_string(&manifest_path)?;

    // Step 1: Determine the Bootstrap Agent's ID.
    // Priority: AAOS_BOOTSTRAP_ID env var > /var/lib/aaos/bootstrap_id file > fresh UUID.
    // A stable ID lets episodic memory accumulate across container restarts.
    // Intentionally scoped to Bootstrap only — regular agent IDs remain unforgeable per-spawn.
    let bootstrap_id = load_or_create_bootstrap_id();
    tracing::info!(bootstrap_id = %bootstrap_id, "bootstrap agent id resolved");

    // Spawn the bootstrap agent as persistent with the pinned ID.
    let spawn_resp = server
        .spawn_with_pinned_id(&manifest_yaml, bootstrap_id)
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
