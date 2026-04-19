//! Batch version of SpawnAgentTool. Spawns up to N children concurrently
//! and collects their results.
//!
//! Design (see `docs/reflection/2026-04-14-run-11-prep-docs-masking-parallelism.md`):
//! - **Best-effort batch semantics.** Preflight reserves N slots in the
//!   registry's active_count atomically. If fewer than N slots are available,
//!   the whole call errors without spawning anything. Once all slots are
//!   reserved, each child's spawn and execution is independent: any per-child
//!   failure is returned as an error entry in the result array; siblings run
//!   to completion regardless.
//! - **Per-child cleanup via delegation.** Each child is run by delegating to
//!   `SpawnAgentTool::invoke`, which already owns the scopeguard that calls
//!   `stop_sync` on the child_id on Drop. That means panics, errors, and
//!   normal completion all funnel through `remove_agent`, releasing the
//!   `active_count` slot.
//! - **Panic policy.** If a tokio task itself panics (programming bug in our
//!   code), the batch returns an error but the JoinSet is drained so
//!   non-panicking children's scopeguards run.

use std::sync::Arc;

use aaos_core::{CoreError, Result, ToolDefinition};
use aaos_runtime::AgentRegistry;
use aaos_tools::{InvocationContext, Tool};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::spawn_tool::SpawnAgentTool;

/// Maximum children per `spawn_agents` call. Configurable via
/// `AAOS_SPAWN_AGENTS_BATCH_CAP` env var (default 3). This is a **per-batch
/// cap**, not a global concurrency cap — if Bootstrap were replaced by
/// multiple parallel callers, each could issue its own batch independently.
/// Today Bootstrap is the sole caller, so per-batch == effective global.
fn batch_cap() -> usize {
    std::env::var("AAOS_SPAWN_AGENTS_BATCH_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3usize)
}

pub struct SpawnAgentsTool {
    /// Delegate single-child spawn to the existing tool so validation,
    /// narrowing, cleanup (scopeguard), and retry logic stay in one place.
    single: Arc<SpawnAgentTool>,
    /// Used for preflight `active_count` slot reservation.
    registry: Arc<AgentRegistry>,
}

impl SpawnAgentsTool {
    pub fn new(single: Arc<SpawnAgentTool>, registry: Arc<AgentRegistry>) -> Self {
        Self { single, registry }
    }
}

#[async_trait]
impl Tool for SpawnAgentsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agents".to_string(),
            description: format!(
                "Spawn up to {cap} independent child agents concurrently in a single call. \
                 Each child is specified as {{manifest, message, prior_findings?}} — same \
                 shape as `spawn_agent`. \
                 Best-effort semantics: preflight all-or-nothing for slot reservation (if \
                 there aren't enough slots, no children spawn); per-child execution is \
                 independent (one child's error does not abort siblings); results are \
                 returned as an array of {{agent_id, response, usage, iterations, stop_reason, error}} \
                 entries indexed to match the input order. \
                 Use `spawn_agents` for independent subtasks (e.g., scanning different crates \
                 in parallel). Use `spawn_agent` when a child's output feeds the next via \
                 `prior_findings`. \
                 Note: a task-level panic (programming bug) aborts the batch with an error; \
                 non-panicking children still clean up via their own stop paths.",
                cap = batch_cap()
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "children": {
                        "type": "array",
                        "description": "Up to AAOS_SPAWN_AGENTS_BATCH_CAP child specs to spawn in parallel",
                        "items": {
                            "type": "object",
                            "properties": {
                                "manifest": { "type": "string", "description": "YAML manifest for the child agent" },
                                "message": { "type": "string", "description": "Child's goal" },
                                "prior_findings": {
                                    "type": "string",
                                    "description": "Optional prior findings to pass as kernel-framed context"
                                }
                            },
                            "required": ["manifest", "message"]
                        }
                    }
                },
                "required": ["children"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let cap = batch_cap();

        let children = input
            .get("children")
            .and_then(|v| v.as_array())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'children' array".into()))?
            .clone();

        if children.is_empty() {
            return Err(CoreError::InvalidManifest(
                "'children' must have at least one entry".into(),
            ));
        }
        if children.len() > cap {
            return Err(CoreError::InvalidManifest(format!(
                "'children' has {} entries but AAOS_SPAWN_AGENTS_BATCH_CAP is {}",
                children.len(),
                cap
            )));
        }

        // === PREFLIGHT: fast-fail check, not reservation ===
        // Delegated SpawnAgentTool::invoke calls spawn_with_tokens, which
        // reserves its own slot. Reserving slots here too would double-count.
        // So preflight is a fast-fail snapshot: if the registry is already
        // too full to fit `children.len()` more, error now without spawning
        // anything. This is NOT atomic against concurrent spawns — by the
        // time we fan out, the snapshot may be stale — but it rejects the
        // obvious over-limit case and makes the tool honest about
        // best-effort semantics past this point.
        let snapshot_count = self.registry.active_count();
        let snapshot_limit = self.registry.max_agents();
        if snapshot_count + children.len() > snapshot_limit {
            return Err(CoreError::InvalidManifest(format!(
                "spawn_agents: {} requested, registry has {}/{} agents — over limit",
                children.len(),
                snapshot_count,
                snapshot_limit
            )));
        }

        // === FAN OUT ===
        // Each task delegates to SpawnAgentTool::invoke. That call owns its
        // own slot reservation (via spawn_with_tokens → reserve_agent_slot)
        // AND its own scopeguard (stop_sync on Drop). Slot release and child
        // cleanup happen through the single-child tool's paths — this batch
        // tool does not directly touch the registry's admission state.
        let mut set = tokio::task::JoinSet::new();
        for (idx, child_input) in children.into_iter().enumerate() {
            let single = self.single.clone();
            let ctx_agent_id = ctx.agent_id;
            let ctx_tokens = ctx.tokens.clone();
            let ctx_cap_registry = ctx.capability_registry.clone();

            set.spawn(async move {
                // Translate the batch-item shape into the single-child input shape.
                let single_input = Value::Object({
                    let mut m = serde_json::Map::new();
                    if let Some(manifest) = child_input.get("manifest") {
                        m.insert("manifest".to_string(), manifest.clone());
                    }
                    if let Some(message) = child_input.get("message") {
                        m.insert("message".to_string(), message.clone());
                    }
                    if let Some(prior) = child_input.get("prior_findings") {
                        m.insert("prior_findings".to_string(), prior.clone());
                    }
                    m
                });

                let child_ctx = InvocationContext {
                    agent_id: ctx_agent_id,
                    tokens: ctx_tokens,
                    capability_registry: ctx_cap_registry,
                };

                let result = single.invoke(single_input, &child_ctx).await;

                // No slot to release here — SpawnAgentTool's internal spawn
                // path owns slot lifecycle via spawn_with_tokens + scopeguard.

                let result_value = match result {
                    Ok(v) => v,
                    Err(e) => json!({
                        "agent_id": null,
                        "response": null,
                        "error": e.to_string(),
                    }),
                };

                (idx, result_value)
            });
        }

        // === COLLECT ===
        // Drain the JoinSet. Panics surface as a batch-level error but do
        // not abort remaining tasks — each task's scopeguard (inside
        // SpawnAgentTool) still runs on Drop for clean child removal.
        let mut indexed: Vec<(usize, Value)> = Vec::with_capacity(set.len());
        let mut panic_err: Option<CoreError> = None;

        while let Some(join) = set.join_next().await {
            match join {
                Ok((idx, v)) => indexed.push((idx, v)),
                Err(e) => {
                    if panic_err.is_none() {
                        panic_err =
                            Some(CoreError::Ipc(format!("spawn_agents task panicked: {e}")));
                    }
                }
            }
        }

        if let Some(e) = panic_err {
            return Err(e);
        }

        indexed.sort_by_key(|(i, _)| *i);
        let children_results: Vec<Value> = indexed.into_iter().map(|(_, v)| v).collect();

        Ok(json!({
            "count": children_results.len(),
            "children": children_results,
        }))
    }
}
