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
