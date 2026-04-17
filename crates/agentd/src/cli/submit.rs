//! `agentd submit <goal>` — send a goal to Bootstrap, stream events, exit when done.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use aaos_core::{AgentId, AuditEvent};
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::signal::unix::{signal, SignalKind};

use crate::cli::client;
use crate::cli::errors::{exit_code, format_error, CliError};
use crate::cli::output::{format_operator_line, is_operator_visible, is_stdout_tty};

pub async fn run(goal: String, verbose: bool, socket: PathBuf) -> anyhow::Result<()> {
    let mut reader =
        match client::call_streaming(&socket, "agent.submit_streaming", json!({ "goal": goal }))
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
    let mut first_sigint_at: Option<Instant> = None;

    let colorize = is_stdout_tty();
    let mut name_cache: HashMap<AgentId, String> = HashMap::new();
    let mut line = String::new();

    loop {
        tokio::select! {
            _ = sigint.recv() => {
                match first_sigint_at {
                    None => {
                        eprintln!("detaching — agent continues. Use `agentd stop <id>` to terminate. Ctrl-C again to abort.");
                        first_sigint_at = Some(Instant::now());
                    }
                    Some(t) if t.elapsed() < Duration::from_secs(2) => {
                        std::process::exit(4);
                    }
                    _ => {
                        first_sigint_at = Some(Instant::now());
                    }
                }
            }
            n = reader.read_line(&mut line) => {
                let n = n.map_err(|e| {
                    eprint!("{}", format_error(&CliError::Io(e)));
                    anyhow::anyhow!("read failed")
                })?;
                if n == 0 {
                    // EOF without end frame.
                    eprint!("{}", format_error(&CliError::BrokenPipe));
                    std::process::exit(3);
                }

                let frame: Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => {
                        line.clear();
                        continue;
                    }
                };
                line.clear();

                match frame.get("kind").and_then(|k| k.as_str()) {
                    Some("final_text") => {
                        if let Some(text) = frame.get("text").and_then(|t| t.as_str()) {
                            // Blank line before Bootstrap's answer so it stands
                            // clearly apart from the audit stream above.
                            println!();
                            println!("{}", text);
                            println!();
                        }
                    }
                    Some("end") => {
                        let exit = frame.get("exit_code").and_then(|c| c.as_i64()).unwrap_or(0) as i32;
                        let input_tokens = frame.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        let output_tokens = frame.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        let elapsed_ms = frame.get("elapsed_ms").and_then(|e| e.as_u64()).unwrap_or(0);

                        let ts = chrono::Local::now().format("%H:%M:%S");
                        let name_col = format!("{:<12}", "bootstrap");
                        let name_col = if colorize {
                            format!("\x1b[2m{}\x1b[0m", name_col)
                        } else {
                            name_col
                        };
                        let status_word = if exit == 0 { "complete" } else { "failed" };
                        let colored_status = if colorize {
                            let code = if exit == 0 { 32 } else { 31 };
                            format!("\x1b[{}m{}\x1b[0m", code, status_word)
                        } else {
                            status_word.to_string()
                        };
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
                                    // Fallback: 8-char prefix of the uuid.
                                    // Real-name lookup via agent.status would be one round-trip
                                    // per new agent; deferred to a later task.
                                    event.agent_id.to_string().chars().take(8).collect::<String>()
                                })
                                .clone();
                            println!("{}", format_operator_line(&event, &name, colorize));
                        }
                    }
                    _ => {
                        // Unknown frame kind; ignore (forward-compatible with future kinds).
                    }
                }
            }
        }
    }
}
