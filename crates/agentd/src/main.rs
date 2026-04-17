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
                // SAFETY: the key now lives in `config.api_key` (an Arc-owned
                // struct field). Scrub the env entry so children spawned
                // under this process — tokio tasks, InProcessBackend agents,
                // NamespacedBackend workers post-exec — cannot read the key
                // via /proc/<pid>/environ. The `aaos` group reading
                // /etc/default/aaos is a separate concern addressed by the
                // 0600 root:root mode on that file.
                scrub_api_key_env();
                let raw: Arc<dyn aaos_llm::LlmClient> = Arc::new(OpenAiCompatibleClient::new(config));
                let sched_config = InferenceSchedulingConfig::from_env();
                let client: Arc<dyn aaos_llm::LlmClient> =
                    Arc::new(ScheduledLlmClient::new(raw, sched_config));
                Server::with_llm_client(client)
            } else if let Ok(config) = AnthropicConfig::from_env() {
                tracing::info!(base_url = %config.base_url, "Anthropic LLM client configured");
                scrub_api_key_env();
                let raw: Arc<dyn aaos_llm::LlmClient> = Arc::new(AnthropicClient::new(config));
                let sched_config = InferenceSchedulingConfig::from_env();
                let client: Arc<dyn aaos_llm::LlmClient> =
                    Arc::new(ScheduledLlmClient::new(raw, sched_config));
                Server::with_llm_client(client)
            } else {
                tracing::warn!("No LLM client configured. agent.run will be unavailable.");
                Arc::new(Server::new())
            };

            server.listen(&socket).await?;
        }
        Command::Submit { goal, verbose, socket } => {
            return agentd::cli::submit::run(goal, verbose, socket).await;
        }
        Command::List { json, socket } => {
            return agentd::cli::list::run(json, socket).await;
        }
        Command::Status { agent_id, json, socket } => {
            return agentd::cli::status::run(agent_id, json, socket).await;
        }
        Command::Stop { agent_id, socket } => {
            return agentd::cli::stop::run(agent_id, socket).await;
        }
        Command::Logs { agent_id, verbose, socket } => {
            return agentd::cli::logs::run(agent_id, verbose, socket).await;
        }
        Command::Roles { subcommand } => {
            use agentd::cli::RolesCommand;
            return match subcommand {
                RolesCommand::List { dir } => agentd::cli::roles::list(dir).await,
                RolesCommand::Show { name, dir } => agentd::cli::roles::show(name, dir).await,
                RolesCommand::Validate { path } => agentd::cli::roles::validate(path).await,
            };
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

/// Scrub LLM API-key env vars from the process environment after the
/// daemon has copied them into owned `LlmClient` config structs. Two
/// layers of scrub:
///
/// 1. **libc env table** via `std::env::remove_var`. This is what every
///    call to `std::env::var` consults. Child processes inheriting the
///    env (`spawn_agent` under `InProcessBackend` tokio tasks share the
///    parent table; `NamespacedBackend` workers inherit via `execve`'s
///    read of libc's current `environ[]`) see nothing.
/// 2. **Kernel-backed `/proc/<pid>/environ`** via in-place byte zeroing
///    of the env strings themselves. libc's `environ[]` pointers still
///    reference the stack region `execve` wrote the env strings into;
///    `remove_var` only unlinks the pointer, not the backing bytes, so
///    `/proc/self/environ` still leaks the key even after `remove_var`.
///    Overwriting the `KEY=VALUE` bytes in place closes that path.
///
/// The key stays accessible inside `agentd` via the `LlmClient` trait's
/// bearer-token field — it was copied into an owned struct before the
/// scrub fired.
///
/// SAFETY: `std::env::remove_var` is unsafe in Rust 2024 because it can
/// race with concurrent `getenv` in other threads. Called during startup
/// before any tokio tasks or child processes spawn, so no concurrency.
/// The in-place zeroing writes through a pointer libc gave us to a
/// region it (and we) already own; libc's `environ[]` entry is unlinked
/// before the overwrite, so no concurrent `getenv` can observe torn bytes.
fn scrub_api_key_env() {
    for key in ["DEEPSEEK_API_KEY", "ANTHROPIC_API_KEY"] {
        scrub_one_env_var(key);
    }
}

/// Zeros the backing bytes of a `KEY=VALUE` env entry before unsetting
/// it. See `scrub_api_key_env` for why both steps are needed.
#[cfg(target_os = "linux")]
fn scrub_one_env_var(key: &str) {
    use std::ffi::CString;
    if std::env::var(key).is_err() {
        return;
    }
    // `getenv` returns a pointer to the VALUE portion of the "KEY=VALUE"
    // string libc placed on the stack at startup. Zero it in place — that's
    // what `/proc/<pid>/environ` ultimately renders from.
    let ckey = CString::new(key).expect("env key has no NUL");
    unsafe {
        let mut p = libc::getenv(ckey.as_ptr());
        if !p.is_null() {
            while *p != 0 {
                *p = 0;
                p = p.add(1);
            }
        }
        std::env::remove_var(key);
    }
}

#[cfg(not(target_os = "linux"))]
fn scrub_one_env_var(key: &str) {
    // Non-Linux fallback: libc env scrub is Linux-specific; just unset.
    unsafe { std::env::remove_var(key); }
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
            scrub_api_key_env();
            Arc::new(OpenAiCompatibleClient::new(config))
        } else if let Ok(config) = AnthropicConfig::from_env() {
            tracing::info!(base_url = %config.base_url, "using Anthropic LLM client");
            scrub_api_key_env();
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
    let server = Server::with_llm_and_audit(llm_client.clone(), audit_log.clone());

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
    let socket_path = PathBuf::from("/run/agentd/agentd.sock");
    tracing::info!(socket = %socket_path.display(), "starting Unix socket listener for additional goals");
    server.listen(&socket_path).await?;

    Ok(())
}
