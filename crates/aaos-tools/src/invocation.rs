use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aaos_core::{
    AgentId, AuditEvent, AuditEventKind, AuditLog, Capability, CapabilityHandle,
    CapabilityRegistry, CoreError, Result, ToolExecutionSurface,
};
use serde_json::Value;

use crate::context::InvocationContext;
use crate::registry::ToolRegistry;

/// Thin trait implemented by the broker-session adapter in `agentd`.
/// Structured error returned by `WorkerHandle::invoke_over_worker`.
///
/// `NoSession` is a distinguishable "this agent has no worker process"
/// condition — i.e., the agent was spawned via an inline path that
/// skipped `AgentBackend::launch`. `ToolInvocation::invoke` catches this
/// variant and falls back to daemon-side execution with the audit event
/// reporting `ToolExecutionSurface::Daemon` so operators see honestly
/// that the call was not confined. Other errors (`Transport`) are real
/// broker failures and propagate as `CoreError::Ipc`.
#[derive(Debug, Clone)]
pub enum WorkerInvokeError {
    /// No broker session exists for this agent. Legitimate when the
    /// agent was spawned via `run_subtask_inline` (does not go through
    /// `backend.launch`). Fall back to daemon-side execution.
    NoSession,
    /// Any other broker-side failure — timeout, transport error,
    /// worker panic, Landlock denial, etc. Caller converts to
    /// `CoreError::Ipc`.
    Transport(String),
}

impl std::fmt::Display for WorkerInvokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerInvokeError::NoSession => write!(f, "no broker session for agent"),
            WorkerInvokeError::Transport(m) => write!(f, "{m}"),
        }
    }
}

/// `ToolInvocation` holds an optional `Arc<dyn WorkerHandle>` and uses
/// it to forward invocations to the confined worker process when the
/// routing table says `ToolExecutionSurface::Worker`.
#[async_trait::async_trait]
pub trait WorkerHandle: Send + Sync {
    /// Return the backend kind for routing (`"namespaced"` or `"in_process"`).
    fn backend_kind(&self) -> &'static str;
    /// Forward a tool invocation across the broker. `tokens` carries the
    /// resolved `CapabilityToken` structs for the invoking agent so the
    /// worker can rebuild a per-call `CapabilityRegistry` and satisfy the
    /// tool's internal `ctx.capability_registry.permits()` check.
    async fn invoke_over_worker(
        &self,
        agent_id: AgentId,
        tool_name: &str,
        input: serde_json::Value,
        tokens: Vec<aaos_core::CapabilityToken>,
    ) -> std::result::Result<serde_json::Value, WorkerInvokeError>;
}

/// Handles tool invocations with capability enforcement and audit logging.
///
/// Every tool call goes through the invocation layer, which:
/// 1. Checks the agent's capability token permits the tool
/// 2. Validates input against the tool's schema
/// 3. Invokes the tool (daemon-side or worker-side depending on routing)
/// 4. Logs the invocation to the audit trail
pub struct ToolInvocation {
    registry: Arc<ToolRegistry>,
    audit_log: Arc<dyn AuditLog>,
    capability_registry: Arc<CapabilityRegistry>,
    repeat_counts: Mutex<HashMap<(AgentId, String, u64), u32>>,
    worker_handle: Option<Arc<dyn WorkerHandle>>,
}

impl ToolInvocation {
    pub fn new(
        registry: Arc<ToolRegistry>,
        audit_log: Arc<dyn AuditLog>,
        capability_registry: Arc<CapabilityRegistry>,
    ) -> Self {
        Self {
            registry,
            audit_log,
            capability_registry,
            repeat_counts: Mutex::new(HashMap::new()),
            worker_handle: None,
        }
    }

    /// Construct a `ToolInvocation` that can route tool calls to a confined
    /// worker process via the supplied `WorkerHandle`. The handle's
    /// `backend_kind()` determines routing — `"namespaced"` sends filesystem
    /// and compute tools to the worker; daemon-side tools (`web_fetch`,
    /// `cargo_run`, `git_commit`) stay in the daemon regardless.
    pub fn new_with_worker_handle(
        registry: Arc<ToolRegistry>,
        audit_log: Arc<dyn AuditLog>,
        capability_registry: Arc<CapabilityRegistry>,
        worker_handle: Arc<dyn WorkerHandle>,
    ) -> Self {
        Self {
            registry,
            audit_log,
            capability_registry,
            repeat_counts: Mutex::new(HashMap::new()),
            worker_handle: Some(worker_handle),
        }
    }

    /// Invoke a tool on behalf of an agent, enforcing capabilities.
    pub async fn invoke(
        &self,
        agent_id: AgentId,
        tool_name: &str,
        input: Value,
        token_handles: &[CapabilityHandle],
    ) -> Result<Value> {
        // Check capability — find the first handle that satisfies the grant so
        // we can call `authorize_and_record` on it after a successful invocation.
        let required = Capability::ToolInvoke {
            tool_name: tool_name.to_string(),
        };
        let matching_handle = token_handles
            .iter()
            .find(|h| self.capability_registry.permits(**h, agent_id, &required))
            .copied();

        if matching_handle.is_none() {
            self.audit_log.record(AuditEvent::new(
                agent_id,
                AuditEventKind::CapabilityDenied {
                    capability: required,
                    reason: format!("agent lacks tool:{tool_name} capability"),
                },
            ));
            return Err(CoreError::CapabilityDenied {
                agent_id,
                capability: Capability::ToolInvoke {
                    tool_name: tool_name.to_string(),
                },
                reason: "tool invocation not permitted".into(),
            });
        }

        // Determine the INTENDED execution surface. Actual surface may
        // downgrade to Daemon if the agent has no broker session —
        // legitimate when the agent was spawned via an inline path
        // (e.g., `run_subtask_inline`) that skipped `backend.launch`.
        // The actual surface is resolved after the call and recorded on
        // the audit event so operators see the honest truth.
        let intended_surface: ToolExecutionSurface = self
            .worker_handle
            .as_ref()
            .map(|h| crate::routing::route_for(tool_name, h.backend_kind()))
            .unwrap_or(ToolExecutionSurface::Daemon);

        // Log invocation
        let input_hash_u64 = md5_hash(&input);
        let input_hash = format!("{:x}", input_hash_u64);
        let args_preview = preview_value(&input);

        // Track repeat counts. Map grows while agentd runs; cap at
        // REPEAT_COUNTS_MAX entries and evict arbitrary entries when full
        // to bound memory. Eviction is coarse but correctness-preserving:
        // an evicted entry just means a cold tool call won't see the
        // repeat-guard hint until it crosses the threshold again.
        const REPEAT_COUNTS_MAX: usize = 1024;
        let repeat_key = (agent_id, tool_name.to_string(), input_hash_u64);
        let attempt_count: u32 = {
            let mut counts = self.repeat_counts.lock().unwrap_or_else(|e| e.into_inner());
            if counts.len() >= REPEAT_COUNTS_MAX && !counts.contains_key(&repeat_key) {
                // Drop a quarter of the map to amortize eviction cost.
                let drop_n = REPEAT_COUNTS_MAX / 4;
                let victims: Vec<_> = counts.keys().take(drop_n).cloned().collect();
                for k in victims {
                    counts.remove(&k);
                }
            }
            let entry = counts.entry(repeat_key.clone()).or_insert(0);
            *entry += 1;
            *entry
        };
        let threshold: u32 = std::env::var("AAOS_TOOL_REPEAT_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        let is_repeat = attempt_count >= threshold;
        if is_repeat {
            self.audit_log.record(AuditEvent::new(
                agent_id,
                AuditEventKind::ToolRepeat {
                    tool: tool_name.to_string(),
                    input_hash: input_hash.clone(),
                    attempt_count,
                },
            ));
        }

        // Filter handles relevant to this tool
        let filtered_handles: Vec<CapabilityHandle> = token_handles
            .iter()
            .filter(|h| {
                // We need to check what capability this handle's token represents.
                // Use inspect() — available in test/debug builds.
                // For production, we pass all handles and let the tool's capability
                // check via permits() filter appropriately.
                // Actually, the original filtering was by token capability type.
                // With handles, we can't inspect the token's capability type in production.
                // The solution: pass ALL handles; the tool's own permits() call will
                // correctly deny handles that don't match the requested capability type.
                let _ = h;
                true
            })
            .cloned()
            .collect();

        // Fork: Worker-routed calls go over the broker; Daemon calls go to
        // the local registry. The capability check above is surface-agnostic
        // and always runs in the daemon.
        //
        // Worker → NoSession downgrade: an agent spawned inline (e.g. via
        // run_subtask_inline) has no broker session. Fall back to daemon-
        // side execution and record the actual surface (Daemon) in the
        // audit event so operators see honestly that the call was not
        // confined. Any other worker error propagates as CoreError::Ipc.
        // Resolve CapabilityToken structs for forwarding to the worker.
        // The worker rebuilds a per-call registry from these so the tool's
        // own `ctx.capability_registry.permits()` call succeeds. Revoked or
        // expired tokens are resolved as-is (the worker's registry will deny
        // them, same as the daemon would — no special handling needed).
        let resolved_tokens = self
            .capability_registry
            .resolve_tokens(&filtered_handles, agent_id);

        let (actual_surface, mut result): (ToolExecutionSurface, Result<Value>) =
            match intended_surface {
                ToolExecutionSurface::Worker => {
                    let handle = self
                        .worker_handle
                        .as_ref()
                        .expect("intended_surface=Worker implies handle is Some");
                    match handle
                        .invoke_over_worker(agent_id, tool_name, input.clone(), resolved_tokens)
                        .await
                    {
                        Ok(v) => (ToolExecutionSurface::Worker, Ok(v)),
                        Err(WorkerInvokeError::NoSession) => {
                            let tool = self.registry.get(tool_name)?;
                            let ctx = InvocationContext {
                                agent_id,
                                tokens: filtered_handles.clone(),
                                capability_registry: self.capability_registry.clone(),
                            };
                            (
                                ToolExecutionSurface::Daemon,
                                tool.invoke(input.clone(), &ctx).await,
                            )
                        }
                        Err(e) => (
                            ToolExecutionSurface::Worker,
                            Err(CoreError::Ipc(e.to_string())),
                        ),
                    }
                }
                ToolExecutionSurface::Daemon => {
                    let tool = self.registry.get(tool_name)?;
                    let ctx = InvocationContext {
                        agent_id,
                        tokens: filtered_handles,
                        capability_registry: self.capability_registry.clone(),
                    };
                    (ToolExecutionSurface::Daemon, tool.invoke(input, &ctx).await)
                }
            };

        // Emit the ToolInvoked audit event AFTER the call so the
        // execution_surface field reflects what actually ran, not the
        // intent — important when Worker was intended but downgraded to
        // Daemon via the NoSession fallback above.
        self.audit_log.record(AuditEvent::new(
            agent_id,
            AuditEventKind::ToolInvoked {
                tool: tool_name.to_string(),
                input_hash,
                args_preview: Some(args_preview),
                execution_surface: actual_surface,
            },
        ));

        // Record the capability use against the token that authorized this
        // call.  `permits()` was a non-consuming check; `authorize_and_record`
        // is the consuming one that enforces max_invocations.  Called only on
        // success because the tool already ran — if the token expired or was
        // revoked in the narrow window between `permits` and here we warn and
        // continue (can't undo the execution).
        if result.is_ok() {
            if let Some(handle) = matching_handle {
                if let Err(e) = self
                    .capability_registry
                    .authorize_and_record(handle, agent_id, &required)
                {
                    tracing::warn!(
                        tool = tool_name,
                        agent_id = %agent_id,
                        reason = ?e,
                        "authorize_and_record failed after successful tool invocation \
                         (token expired/revoked mid-call); invocation count not recorded",
                    );
                }
            }
        }

        // Log result (with a bounded preview for operator observability).
        let result_preview = match &result {
            Ok(v) => Some(preview_value(v)),
            Err(e) => Some(preview_str(&e.to_string())),
        };
        self.audit_log.record(AuditEvent::new(
            agent_id,
            AuditEventKind::ToolResult {
                tool: tool_name.to_string(),
                success: result.is_ok(),
                result_preview,
            },
        ));

        // Inject repeat guard hint if threshold reached
        if is_repeat {
            // Phase F-b sub-project 2: emit a dedicated audit event so the
            // plan executor can detect the signal from the broadcast
            // stream without introspecting tool-result JSON. The existing
            // `_repeat_guard` hint stays — it's LLM-visible and is what
            // actually nudges the agent.
            self.audit_log.record(AuditEvent::new(
                agent_id,
                AuditEventKind::ToolRepeatGuardFired {
                    agent_id,
                    tool: tool_name.to_string(),
                    attempt_count,
                },
            ));

            let hint = format!(
                "You have called `{}` with these exact arguments {} times in this subtask. The previous attempts returned the same result. Try different arguments or a different tool.",
                tool_name, attempt_count
            );
            if let Ok(ref mut v) = result {
                if let Some(obj) = v.as_object_mut() {
                    use serde_json::json;
                    obj.insert(
                        "_repeat_guard".to_string(),
                        json!({ "attempt_count": attempt_count, "hint": hint }),
                    );
                }
            }
            // For Err, we cannot mutate the CoreError in place. The audit
            // event already fired above; the LLM will see the error string
            // on its own. We intentionally don't rewrite the error to
            // include the hint — the audit event + streaming output is the
            // operator-side signal. The LLM's recovery path for errors
            // happens already via the existing tool.invoke() error return.
        }

        // Observability: surface tool failures in the daemon log so operators
        // can diagnose without replaying the LLM's tool_call response text.
        // The audit event only carries success/false — not the actual error
        // string — by design (audit events are a stable schema). The tracing
        // log is the right place for free-form diagnostic detail.
        if let Err(ref e) = result {
            tracing::warn!(
                tool = tool_name,
                agent_id = %agent_id,
                error = %e,
                "tool invocation failed",
            );
        }

        result
    }
}

/// Simple hash for audit logging (not cryptographic).
fn md5_hash(value: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.to_string().hash(&mut hasher);
    hasher.finish()
}

const PREVIEW_CAP: usize = 200;

/// Produces a bounded, operator-readable summary of a JSON value for audit
/// events. Strings get a direct byte-capped excerpt; objects and arrays
/// get their compact JSON representation byte-capped. Null collapses to
/// "null". Keeps the preview stable enough to be useful without leaking
/// large payloads into every audit consumer.
fn preview_value(v: &Value) -> String {
    match v {
        Value::String(s) => preview_str(s),
        Value::Null => "null".into(),
        _ => preview_str(&v.to_string()),
    }
}

/// Truncates a string at `PREVIEW_CAP` bytes, respecting UTF-8 boundaries
/// (never splits a codepoint). Appends a trailing `…` marker when truncated.
fn preview_str(s: &str) -> String {
    if s.len() <= PREVIEW_CAP {
        return s.to_string();
    }
    let mut end = PREVIEW_CAP;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
impl ToolInvocation {
    /// Test-only: peek the current repeat count for a key. Returns 0
    /// if the key has never been seen.
    pub fn test_repeat_count(
        &self,
        agent_id: aaos_core::AgentId,
        tool: &str,
        input: &Value,
    ) -> u32 {
        let hash = md5_hash(input);
        let counts = self.repeat_counts.lock().unwrap_or_else(|e| e.into_inner());
        counts
            .get(&(agent_id, tool.to_string(), hash))
            .copied()
            .unwrap_or(0)
    }

    /// Test-only: the current size of the repeat-counts map.
    #[cfg(test)]
    pub fn test_repeat_counts_len(&self) -> usize {
        self.repeat_counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

#[cfg(test)]
#[allow(clippy::type_complexity)]
mod tests {
    use super::*;
    use crate::tool::EchoTool;
    use aaos_core::{CapabilityRegistry, CapabilityToken, Constraints, InMemoryAuditLog};
    use std::sync::Arc;

    fn setup() -> (
        ToolInvocation,
        AgentId,
        Vec<CapabilityHandle>,
        Arc<InMemoryAuditLog>,
        Arc<CapabilityRegistry>,
    ) {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(EchoTool));
        let log = Arc::new(InMemoryAuditLog::new());
        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "echo".into(),
            },
            Constraints::default(),
        );
        let cap_registry = Arc::new(CapabilityRegistry::new());
        let handle = cap_registry.insert(agent_id, token);
        let invocation = ToolInvocation::new(registry, log.clone(), cap_registry.clone());
        (invocation, agent_id, vec![handle], log, cap_registry)
    }

    #[tokio::test]
    async fn invoke_with_capability() {
        let (invocation, agent_id, handles, log, _cap_registry) = setup();
        let result = invocation
            .invoke(
                agent_id,
                "echo",
                serde_json::json!({"message": "hi"}),
                &handles,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({"message": "hi"}));
        assert!(log.len() >= 2); // invocation + result
    }

    #[tokio::test]
    async fn invoke_without_capability() {
        let (invocation, agent_id, _handles, log, _cap_registry) = setup();
        let result = invocation
            .invoke(agent_id, "echo", serde_json::json!({}), &[]) // no handles
            .await;
        assert!(result.is_err());
        // Should have logged the denial
        assert!(!log.is_empty());
    }

    // ---- preview helpers ----

    #[test]
    fn preview_str_short_passes_through() {
        assert_eq!(super::preview_str("hello"), "hello");
    }

    #[test]
    fn preview_str_long_truncates_with_marker() {
        let long = "a".repeat(500);
        let out = super::preview_str(&long);
        assert!(out.ends_with('…'));
        assert!(
            out.len() <= super::PREVIEW_CAP + 4,
            "utf-8 marker adds up to 3 bytes"
        );
    }

    #[test]
    fn preview_str_respects_utf8_boundary() {
        // 100 chars, each 3 bytes (kanji), total 300 bytes — forces
        // truncation mid-cap and must not split a codepoint.
        let s: String = "日".repeat(100);
        let out = super::preview_str(&s);
        // Round-trip through String ensures no invalid utf-8 slipped through.
        assert!(out.chars().count() > 0);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn preview_value_handles_string() {
        let v = serde_json::json!("simple string");
        assert_eq!(super::preview_value(&v), "simple string");
    }

    #[test]
    fn preview_value_handles_object() {
        let v = serde_json::json!({"url": "https://example.com", "method": "GET"});
        let out = super::preview_value(&v);
        assert!(out.contains("example.com"));
    }

    #[test]
    fn preview_value_handles_null() {
        assert_eq!(super::preview_value(&serde_json::Value::Null), "null");
    }

    #[tokio::test]
    async fn invoke_populates_args_and_result_preview() {
        let (invocation, agent_id, handles, log, _cap_registry) = setup();
        let _ = invocation
            .invoke(
                agent_id,
                "echo",
                serde_json::json!({"message": "hello-audit"}),
                &handles,
            )
            .await
            .unwrap();
        let events = log.events();
        let invoked = events.iter().find_map(|e| match &e.event {
            AuditEventKind::ToolInvoked { args_preview, .. } => args_preview.clone(),
            _ => None,
        });
        let result = events.iter().find_map(|e| match &e.event {
            AuditEventKind::ToolResult { result_preview, .. } => result_preview.clone(),
            _ => None,
        });
        assert!(
            invoked
                .as_ref()
                .map(|s| s.contains("hello-audit"))
                .unwrap_or(false),
            "ToolInvoked args_preview should contain the input message; got {:?}",
            invoked
        );
        assert!(
            result.is_some(),
            "ToolResult must carry a preview even on success"
        );
    }

    #[tokio::test]
    async fn invoke_nonexistent_tool() {
        let (invocation, agent_id, _handles, _log, cap_registry) = setup();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "*".into(),
            },
            Constraints::default(),
        );
        let handle = cap_registry.insert(agent_id, token);
        let result = invocation
            .invoke(agent_id, "nonexistent", serde_json::json!({}), &[handle])
            .await;
        assert!(result.is_err());
    }

    // ---- repeat-guard tests (run 11 feature) ----

    #[tokio::test]
    async fn first_two_calls_have_no_repeat_guard() {
        let (invocation, agent_id, handles, _log, _) = setup();
        let input = serde_json::json!({"message": "hello"});
        for _ in 0..2 {
            let result = invocation
                .invoke(agent_id, "echo", input.clone(), &handles)
                .await
                .unwrap();
            assert!(
                result.get("_repeat_guard").is_none(),
                "first two calls must not carry a repeat_guard; got {:?}",
                result
            );
        }
    }

    #[tokio::test]
    async fn third_call_injects_repeat_guard() {
        let (invocation, agent_id, handles, _log, _) = setup();
        let input = serde_json::json!({"message": "same"});
        let _ = invocation
            .invoke(agent_id, "echo", input.clone(), &handles)
            .await;
        let _ = invocation
            .invoke(agent_id, "echo", input.clone(), &handles)
            .await;
        let third = invocation
            .invoke(agent_id, "echo", input.clone(), &handles)
            .await
            .unwrap();
        let guard = third
            .get("_repeat_guard")
            .expect("third call must carry _repeat_guard");
        assert_eq!(guard["attempt_count"], 3);
        assert!(
            guard["hint"].as_str().unwrap().contains("echo"),
            "hint should name the tool; got {:?}",
            guard["hint"]
        );
    }

    #[tokio::test]
    async fn repeat_guard_emits_audit_event_on_third_call() {
        use aaos_core::AuditEventKind;

        let (invocation, agent_id, handles, log, _) = setup();
        let input = serde_json::json!({"message": "audit-test"});

        // Invoke the same tool with identical args 3 times
        for _ in 0..3 {
            let _ = invocation
                .invoke(agent_id, "echo", input.clone(), &handles)
                .await;
        }

        // Filter for ToolRepeatGuardFired events with tool=="echo" and attempt_count >= 3
        let repeat_events: Vec<_> = log
            .events()
            .into_iter()
            .filter(|e| {
                matches!(
                    &e.event,
                    AuditEventKind::ToolRepeatGuardFired {
                        tool,
                        attempt_count,
                        ..
                    } if tool == "echo" && *attempt_count >= 3
                )
            })
            .collect();

        assert!(
            !repeat_events.is_empty(),
            "expected at least one ToolRepeatGuardFired event at attempt 3+"
        );
    }

    #[tokio::test]
    async fn different_input_hash_resets_counter() {
        let (invocation, agent_id, handles, _log, _) = setup();
        let a = serde_json::json!({"message": "a"});
        let b = serde_json::json!({"message": "b"});
        let _ = invocation
            .invoke(agent_id, "echo", a.clone(), &handles)
            .await;
        let _ = invocation
            .invoke(agent_id, "echo", a.clone(), &handles)
            .await;
        let _ = invocation
            .invoke(agent_id, "echo", b.clone(), &handles)
            .await;
        assert_eq!(invocation.test_repeat_count(agent_id, "echo", &a), 2);
        assert_eq!(invocation.test_repeat_count(agent_id, "echo", &b), 1);
    }

    #[tokio::test]
    async fn different_agent_resets_counter() {
        let (invocation, agent_a, handles_a, _log, cap_registry) = setup();
        let input = serde_json::json!({"message": "x"});
        // Run agent_a three times so it trips the guard.
        for _ in 0..3 {
            let _ = invocation
                .invoke(agent_a, "echo", input.clone(), &handles_a)
                .await;
        }
        // Agent B: fresh agent, fresh grant, fresh counter.
        let agent_b = AgentId::new();
        let token_b = CapabilityToken::issue(
            agent_b,
            Capability::ToolInvoke {
                tool_name: "echo".into(),
            },
            Constraints::default(),
        );
        let handle_b = cap_registry.insert(agent_b, token_b);
        assert_eq!(invocation.test_repeat_count(agent_b, "echo", &input), 0);
        let first_for_b = invocation
            .invoke(agent_b, "echo", input.clone(), &[handle_b])
            .await
            .unwrap();
        assert!(
            first_for_b.get("_repeat_guard").is_none(),
            "agent B's first call must not see agent A's count"
        );
        assert_eq!(invocation.test_repeat_count(agent_b, "echo", &input), 1);
    }

    #[tokio::test]
    async fn repeat_counts_map_is_bounded() {
        // Hammer the invocation with 2000 distinct inputs and confirm the
        // map never exceeds the REPEAT_COUNTS_MAX cap of 1024.
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(EchoTool));
        let audit_log: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let cap_registry = Arc::new(CapabilityRegistry::new());
        let invocation = ToolInvocation::new(registry, audit_log, cap_registry.clone());

        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "echo".into(),
            },
            Constraints::default(),
        );
        let handle = cap_registry.insert(agent_id, token);

        for i in 0..2000u32 {
            let input = serde_json::json!({ "message": format!("msg-{i}") });
            let _ = invocation
                .invoke(agent_id, "echo", input, &[handle])
                .await
                .unwrap();
        }

        let len = invocation.test_repeat_counts_len();
        assert!(len <= 1024, "map size {len} exceeded cap of 1024");
        // Eviction in quarters means the map can dip as low as 3/4 of cap
        // right after a cleanup; it should be populated, not empty.
        assert!(len > 0, "map should not be empty after 2000 calls");
    }

    // ---- WorkerHandle routing tests (T7) ----

    /// Mock WorkerHandle that records every invoke_over_worker call and
    /// returns `{"ok": true}`. backend_kind is configurable.
    struct MockWorkerHandle {
        kind: &'static str,
        invocations: Arc<Mutex<Vec<(AgentId, String, serde_json::Value)>>>,
        /// Received token lists per call, for assertions in
        /// `worker_handle_receives_forwarded_tokens`.
        received_tokens: Arc<Mutex<Vec<Vec<aaos_core::CapabilityToken>>>>,
    }

    #[async_trait::async_trait]
    impl WorkerHandle for MockWorkerHandle {
        fn backend_kind(&self) -> &'static str {
            self.kind
        }

        async fn invoke_over_worker(
            &self,
            agent_id: AgentId,
            tool_name: &str,
            input: serde_json::Value,
            tokens: Vec<aaos_core::CapabilityToken>,
        ) -> std::result::Result<serde_json::Value, WorkerInvokeError> {
            self.invocations
                .lock()
                .unwrap()
                .push((agent_id, tool_name.to_string(), input));
            self.received_tokens.lock().unwrap().push(tokens);
            Ok(serde_json::json!({"ok": true}))
        }
    }

    fn setup_worker_harness(
        backend_kind: &'static str,
    ) -> (
        ToolInvocation,
        AgentId,
        Vec<CapabilityHandle>,
        Arc<InMemoryAuditLog>,
        Arc<Mutex<Vec<(AgentId, String, serde_json::Value)>>>,
    ) {
        let (invocation, agent_id, handles, log, invocations, _received_tokens) =
            setup_worker_harness_full(backend_kind);
        (invocation, agent_id, handles, log, invocations)
    }

    /// Extended harness that also returns the received_tokens arc for
    /// assertions in token-forwarding tests.
    fn setup_worker_harness_full(
        backend_kind: &'static str,
    ) -> (
        ToolInvocation,
        AgentId,
        Vec<CapabilityHandle>,
        Arc<InMemoryAuditLog>,
        Arc<Mutex<Vec<(AgentId, String, serde_json::Value)>>>,
        Arc<Mutex<Vec<Vec<aaos_core::CapabilityToken>>>>,
    ) {
        let registry = Arc::new(ToolRegistry::new());
        // Register EchoTool as "file_write" and "web_fetch" so daemon-side
        // lookups don't fail with ToolNotFound when routing goes Daemon.
        registry.register_as(Arc::new(EchoTool), "file_write");
        registry.register_as(Arc::new(EchoTool), "web_fetch");

        let log = Arc::new(InMemoryAuditLog::new());
        let cap_registry = Arc::new(CapabilityRegistry::new());
        let agent_id = AgentId::new();

        // Grant wildcard capability so both tool names pass the check.
        let token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "*".into(),
            },
            Constraints::default(),
        );
        let handle = cap_registry.insert(agent_id, token);

        let invocations: Arc<Mutex<Vec<(AgentId, String, serde_json::Value)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let received_tokens: Arc<Mutex<Vec<Vec<aaos_core::CapabilityToken>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let mock = Arc::new(MockWorkerHandle {
            kind: backend_kind,
            invocations: invocations.clone(),
            received_tokens: received_tokens.clone(),
        });

        let invocation =
            ToolInvocation::new_with_worker_handle(registry, log.clone(), cap_registry, mock);

        (
            invocation,
            agent_id,
            vec![handle],
            log,
            invocations,
            received_tokens,
        )
    }

    #[tokio::test]
    async fn namespaced_routes_file_write_to_worker() {
        let (invocation, agent_id, handles, log, mock_invocations) =
            setup_worker_harness("namespaced");

        let result = invocation
            .invoke(
                agent_id,
                "file_write",
                serde_json::json!({"path": "/tmp/x", "content": "hi"}),
                &handles,
            )
            .await
            .unwrap();

        // Mock returned {"ok": true}
        assert_eq!(result, serde_json::json!({"ok": true}));

        // Mock captured the call
        let calls = mock_invocations.lock().unwrap();
        assert_eq!(calls.len(), 1, "mock should have received exactly one call");
        assert_eq!(calls[0].1, "file_write");

        // Audit event carries surface = Worker
        let events = log.events();
        let surface_in_audit = events.iter().find_map(|e| match &e.event {
            AuditEventKind::ToolInvoked {
                tool,
                execution_surface,
                ..
            } if tool == "file_write" => Some(*execution_surface),
            _ => None,
        });
        assert_eq!(
            surface_in_audit,
            Some(ToolExecutionSurface::Worker),
            "ToolInvoked audit event must carry execution_surface = Worker"
        );
    }

    #[tokio::test]
    async fn namespaced_keeps_web_fetch_on_daemon() {
        let (invocation, agent_id, handles, log, mock_invocations) =
            setup_worker_harness("namespaced");

        // web_fetch is in DAEMON_SIDE_TOOLS — even with namespaced backend,
        // route_for returns Daemon and the mock must NOT be called.
        let _result = invocation
            .invoke(
                agent_id,
                "web_fetch",
                serde_json::json!({"url": "https://example.com"}),
                &handles,
            )
            .await;

        // Mock must NOT have been called
        let calls = mock_invocations.lock().unwrap();
        assert!(
            calls.is_empty(),
            "mock must not be called for web_fetch (daemon-side tool)"
        );

        // Audit event carries surface = Daemon
        let events = log.events();
        let surface_in_audit = events.iter().find_map(|e| match &e.event {
            AuditEventKind::ToolInvoked {
                tool,
                execution_surface,
                ..
            } if tool == "web_fetch" => Some(*execution_surface),
            _ => None,
        });
        assert_eq!(
            surface_in_audit,
            Some(ToolExecutionSurface::Daemon),
            "ToolInvoked audit event must carry execution_surface = Daemon for daemon-side tools"
        );
    }

    /// When the worker handle returns `NoSession` (agent spawned via an
    /// inline path with no broker session), ToolInvocation must fall back
    /// to daemon-side execution AND the audit event must carry
    /// `execution_surface: Daemon` so operators see the honest truth.
    #[tokio::test]
    async fn worker_no_session_falls_back_to_daemon() {
        struct NoSessionHandle;

        #[async_trait::async_trait]
        impl WorkerHandle for NoSessionHandle {
            fn backend_kind(&self) -> &'static str {
                "namespaced"
            }
            async fn invoke_over_worker(
                &self,
                _agent_id: AgentId,
                _tool_name: &str,
                _input: serde_json::Value,
                _tokens: Vec<aaos_core::CapabilityToken>,
            ) -> std::result::Result<serde_json::Value, WorkerInvokeError> {
                Err(WorkerInvokeError::NoSession)
            }
        }

        let log = Arc::new(InMemoryAuditLog::new());
        let cap_registry = Arc::new(CapabilityRegistry::new());
        let registry = Arc::new(ToolRegistry::new());
        registry.register_as(Arc::new(crate::tool::EchoTool), "file_write");

        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "*".into(),
            },
            Constraints::default(),
        );
        let handle = cap_registry.insert(agent_id, token);

        let invocation = ToolInvocation::new_with_worker_handle(
            registry,
            log.clone(),
            cap_registry,
            Arc::new(NoSessionHandle),
        );

        let result = invocation
            .invoke(
                agent_id,
                "file_write",
                serde_json::json!({"message": "hi"}),
                &[handle],
            )
            .await
            .expect("NoSession must fall back, not fail");
        assert_eq!(result, serde_json::json!({"message": "hi"}));

        let events = log.events();
        let surface_in_audit = events.iter().find_map(|e| match &e.event {
            AuditEventKind::ToolInvoked {
                tool,
                execution_surface,
                ..
            } if tool == "file_write" => Some(*execution_surface),
            _ => None,
        });
        assert_eq!(
            surface_in_audit,
            Some(ToolExecutionSurface::Daemon),
            "NoSession fallback must record actual surface = Daemon in audit"
        );
    }

    #[tokio::test]
    async fn no_worker_handle_stays_on_daemon() {
        // Construct a ToolInvocation WITHOUT a worker_handle (today's default
        // build). Every tool call must route daemon-side — no route_for
        // consultation, no surface fork.
        let log = Arc::new(InMemoryAuditLog::new());
        let cap_registry = Arc::new(CapabilityRegistry::new());
        let registry = Arc::new(ToolRegistry::new());
        // Register EchoTool under the file_write name so the daemon path
        // has something concrete to call.
        registry.register_as(Arc::new(crate::tool::EchoTool), "file_write");

        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "*".into(),
            },
            Constraints::default(),
        );
        let handle = cap_registry.insert(agent_id, token);

        let invocation = ToolInvocation::new(registry, log.clone(), cap_registry);
        let _ = invocation
            .invoke(
                agent_id,
                "file_write",
                serde_json::json!({"path": "/tmp/x"}),
                &[handle],
            )
            .await
            .unwrap();

        let events = log.events();
        let surface_in_audit = events.iter().find_map(|e| match &e.event {
            AuditEventKind::ToolInvoked {
                tool,
                execution_surface,
                ..
            } if tool == "file_write" => Some(*execution_surface),
            _ => None,
        });
        assert_eq!(
            surface_in_audit,
            Some(ToolExecutionSurface::Daemon),
            "no worker_handle must always route daemon-side"
        );
    }

    #[tokio::test]
    async fn in_process_handle_routes_everything_daemon_side() {
        // Even with a WorkerHandle present, if backend_kind = "in_process"
        // then route_for returns Daemon for every tool — the handle must
        // NOT be called. This guards against a regression where the
        // handle's mere presence forced worker-side routing.
        let (invocation, agent_id, handles, log, mock_invocations) =
            setup_worker_harness("in_process");

        let _ = invocation
            .invoke(
                agent_id,
                "file_write",
                serde_json::json!({"path": "/tmp/x"}),
                &handles,
            )
            .await;

        let calls = mock_invocations.lock().unwrap();
        assert!(
            calls.is_empty(),
            "mock must not be called when backend_kind=in_process"
        );

        let events = log.events();
        let surface_in_audit = events.iter().find_map(|e| match &e.event {
            AuditEventKind::ToolInvoked {
                tool,
                execution_surface,
                ..
            } if tool == "file_write" => Some(*execution_surface),
            _ => None,
        });
        assert_eq!(
            surface_in_audit,
            Some(ToolExecutionSurface::Daemon),
            "in_process backend must route every tool daemon-side"
        );
    }

    /// Verify that when a worker-routed tool call is made, the mock
    /// handle receives a non-empty token list containing the agent's
    /// resolved `CapabilityToken` structs. This pins the token-forwarding
    /// behaviour: the daemon resolves handles → tokens before the send so
    /// the worker can rebuild its per-call registry.
    #[tokio::test]
    async fn worker_handle_receives_forwarded_tokens() {
        let (invocation, agent_id, handles, _log, _invocations, received_tokens) =
            setup_worker_harness_full("namespaced");

        let _ = invocation
            .invoke(
                agent_id,
                "file_write",
                serde_json::json!({"path": "/tmp/x", "content": "hi"}),
                &handles,
            )
            .await
            .unwrap();

        let calls = received_tokens.lock().unwrap();
        assert_eq!(calls.len(), 1, "mock must have received exactly one call");
        let tokens_for_call = &calls[0];
        assert!(
            !tokens_for_call.is_empty(),
            "worker must receive a non-empty token list; got empty"
        );
        // The forwarded token should be the ToolInvoke:* grant we issued.
        let has_tool_invoke = tokens_for_call.iter().any(|t| {
            matches!(&t.capability, aaos_core::Capability::ToolInvoke { tool_name } if tool_name == "*")
        });
        assert!(
            has_tool_invoke,
            "forwarded tokens must include the ToolInvoke:* grant; got {:?}",
            tokens_for_call
                .iter()
                .map(|t| &t.capability)
                .collect::<Vec<_>>()
        );
    }

    // ---- Bug 10: max_invocations enforced through invoke ----

    #[tokio::test]
    async fn max_invocations_enforced_through_invoke() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(EchoTool));
        let log = Arc::new(InMemoryAuditLog::new());
        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "echo".into(),
            },
            Constraints {
                max_invocations: Some(3),
                rate_limit: None,
            },
        );
        let cap_registry = Arc::new(CapabilityRegistry::new());
        let handle = cap_registry.insert(agent_id, token);
        let invocation = ToolInvocation::new(registry, log.clone(), cap_registry.clone());
        let handles = vec![handle];

        // First three invocations must succeed.
        for i in 0..3 {
            let result = invocation
                .invoke(
                    agent_id,
                    "echo",
                    serde_json::json!({"message": format!("call-{i}")}),
                    &handles,
                )
                .await;
            assert!(
                result.is_ok(),
                "invocation {i} should succeed; got {:?}",
                result
            );
        }

        // Fourth invocation must fail with CapabilityDenied.
        let fourth = invocation
            .invoke(
                agent_id,
                "echo",
                serde_json::json!({"message": "call-3"}),
                &handles,
            )
            .await;
        assert!(
            matches!(fourth, Err(CoreError::CapabilityDenied { .. })),
            "4th invocation must be denied; got {:?}",
            fourth
        );
    }

    // ---- Bug 11: resolve_tokens filters revoked tokens ----

    #[test]
    fn resolve_tokens_filters_revoked() {
        let agent_id = AgentId::new();
        let cap_registry = Arc::new(CapabilityRegistry::new());

        // Issue two tokens.
        let live_token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "echo".into(),
            },
            Constraints::default(),
        );
        let revoked_token = CapabilityToken::issue(
            agent_id,
            Capability::ToolInvoke {
                tool_name: "echo".into(),
            },
            Constraints::default(),
        );
        let revoked_token_id = revoked_token.id;

        let live_handle = cap_registry.insert(agent_id, live_token);
        let revoked_handle = cap_registry.insert(agent_id, revoked_token);

        // Revoke the second token.
        assert!(cap_registry.revoke(revoked_token_id));

        // resolve_tokens must skip the revoked one.
        let resolved = cap_registry.resolve_tokens(&[live_handle, revoked_handle], agent_id);
        assert_eq!(
            resolved.len(),
            1,
            "resolve_tokens must return only the live token; got {} tokens",
            resolved.len()
        );
        assert!(
            !resolved[0].is_revoked(),
            "returned token must not be revoked"
        );
    }
}
