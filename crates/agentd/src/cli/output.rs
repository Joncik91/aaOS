//! Operator-facing rendering of audit events.
//!
//! Two filter levels:
//!   * Default (operator view): a short list of event kinds, rendered with
//!     agent-name + action + optional color.
//!   * Verbose (--verbose): raw NDJSON, no filter. Handled by the caller,
//!     not this module.
//!
//! This module exposes the filter predicate, the formatter, and a tty check
//! for choosing colors.

use aaos_core::{AuditEvent, AuditEventKind, ToolExecutionSurface};

pub fn is_operator_visible(event: &AuditEvent) -> bool {
    match &event.event {
        AuditEventKind::AgentSpawned { .. }
        | AuditEventKind::ToolInvoked { .. }
        | AuditEventKind::AgentExecutionCompleted { .. }
        | AuditEventKind::AgentLoopStopped { .. }
        | AuditEventKind::CapabilityDenied { .. }
        | AuditEventKind::SubtaskTtlExpired { .. }
        | AuditEventKind::SubtaskModelEscalated { .. }
        | AuditEventKind::ToolRepeatGuardFired { .. }
        | AuditEventKind::OrchestrationSelected { .. } => true,
        // Show only failed tool results — successes are implied by
        // the next event in the stream and would double the noise.
        AuditEventKind::ToolResult { success, .. } => !success,
        _ => false,
    }
}

pub fn format_operator_line(event: &AuditEvent, agent_name: &str, colorize: bool) -> String {
    let ts = event.timestamp.format("%H:%M:%S");
    let name_col_text = format!("{:<12}", agent_name);
    let name_col = if colorize {
        format!("\x1b[2m{}\x1b[0m", name_col_text)
    } else {
        name_col_text
    };

    let body = match &event.event {
        AuditEventKind::AgentSpawned { manifest_name } => {
            format!("spawned {}", manifest_name)
        }
        AuditEventKind::ToolInvoked {
            tool,
            args_preview,
            execution_surface,
            ..
        } => {
            let tool_col = if colorize {
                format!("\x1b[36m{}\x1b[0m", tool)
            } else {
                tool.clone()
            };
            let surface_tag = if colorize {
                match execution_surface {
                    ToolExecutionSurface::Worker => "\x1b[32m[worker]\x1b[0m",
                    ToolExecutionSurface::Daemon => "\x1b[2m[daemon]\x1b[0m",
                }
                .to_string()
            } else {
                format!("[{}]", execution_surface.as_str())
            };
            match args_preview {
                Some(a) if !a.is_empty() => format!("tool: {} {} {}", tool_col, surface_tag, a),
                _ => format!("tool: {} {}", tool_col, surface_tag),
            }
        }
        AuditEventKind::ToolResult {
            tool,
            success,
            result_preview,
        } => {
            let base = if *success {
                format!("tool ok: {}", tool)
            } else {
                format!("tool FAILED: {}", tool)
            };
            let text = match result_preview {
                Some(p) if !p.is_empty() => format!("{} — {}", base, p),
                _ => base,
            };
            if colorize && !success {
                format!("\x1b[31m{}\x1b[0m", text)
            } else {
                text
            }
        }
        AuditEventKind::AgentExecutionCompleted { .. } => {
            if colorize {
                "\x1b[32mcomplete\x1b[0m".to_string()
            } else {
                "complete".to_string()
            }
        }
        AuditEventKind::AgentLoopStopped { reason, .. } => {
            let (text, color) = match reason.as_str() {
                "budget_exceeded" => ("budget exceeded".to_string(), true),
                "error" => ("failed".to_string(), true),
                other => (format!("stopped ({})", other), false),
            };
            if colorize && color {
                format!("\x1b[31m{}\x1b[0m", text)
            } else {
                text
            }
        }
        AuditEventKind::CapabilityDenied { capability, reason } => {
            let text = format!("capability denied: {:?} — {}", capability, reason);
            if colorize {
                format!("\x1b[31m{}\x1b[0m", text)
            } else {
                text
            }
        }
        AuditEventKind::SubtaskTtlExpired { subtask_id, reason } => {
            let label = format!("TTL expired ({}): {}", reason, subtask_id);
            if colorize {
                format!("\x1b[33m{}\x1b[0m", label) // yellow — warning semantics
            } else {
                label
            }
        }
        AuditEventKind::SubtaskModelEscalated {
            subtask_id,
            from_model,
            to_model,
            reason,
            ..
        } => {
            let label =
                format!("model escalated ({reason}): {subtask_id} — {from_model} → {to_model}");
            if colorize {
                format!("\x1b[36m{label}\x1b[0m") // cyan — informational
            } else {
                label
            }
        }
        AuditEventKind::ToolRepeatGuardFired {
            tool,
            attempt_count,
            ..
        } => {
            let label = format!("repeat guard: {tool} (attempt {attempt_count})");
            if colorize {
                format!("\x1b[33m{label}\x1b[0m") // yellow — warning
            } else {
                label
            }
        }
        AuditEventKind::OrchestrationSelected { mode, source } => {
            let label = format!("orchestration: {mode} (source: {source})");
            if colorize {
                format!("\x1b[2m{label}\x1b[0m") // dim — informational
            } else {
                label
            }
        }
        other => format!("{:?}", other),
    };

    format!("[{}] {}{}", ts, name_col, body)
}

pub fn is_stdout_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, AuditEvent, AuditEventKind, Capability};

    fn evt(kind: AuditEventKind) -> AuditEvent {
        AuditEvent::new(AgentId::new(), kind)
    }

    // ---- is_operator_visible ----

    #[test]
    fn visible_agent_spawned() {
        assert!(is_operator_visible(&evt(AuditEventKind::AgentSpawned {
            manifest_name: "fetcher".into(),
        })));
    }

    #[test]
    fn visible_tool_invoked() {
        assert!(is_operator_visible(&evt(AuditEventKind::ToolInvoked {
            tool: "web_fetch".into(),
            input_hash: "h".into(),
            args_preview: None,
            execution_surface: aaos_core::ToolExecutionSurface::Daemon,
        })));
    }

    #[test]
    fn visible_agent_execution_completed() {
        assert!(is_operator_visible(&evt(
            AuditEventKind::AgentExecutionCompleted {
                stop_reason: "done".into(),
                total_iterations: 1,
            }
        )));
    }

    #[test]
    fn visible_agent_loop_stopped() {
        assert!(is_operator_visible(&evt(
            AuditEventKind::AgentLoopStopped {
                reason: "shutdown".into(),
                messages_processed: 5,
            }
        )));
    }

    #[test]
    fn visible_capability_denied() {
        assert!(is_operator_visible(&evt(
            AuditEventKind::CapabilityDenied {
                capability: Capability::FileRead {
                    path_glob: "/etc/*".into(),
                },
                reason: "not in allowed paths".into(),
            }
        )));
    }

    #[test]
    fn visible_subtask_ttl_expired() {
        assert!(is_operator_visible(&evt(
            AuditEventKind::SubtaskTtlExpired {
                subtask_id: "task-123".into(),
                reason: "wall_clock_exceeded".into(),
            }
        )));
    }

    #[test]
    fn hidden_usage_reported() {
        assert!(!is_operator_visible(&evt(AuditEventKind::UsageReported {
            input_tokens: 100,
            output_tokens: 50,
        })));
    }

    #[test]
    fn hidden_memory_queried() {
        assert!(!is_operator_visible(&evt(AuditEventKind::MemoryQueried {
            query_hash: "h".into(),
            results_count: 3,
        })));
    }

    #[test]
    fn hidden_tool_result() {
        assert!(!is_operator_visible(&evt(AuditEventKind::ToolResult {
            tool: "t".into(),
            success: true,
            result_preview: None,
        })));
    }

    // ---- format_operator_line ----

    #[test]
    fn format_spawned_line() {
        let e = evt(AuditEventKind::AgentSpawned {
            manifest_name: "fetcher".into(),
        });
        let s = format_operator_line(&e, "bootstrap", false);
        assert!(s.contains("bootstrap"));
        assert!(s.contains("spawned"));
        assert!(s.contains("fetcher"));
    }

    #[test]
    fn format_tool_invoked_line() {
        let e = evt(AuditEventKind::ToolInvoked {
            tool: "web_fetch".into(),
            input_hash: "h".into(),
            args_preview: None,
            execution_surface: aaos_core::ToolExecutionSurface::Daemon,
        });
        let s = format_operator_line(&e, "fetcher", false);
        assert!(s.contains("web_fetch"));
        assert!(s.contains("tool:"));
    }

    #[test]
    fn format_complete_line() {
        let e = evt(AuditEventKind::AgentExecutionCompleted {
            stop_reason: "done".into(),
            total_iterations: 1,
        });
        let s = format_operator_line(&e, "bootstrap", false);
        assert!(s.contains("complete"));
        assert!(!s.contains("\x1b["), "no color escapes when colorize=false");
    }

    #[test]
    fn format_loop_stopped_error_renders_failed() {
        let e = evt(AuditEventKind::AgentLoopStopped {
            reason: "error".into(),
            messages_processed: 3,
        });
        let s = format_operator_line(&e, "x", false);
        assert!(s.contains("failed"));
    }

    #[test]
    fn format_loop_stopped_budget_renders_budget_exceeded() {
        let e = evt(AuditEventKind::AgentLoopStopped {
            reason: "budget_exceeded".into(),
            messages_processed: 3,
        });
        let s = format_operator_line(&e, "x", false);
        assert!(s.contains("budget exceeded"));
    }

    #[test]
    fn format_loop_stopped_other_reason_renders_stopped_with_reason() {
        let e = evt(AuditEventKind::AgentLoopStopped {
            reason: "user_cancelled".into(),
            messages_processed: 3,
        });
        let s = format_operator_line(&e, "x", false);
        assert!(s.contains("stopped"));
        assert!(s.contains("user_cancelled"));
    }

    #[test]
    fn format_capability_denied_line() {
        let e = evt(AuditEventKind::CapabilityDenied {
            capability: Capability::FileRead {
                path_glob: "/etc/*".into(),
            },
            reason: "not granted".into(),
        });
        let s = format_operator_line(&e, "child", false);
        assert!(s.contains("capability denied"));
        assert!(s.contains("not granted"));
    }

    #[test]
    fn format_ttl_expired_line() {
        let e = evt(AuditEventKind::SubtaskTtlExpired {
            subtask_id: "x".into(),
            reason: "wall_clock_exceeded".into(),
        });
        let s = format_operator_line(&e, "bootstrap", false);
        assert!(s.contains("TTL expired"), "got: {}", s);
        assert!(s.contains("wall_clock_exceeded"), "got: {}", s);
        assert!(s.contains("x"), "got: {}", s);
    }

    // ---- Colorization ----

    #[test]
    fn colorize_false_emits_no_escapes_for_any_visible_event() {
        let events = vec![
            evt(AuditEventKind::AgentSpawned {
                manifest_name: "f".into(),
            }),
            evt(AuditEventKind::ToolInvoked {
                tool: "t".into(),
                input_hash: "h".into(),
                args_preview: None,
                execution_surface: aaos_core::ToolExecutionSurface::Daemon,
            }),
            evt(AuditEventKind::AgentExecutionCompleted {
                stop_reason: "d".into(),
                total_iterations: 1,
            }),
            evt(AuditEventKind::AgentLoopStopped {
                reason: "error".into(),
                messages_processed: 1,
            }),
            evt(AuditEventKind::CapabilityDenied {
                capability: Capability::FileRead {
                    path_glob: "/".into(),
                },
                reason: "r".into(),
            }),
        ];
        for e in &events {
            let s = format_operator_line(e, "x", false);
            assert!(!s.contains('\x1b'), "unexpected ANSI in {:?}", s);
        }
    }

    #[test]
    fn colorize_true_colors_complete_green() {
        let e = evt(AuditEventKind::AgentExecutionCompleted {
            stop_reason: "done".into(),
            total_iterations: 1,
        });
        let s = format_operator_line(&e, "x", true);
        assert!(s.contains("\x1b[32m"), "expected green for complete: {}", s);
    }

    #[test]
    fn colorize_true_colors_failed_red() {
        let e = evt(AuditEventKind::AgentLoopStopped {
            reason: "error".into(),
            messages_processed: 1,
        });
        let s = format_operator_line(&e, "x", true);
        assert!(s.contains("\x1b[31m"), "expected red for failed: {}", s);
    }

    #[test]
    fn colorize_true_colors_tool_cyan() {
        let e = evt(AuditEventKind::ToolInvoked {
            tool: "file_write".into(),
            input_hash: "h".into(),
            args_preview: None,
            execution_surface: aaos_core::ToolExecutionSurface::Daemon,
        });
        let s = format_operator_line(&e, "x", true);
        assert!(s.contains("\x1b[36m"), "expected cyan for tool: {}", s);
    }

    #[test]
    fn tool_invoked_shows_worker_tag_when_surface_is_worker() {
        let e = evt(AuditEventKind::ToolInvoked {
            tool: "file_write".into(),
            input_hash: "h".into(),
            args_preview: None,
            execution_surface: aaos_core::ToolExecutionSurface::Worker,
        });
        let colored = format_operator_line(&e, "x", true);
        assert!(
            colored.contains("[worker]"),
            "missing [worker] tag: {}",
            colored
        );
        assert!(
            colored.contains("\x1b[32m"),
            "expected green for worker: {}",
            colored
        );
        let plain = format_operator_line(&e, "x", false);
        assert!(
            plain.contains("[worker]"),
            "plain missing [worker] tag: {}",
            plain
        );
    }

    #[test]
    fn tool_invoked_shows_daemon_tag_when_surface_is_daemon() {
        let e = evt(AuditEventKind::ToolInvoked {
            tool: "web_fetch".into(),
            input_hash: "h".into(),
            args_preview: None,
            execution_surface: aaos_core::ToolExecutionSurface::Daemon,
        });
        let plain = format_operator_line(&e, "x", false);
        assert!(
            plain.contains("[daemon]"),
            "missing [daemon] tag: {}",
            plain
        );
    }

    #[test]
    fn colorize_true_dims_agent_name() {
        let e = evt(AuditEventKind::AgentSpawned {
            manifest_name: "f".into(),
        });
        let s = format_operator_line(&e, "bootstrap", true);
        assert!(s.contains("\x1b[2m"), "expected dim for agent name: {}", s);
    }

    // ---- Timestamp ----

    #[test]
    fn model_escalated_is_operator_visible() {
        use aaos_core::{AgentId, AuditEvent, AuditEventKind};
        let e = AuditEvent::new(
            AgentId::from_uuid(uuid::Uuid::nil()),
            AuditEventKind::SubtaskModelEscalated {
                subtask_id: "analyze".into(),
                from_tier: 0,
                to_tier: 1,
                from_model: "deepseek-chat".into(),
                to_model: "deepseek-reasoner".into(),
                reason: "replan_retry".into(),
            },
        );
        assert!(is_operator_visible(&e));
        let rendered = format_operator_line(&e, "bootstrap", false);
        assert!(rendered.contains("model escalated"), "got: {rendered}");
        assert!(rendered.contains("replan_retry"), "got: {rendered}");
        assert!(rendered.contains("analyze"), "got: {rendered}");
        assert!(rendered.contains("deepseek-chat"), "got: {rendered}");
        assert!(rendered.contains("deepseek-reasoner"), "got: {rendered}");
    }

    #[test]
    fn tool_repeat_guard_is_operator_visible() {
        use aaos_core::{AgentId, AuditEvent, AuditEventKind};
        let e = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::ToolRepeatGuardFired {
                agent_id: AgentId::new(),
                tool: "web_fetch".into(),
                attempt_count: 3,
            },
        );
        assert!(is_operator_visible(&e));
        let rendered = format_operator_line(&e, "writer", false);
        assert!(rendered.contains("repeat guard"), "got: {rendered}");
        assert!(rendered.contains("web_fetch"), "got: {rendered}");
        assert!(rendered.contains("3"), "got: {rendered}");
    }

    #[test]
    fn format_line_includes_timestamp_in_hh_mm_ss() {
        let e = evt(AuditEventKind::AgentSpawned {
            manifest_name: "f".into(),
        });
        let s = format_operator_line(&e, "x", false);
        // Timestamp is at the start: "[HH:MM:SS]".
        assert!(s.starts_with('['), "{}", s);
        let close = s.find(']').expect("closing bracket");
        let ts = &s[1..close];
        assert_eq!(ts.len(), 8, "expected 8-char HH:MM:SS, got {:?}", ts);
        assert_eq!(ts.as_bytes()[2], b':');
        assert_eq!(ts.as_bytes()[5], b':');
    }
}
