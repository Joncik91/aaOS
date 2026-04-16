//! `agentd stop <agent_id>` — terminate a running agent by id (or unique prefix).

use std::path::PathBuf;

use serde_json::json;

use crate::cli::client;
use crate::cli::errors::{exit_code, format_error, CliError};
use crate::cli::prefix::{resolve_prefix, PrefixError};

pub async fn run(agent_id: String, socket: PathBuf) -> anyhow::Result<()> {
    let list = match client::call_sync(&socket, "agent.list", json!({})).await {
        Ok(r) => r,
        Err(e) => {
            eprint!("{}", format_error(&e));
            std::process::exit(exit_code(&e));
        }
    };
    let ids: Vec<String> = list
        .get("agents")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|a| a.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect();

    let full_id = match resolve_prefix(&agent_id, &ids) {
        Ok(id) => id,
        Err(PrefixError::NotFound) => {
            let e = CliError::Usage(format!("agent not found: {}", agent_id));
            eprint!("{}", format_error(&e));
            std::process::exit(exit_code(&e));
        }
        Err(PrefixError::Ambiguous(candidates)) => {
            let list = candidates
                .iter()
                .map(|c| c.chars().take(8).collect::<String>())
                .collect::<Vec<_>>()
                .join(", ");
            let e = CliError::Usage(format!("ambiguous prefix '{}': {}", agent_id, list));
            eprint!("{}", format_error(&e));
            std::process::exit(exit_code(&e));
        }
    };

    if let Err(e) = client::call_sync(
        &socket,
        "agent.stop",
        json!({ "agent_id": full_id }),
    )
    .await
    {
        eprint!("{}", format_error(&e));
        std::process::exit(exit_code(&e));
    }

    let short: String = full_id.chars().take(8).collect();
    println!("stopped {}", short);
    Ok(())
}
