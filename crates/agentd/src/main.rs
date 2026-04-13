use std::path::PathBuf;
use std::sync::Arc;

use aaos_llm::{AnthropicClient, AnthropicConfig};
use clap::Parser;
use tracing_subscriber::EnvFilter;

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

/// Bootstrap mode: load a manifest, spawn the bootstrap agent, send it the goal,
/// and wait for its response. Uses StdoutAuditLog for container observability.
async fn run_bootstrap(manifest_path: PathBuf, goal: String) -> anyhow::Result<()> {
    tracing::info!("aaOS bootstrap mode");
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

    // Spawn the bootstrap agent
    let agent_id = server.registry.spawn(manifest.clone())?;
    tracing::info!(agent_id = %agent_id, "bootstrap agent spawned");

    // Build services and executor for the agent
    let services: Arc<dyn aaos_core::AgentServices> =
        Arc::new(aaos_runtime::InProcessAgentServices::new(
            server.registry.clone(),
            server.tool_invocation.clone(),
            server.tool_registry.clone(),
            server.audit_log.clone(),
            server.router.clone(),
            server.approval_queue.clone() as Arc<dyn aaos_core::ApprovalService>,
        ));

    let executor = aaos_llm::AgentExecutor::new(
        llm_client.clone(),
        services,
        aaos_llm::ExecutorConfig::default(),
    );

    // Run the agent with the goal
    tracing::info!("sending goal to bootstrap agent");
    let result = executor.run(agent_id, &manifest, &goal).await;

    // Log the result
    audit_log.record(aaos_core::AuditEvent::new(
        agent_id,
        aaos_core::AuditEventKind::AgentExecutionCompleted {
            stop_reason: result.stop_reason.to_string(),
            total_iterations: result.iterations,
        },
    ));

    tracing::info!(
        iterations = result.iterations,
        stop_reason = %result.stop_reason,
        input_tokens = result.usage.input_tokens,
        output_tokens = result.usage.output_tokens,
        "bootstrap agent completed"
    );

    println!("\n=== Bootstrap Agent Response ===\n");
    println!("{}", result.response);

    Ok(())
}
