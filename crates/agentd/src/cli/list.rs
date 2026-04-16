//! `agentd list` — show running agents as a table or JSON.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::cli::client;
use crate::cli::errors::{exit_code, format_error};

pub async fn run(json_flag: bool, socket: PathBuf) -> anyhow::Result<()> {
    let result = match client::call_sync(&socket, "agent.list", json!({})).await {
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

    let agents = result
        .get("agents")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    if agents.is_empty() {
        println!("No agents running.");
        return Ok(());
    }

    println!(
        "{:<10} {:<20} {:<10} {:<10}",
        "ID", "NAME", "STATE", "UPTIME"
    );
    for a in agents {
        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let id_short: String = id.chars().take(8).collect();
        let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let name_trunc: String = name.chars().take(20).collect();
        let state = a.get("state").and_then(|v| v.as_str()).unwrap_or("?");
        let uptime = format_uptime(a.get("started_at"));
        println!(
            "{:<10} {:<20} {:<10} {:<10}",
            id_short, name_trunc, state, uptime
        );
    }

    Ok(())
}

fn format_uptime(started_at: Option<&Value>) -> String {
    let Some(v) = started_at else {
        return "—".to_string();
    };
    let Some(s) = v.as_str() else {
        return "—".to_string();
    };
    let parsed = chrono::DateTime::parse_from_rfc3339(s);
    let Ok(t) = parsed else {
        return "—".to_string();
    };
    let started_utc = t.with_timezone(&chrono::Utc);
    let d = chrono::Utc::now().signed_duration_since(started_utc);
    let total_secs = d.num_seconds().max(0);
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 99 {
        format!("99+:{:02}:{:02}", minutes, seconds)
    } else {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_missing_field_is_emdash() {
        let s = format_uptime(None);
        assert_eq!(s, "—");
    }

    #[test]
    fn uptime_non_string_is_emdash() {
        let s = format_uptime(Some(&json!(42)));
        assert_eq!(s, "—");
    }

    #[test]
    fn uptime_unparseable_is_emdash() {
        let s = format_uptime(Some(&json!("not-a-date")));
        assert_eq!(s, "—");
    }

    #[test]
    fn uptime_recent_is_hh_mm_ss() {
        // ~65 seconds ago.
        let recent = chrono::Utc::now() - chrono::Duration::seconds(65);
        let rfc = recent.to_rfc3339();
        let s = format_uptime(Some(&json!(rfc)));
        // ~1 minute ago → "00:01:NN" where NN is roughly 5.
        assert!(s.starts_with("00:01:"), "got {:?}", s);
    }

    #[test]
    fn uptime_future_clamps_to_zero() {
        // If started_at is in the future (clock skew), don't show negative.
        let future = chrono::Utc::now() + chrono::Duration::seconds(10);
        let rfc = future.to_rfc3339();
        let s = format_uptime(Some(&json!(rfc)));
        assert_eq!(s, "00:00:00");
    }
}
