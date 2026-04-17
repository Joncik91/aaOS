//! `agentd status <agent_id>` — detail view for one agent. Prefix-resolves
//! the id against the live agent list first.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::cli::client;
use crate::cli::errors::{exit_code, format_error, CliError};
use crate::cli::prefix::{resolve_prefix, PrefixError};

pub async fn run(agent_id: String, json_flag: bool, socket: PathBuf) -> anyhow::Result<()> {
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

    let result =
        match client::call_sync(&socket, "agent.status", json!({ "agent_id": full_id })).await {
            Ok(r) => r,
            Err(e) => {
                eprint!("{}", format_error(&e));
                std::process::exit(exit_code(&e));
            }
        };

    if json_flag {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).unwrap_or_default()
        );
        return Ok(());
    }

    println!("{:<15}{}", "ID:", field_str(&result, "id"));
    println!("{:<15}{}", "Name:", field_str(&result, "name"));
    println!("{:<15}{}", "Model:", field_str(&result, "model"));
    println!("{:<15}{}", "State:", field_str(&result, "state"));
    println!(
        "{:<15}{}",
        "Parent:",
        format_parent(result.get("parent_agent"))
    );
    println!(
        "{:<15}{}",
        "Capabilities:",
        result
            .get("capability_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    );

    Ok(())
}

fn field_str<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("?")
}

fn format_parent(parent: Option<&Value>) -> String {
    match parent {
        Some(v) if v.is_null() => "—".to_string(),
        None => "—".to_string(),
        Some(v) => match v.as_str() {
            Some(s) if !s.is_empty() => s.chars().take(8).collect(),
            _ => "—".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_null_renders_emdash() {
        assert_eq!(format_parent(Some(&json!(null))), "—");
    }

    #[test]
    fn parent_missing_renders_emdash() {
        assert_eq!(format_parent(None), "—");
    }

    #[test]
    fn parent_string_renders_8_char_prefix() {
        let s = "a3b7c9d2-1111-2222-3333-444455556666";
        assert_eq!(format_parent(Some(&json!(s))), "a3b7c9d2");
    }

    #[test]
    fn parent_empty_string_renders_emdash() {
        assert_eq!(format_parent(Some(&json!(""))), "—");
    }
}
