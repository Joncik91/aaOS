//! `agentd logs <agent_id>` — attach to a running agent's audit stream.
//!
//! Differs from submit: no goal injection, prefix resolution first, single
//! Ctrl-C detaches cleanly (no grace-period dance), clean EOF on stream close.

use std::collections::HashMap;
use std::path::PathBuf;

use aaos_core::{AgentId, AuditEvent};
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::signal::unix::{signal, SignalKind};

use crate::cli::client;
use crate::cli::errors::{exit_code, format_error, CliError};
use crate::cli::output::{format_operator_line, is_operator_visible, is_stdout_tty};
use crate::cli::prefix::{resolve_prefix, PrefixError};

pub async fn run(agent_id: String, verbose: bool, socket: PathBuf) -> anyhow::Result<()> {
    // Step 1: list agents so we can resolve the prefix.
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

    // Step 2: attach to the stream.
    let mut reader = match client::call_streaming(
        &socket,
        "agent.logs_streaming",
        json!({ "agent_id": full_id }),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            eprint!("{}", format_error(&e));
            std::process::exit(exit_code(&e));
        }
    };

    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| anyhow::anyhow!("failed to install SIGINT handler: {}", e))?;

    let colorize = is_stdout_tty();
    let mut name_cache: HashMap<AgentId, String> = HashMap::new();
    let short_target: String = full_id.chars().take(8).collect();
    let mut line = String::new();

    loop {
        tokio::select! {
            _ = sigint.recv() => {
                eprintln!("detaching.");
                return Ok(());
            }
            n = reader.read_line(&mut line) => {
                let n = n.map_err(|e| {
                    eprint!("{}", format_error(&CliError::Io(e)));
                    anyhow::anyhow!("read failed")
                })?;
                if n == 0 {
                    // Server closed the stream cleanly (e.g. agent terminated
                    // on the server side without emitting an end frame). Exit 0.
                    return Ok(());
                }

                let frame: Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => { line.clear(); continue; }
                };
                line.clear();

                match frame.get("kind").and_then(|k| k.as_str()) {
                    Some("end") => {
                        let exit = frame.get("exit_code").and_then(|c| c.as_i64()).unwrap_or(0) as i32;
                        let input_tokens = frame.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        let output_tokens = frame.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        let elapsed_ms = frame.get("elapsed_ms").and_then(|e| e.as_u64()).unwrap_or(0);
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        let name_col = format!("{:<12}", short_target);
                        let name_col = if colorize {
                            format!("\x1b[2m{}\x1b[0m", name_col)
                        } else { name_col };
                        let status_word = if exit == 0 { "complete" } else { "failed" };
                        let colored_status = if colorize {
                            let code = if exit == 0 { 32 } else { 31 };
                            format!("\x1b[{}m{}\x1b[0m", code, status_word)
                        } else { status_word.to_string() };
                        let tokens_in_k = input_tokens / 1000;
                        let tokens_out_k = output_tokens / 1000;
                        let secs = (elapsed_ms as f64 / 1000.0).round() as u64;
                        println!(
                            "[{}] {}{} ({}k in / {}k out, {}s)",
                            ts, name_col, colored_status, tokens_in_k, tokens_out_k, secs
                        );
                        std::process::exit(exit);
                    }
                    Some("lag") => {
                        let missed = frame.get("missed").and_then(|m| m.as_u64()).unwrap_or(0);
                        eprintln!("[lag] dropped {} events (broadcast buffer overflow)", missed);
                    }
                    Some("event") => {
                        let inner = match frame.get("event") {
                            Some(e) => e,
                            None => continue,
                        };
                        let event: AuditEvent = match serde_json::from_value(inner.clone()) {
                            Ok(e) => e,
                            Err(_) => continue,
                        };
                        if verbose {
                            println!("{}", inner);
                        } else if is_operator_visible(&event) {
                            let name = name_cache
                                .entry(event.agent_id)
                                .or_insert_with(|| {
                                    event.agent_id.to_string().chars().take(8).collect::<String>()
                                })
                                .clone();
                            println!("{}", format_operator_line(&event, &name, colorize));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
