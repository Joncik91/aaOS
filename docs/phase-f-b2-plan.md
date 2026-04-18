# Dynamic Model Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single pinned `role.model` with a per-role model ladder that the plan executor walks on three observable escalation signals (replan-after-failure, tool-repeat-guard-fired, `MaxTokens` stop reason), emitting a visible audit event on each tier bump.

**Architecture:** Add `Role.model_ladder: Vec<String>` + `Role.escalate_on: Vec<EscalationSignal>` (both optional, back-compat). Add `Subtask.current_model_tier: u8`. Track per-subtask failure context by extending `ExecutorError::Correctable` with a structured `FailedSubtask` list so the replan loop can decide escalation without re-querying audit. Thread the tier through `Role::render_manifest_with_model(model, params)` at spawn time. Ship `PerModelLatencyTracker` as a second `LatencyTracker` impl for observability (not consumed by routing in v1).

**Tech Stack:** Rust, `serde` / `serde_yaml` / `serde_json`, `dashmap` (for per-model sample rings), `tokio` audit broadcast, no new dependencies.

---

## File structure

**New files:**

- `crates/aaos-runtime/src/scheduler/per_model_latency.rs` — `PerModelLatencyTracker` + `ModelSampleRing`.
- `crates/aaos-runtime/src/plan/escalation.rs` — `EscalationSignal` enum + `decide_escalation()` + `FailedSubtask` struct + `carry_tiers_forward()` helper.

**Modified files:**

- `crates/aaos-core/src/audit.rs` — add `SubtaskModelEscalated` + `ToolRepeatGuardFired` variants.
- `crates/aaos-runtime/src/plan/role.rs` — add `model_ladder` + `escalate_on` fields + `resolved_ladder()` + `render_manifest_with_model()`. Catalog-load validation for ladder[0] == model.
- `crates/aaos-runtime/src/plan/mod.rs` — add `Subtask.current_model_tier` field.
- `crates/aaos-runtime/src/plan/executor.rs` — consume `subtask.current_model_tier` in `spawn_subtask`, collect `FailedSubtask`s into `Correctable` payload, run `decide_escalation` + `carry_tiers_forward` in the outer replan loop, emit `SubtaskModelEscalated` events.
- `crates/aaos-runtime/src/scheduler/mod.rs` — re-export `PerModelLatencyTracker`.
- `crates/aaos-runtime/src/scheduler/view.rs` — `SchedulerView::new` accepts the `PerModelLatencyTracker` + registers the subtask→model mapping before the first `record()`.
- `crates/aaos-tools/src/invocation.rs` — emit `ToolRepeatGuardFired` audit event at the existing repeat-guard firing site.
- `crates/agentd/src/server.rs` — build the `PerModelLatencyTracker` alongside `SubtaskWallClockTracker`, thread both into `SchedulerView`.
- `crates/agentd/src/cli/output.rs` — whitelist + formatter for `SubtaskModelEscalated` and `ToolRepeatGuardFired`.
- `packaging/roles/{writer,analyzer,generalist,fetcher}.yaml` — OPTIONAL: add `model_ladder` explicitly for the roles that benefit.

---

## Task 1: `EscalationSignal` enum

**Files:**
- Create: `crates/aaos-runtime/src/plan/escalation.rs`
- Modify: `crates/aaos-runtime/src/plan/mod.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/aaos-runtime/src/plan/escalation.rs`:

```rust
//! Escalation signal taxonomy for dynamic model routing.
//!
//! The plan executor watches for three observable signals during a subtask's
//! execution. When a subtask fails AND a configured signal fired, the
//! executor bumps the subtask's model tier for the next replan attempt.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationSignal {
    ReplanRetry,
    ToolRepeatGuard,
    MaxTokens,
}

impl EscalationSignal {
    /// Machine-readable reason string used in SubtaskModelEscalated audit
    /// events. Stable — downstream log consumers may match on these.
    pub fn reason(&self) -> &'static str {
        match self {
            EscalationSignal::ReplanRetry => "replan_retry",
            EscalationSignal::ToolRepeatGuard => "tool_repeat_guard",
            EscalationSignal::MaxTokens => "max_tokens",
        }
    }
}

pub fn default_escalation_signals() -> Vec<EscalationSignal> {
    vec![
        EscalationSignal::ReplanRetry,
        EscalationSignal::ToolRepeatGuard,
        EscalationSignal::MaxTokens,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_serde_roundtrip() {
        for s in [
            EscalationSignal::ReplanRetry,
            EscalationSignal::ToolRepeatGuard,
            EscalationSignal::MaxTokens,
        ] {
            let j = serde_json::to_string(&s).unwrap();
            let expected = format!("\"{}\"", s.reason());
            assert_eq!(j, expected, "serialize must use snake_case reason string");
            let back: EscalationSignal = serde_json::from_str(&j).unwrap();
            assert_eq!(back, s);
        }
    }

    #[test]
    fn unknown_signal_is_an_error() {
        let r: Result<EscalationSignal, _> = serde_json::from_str("\"ragequit\"");
        assert!(r.is_err(), "unknown signal strings must error, not default");
    }

    #[test]
    fn default_set_contains_all_three() {
        let defaults = default_escalation_signals();
        assert_eq!(defaults.len(), 3);
        assert!(defaults.contains(&EscalationSignal::ReplanRetry));
        assert!(defaults.contains(&EscalationSignal::ToolRepeatGuard));
        assert!(defaults.contains(&EscalationSignal::MaxTokens));
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/aaos-runtime/src/plan/mod.rs`, add near the other `pub mod ...` declarations:

```rust
pub mod escalation;
```

Also add a `pub use` alongside the existing ones (e.g. `pub use role::{...}`):

```rust
pub use escalation::{default_escalation_signals, EscalationSignal};
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p aaos-runtime --lib plan::escalation 2>&1 | tail -10`
Expected: `test result: ok. 3 passed; 0 failed`.

- [ ] **Step 4: Commit**

```bash
git add crates/aaos-runtime/src/plan/escalation.rs crates/aaos-runtime/src/plan/mod.rs
git commit -m "$(cat <<'EOF'
feat(runtime): EscalationSignal enum — replan_retry, tool_repeat_guard, max_tokens

Three escalation signals for dynamic model routing. Pure type; no
consumers yet. Follow-up tasks wire Role.escalate_on and the executor
decision path.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Two new audit variants

**Files:**
- Modify: `crates/aaos-core/src/audit.rs`

- [ ] **Step 1: Write the failing tests**

Find the existing `#[cfg(test)] mod tests` block in `crates/aaos-core/src/audit.rs` and add:

```rust
    #[test]
    fn subtask_model_escalated_variant_roundtrips() {
        let e = AuditEventKind::SubtaskModelEscalated {
            subtask_id: "s1".into(),
            from_tier: 0,
            to_tier: 1,
            from_model: "deepseek-chat".into(),
            to_model: "deepseek-reasoner".into(),
            reason: "replan_retry".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: AuditEventKind = serde_json::from_str(&s).unwrap();
        match back {
            AuditEventKind::SubtaskModelEscalated {
                subtask_id,
                from_tier,
                to_tier,
                from_model,
                to_model,
                reason,
            } => {
                assert_eq!(subtask_id, "s1");
                assert_eq!(from_tier, 0);
                assert_eq!(to_tier, 1);
                assert_eq!(from_model, "deepseek-chat");
                assert_eq!(to_model, "deepseek-reasoner");
                assert_eq!(reason, "replan_retry");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tool_repeat_guard_fired_variant_roundtrips() {
        use crate::AgentId;
        let e = AuditEventKind::ToolRepeatGuardFired {
            agent_id: AgentId::new(),
            tool: "web_fetch".into(),
            attempt_count: 3,
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: AuditEventKind = serde_json::from_str(&s).unwrap();
        match back {
            AuditEventKind::ToolRepeatGuardFired {
                tool,
                attempt_count,
                ..
            } => {
                assert_eq!(tool, "web_fetch");
                assert_eq!(attempt_count, 3);
            }
            _ => panic!("wrong variant"),
        }
    }
```

- [ ] **Step 2: Run to verify compile fail**

Run: `cargo test -p aaos-core subtask_model_escalated 2>&1 | tail -5`
Expected: compile error — variants don't exist yet.

- [ ] **Step 3: Add the variants**

In `crates/aaos-core/src/audit.rs`, find `pub enum AuditEventKind`. The existing `SubtaskTtlExpired` variant shows the shape to mirror. Add these two **right after** `SubtaskTtlExpired` and before the enum's closing `}`:

```rust
    SubtaskModelEscalated {
        subtask_id: String,
        from_tier: u8,
        to_tier: u8,
        from_model: String,
        to_model: String,
        /// Machine-readable reason: "replan_retry" | "tool_repeat_guard" | "max_tokens".
        reason: String,
    },
    ToolRepeatGuardFired {
        agent_id: crate::AgentId,
        tool: String,
        attempt_count: u32,
    },
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p aaos-core 2>&1 | tail -5`
Expected: all aaos-core tests pass — new ones plus no regressions.

Also verify workspace compiles: `cargo build --workspace 2>&1 | tail -3`. Any `match`-exhaustiveness warnings from the new variants? If so, address by extending existing wildcard arms. Do not add operator-visibility for `SubtaskModelEscalated` here; Task 10 does that.

- [ ] **Step 5: Commit**

```bash
git add crates/aaos-core/src/audit.rs
git commit -m "$(cat <<'EOF'
feat(core): SubtaskModelEscalated + ToolRepeatGuardFired audit variants

SubtaskModelEscalated fires when the executor bumps a subtask's model
tier on replan. ToolRepeatGuardFired fires when the existing
aaos-tools repeat-guard hint is injected (attempt_count >= threshold),
so the executor can detect the signal from the audit stream without
introspecting the tool-result JSON.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `Role.model_ladder` + `escalate_on` + `resolved_ladder()` + validation

**Files:**
- Modify: `crates/aaos-runtime/src/plan/role.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/aaos-runtime/src/plan/role.rs`, find the existing `#[cfg(test)] mod tests` block and add:

```rust
    #[test]
    fn resolved_ladder_missing_field_returns_single_element() {
        let yaml = r#"
name: r
model: deepseek-chat
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            role.resolved_ladder().unwrap(),
            vec!["deepseek-chat".to_string()]
        );
    }

    #[test]
    fn resolved_ladder_explicit_two_tier() {
        let yaml = r#"
name: r
model: deepseek-chat
model_ladder:
  - deepseek-chat
  - deepseek-reasoner
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            role.resolved_ladder().unwrap(),
            vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()]
        );
    }

    #[test]
    fn resolved_ladder_rejects_drift_between_model_and_first_tier() {
        let yaml = r#"
name: r
model: deepseek-chat
model_ladder:
  - deepseek-reasoner
  - claude-opus-4
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        let err = role.resolved_ladder().unwrap_err();
        assert!(
            err.contains("deepseek-chat") && err.contains("deepseek-reasoner"),
            "error must name both drifted values; got: {err}"
        );
    }

    #[test]
    fn escalate_on_defaults_to_all_three_signals() {
        use crate::plan::EscalationSignal;
        let yaml = r#"
name: r
model: deepseek-chat
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        assert!(role.escalate_on.contains(&EscalationSignal::ReplanRetry));
        assert!(role.escalate_on.contains(&EscalationSignal::ToolRepeatGuard));
        assert!(role.escalate_on.contains(&EscalationSignal::MaxTokens));
    }

    #[test]
    fn escalate_on_explicit_subset() {
        use crate::plan::EscalationSignal;
        let yaml = r#"
name: r
model: deepseek-chat
escalate_on:
  - max_tokens
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(role.escalate_on, vec![EscalationSignal::MaxTokens]);
    }
```

- [ ] **Step 2: Run to verify compile fail**

Run: `cargo test -p aaos-runtime --lib plan::role 2>&1 | tail -10`
Expected: compile errors — `Role.model_ladder`, `Role.escalate_on`, `Role::resolved_ladder` don't exist yet.

- [ ] **Step 3: Add the fields + helper**

In `crates/aaos-runtime/src/plan/role.rs`, find `pub struct Role`. Insert the new fields after `retry` and before the `scaffold` field:

```rust
    /// Ordered list of models to try for a subtask in this role. Tier 0 is
    /// `role.model`; escalation walks up the ladder on failure signals.
    /// Missing or empty = single-tier routing with `role.model`.
    #[serde(default)]
    pub model_ladder: Vec<String>,

    /// Which observable signals trigger escalation to the next tier on
    /// replan. Missing = all three signals active (see
    /// `EscalationSignal::default_escalation_signals`).
    #[serde(default = "crate::plan::escalation::default_escalation_signals")]
    pub escalate_on: Vec<crate::plan::EscalationSignal>,
```

At the bottom of `impl Role`, add:

```rust
    /// Canonical model ladder. Substitutes `[role.model]` when
    /// `model_ladder` is empty; validates `model_ladder[0] == role.model`
    /// when non-empty. Returns a human-readable error on drift; the
    /// catalog loader surfaces these at startup.
    pub fn resolved_ladder(&self) -> Result<Vec<String>, String> {
        if self.model_ladder.is_empty() {
            return Ok(vec![self.model.clone()]);
        }
        if self.model_ladder[0] != self.model {
            return Err(format!(
                "role '{}': model_ladder[0] = '{}' but model = '{}' — the two must match (model is the display/back-compat field; the ladder drives routing)",
                self.name, self.model_ladder[0], self.model
            ));
        }
        Ok(self.model_ladder.clone())
    }

    /// Render a manifest targeting a specific model. Used by the executor
    /// when `subtask.current_model_tier > 0` so the tier-bumped subtask
    /// gets a manifest naming the escalated model instead of the tier-0
    /// default baked into `render_manifest`.
    pub fn render_manifest_with_model(&self, model: &str, params: &serde_json::Value) -> String {
        let caps: Vec<String> = self
            .capabilities
            .iter()
            .flat_map(|c| expand_capability(c, params))
            .collect();
        let caps_yaml: String = caps
            .iter()
            .map(|c| format!("  - \"{}\"\n", c.replace('"', "\\\"")))
            .collect();
        format!(
            "name: {name}\nmodel: {model}\nsystem_prompt: |\n{prompt}\ncapabilities:\n{caps}",
            name = self.name,
            model = model,
            prompt = indent(&self.system_prompt, "  "),
            caps = caps_yaml,
        )
    }
```

Update the existing `render_manifest` to delegate:

```rust
    pub fn render_manifest(&self, params: &serde_json::Value) -> String {
        self.render_manifest_with_model(&self.model, params)
    }
```

- [ ] **Step 4: Fix existing `Role { ... }` literal constructors**

Adding new fields with `#[serde(default ...)]` keeps YAML back-compat but literal constructors in Rust tests break. Run: `cargo build --workspace 2>&1 | grep "missing field" | head -10`

Expected: hits in `plan/role.rs` tests, `plan/executor.rs` tests, possibly `plan/planner.rs` tests, `crates/agentd/src/server.rs` tests. For each, append:

```rust
            model_ladder: vec![],
            escalate_on: default_escalation_signals(),
```

to the constructor. Import `default_escalation_signals` where needed:

```rust
use crate::plan::escalation::default_escalation_signals;
```

Iterate until `cargo build --workspace` is clean.

- [ ] **Step 5: Add catalog-load validation**

Find `RoleCatalog::load_from_dir`. It currently returns the first parse error. Extend to also call `resolved_ladder()` on every loaded role and surface validation errors:

```rust
    pub fn load_from_dir(dir: &Path) -> Result<Self, RoleCatalogError> {
        // ... existing code that populates the HashMap ...

        // Validate model_ladder / model consistency. Cheap walk after
        // parsing, surfaces operator-visible drift at startup rather
        // than at first spawn.
        for role in roles.values() {
            role.resolved_ladder()
                .map_err(RoleCatalogError::Parse)?;
        }

        Ok(Self { roles })
    }
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p aaos-runtime --lib plan 2>&1 | tail -10`
Expected: all new tests pass + no regressions.

Run: `cargo test --workspace 2>&1 | grep "test result" | grep -v "ok" | head`
Expected: empty.

- [ ] **Step 7: Commit**

```bash
git add crates/aaos-runtime/src/plan/role.rs crates/
git commit -m "$(cat <<'EOF'
feat(runtime): Role.model_ladder + escalate_on + resolved_ladder validation

Optional YAML fields, back-compat for every existing role. Invariant
model_ladder[0] == role.model enforced at catalog-load (drift is
immediately operator-visible, not a runtime spawn error).
render_manifest_with_model() added so the executor can inject the
escalated tier's model into the subtask manifest.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `Subtask.current_model_tier`

**Files:**
- Modify: `crates/aaos-runtime/src/plan/mod.rs`

- [ ] **Step 1: Write the failing test**

Inside the existing `#[cfg(test)] mod tests` block in `crates/aaos-runtime/src/plan/mod.rs`, add:

```rust
    #[test]
    fn subtask_current_model_tier_defaults_to_zero_and_roundtrips() {
        // Missing field defaults to 0 — back-compat for serialized plans.
        let s: Subtask = serde_json::from_str(
            r#"{"id":"a","role":"writer","params":{},"depends_on":[]}"#,
        )
        .unwrap();
        assert_eq!(s.current_model_tier, 0);

        // Explicit value round-trips.
        let s2 = Subtask {
            id: "b".into(),
            role: "writer".into(),
            params: serde_json::json!({}),
            depends_on: vec![],
            ttl: None,
            current_model_tier: 2,
        };
        let json = serde_json::to_string(&s2).unwrap();
        let back: Subtask = serde_json::from_str(&json).unwrap();
        assert_eq!(s2, back);
        assert!(json.contains("\"current_model_tier\":2"), "explicit tier must serialize: {json}");
    }
```

- [ ] **Step 2: Run to verify compile fail**

Run: `cargo test -p aaos-runtime --lib plan::tests::subtask_current_model_tier 2>&1 | tail -5`
Expected: compile error — field doesn't exist.

- [ ] **Step 3: Add the field**

In `crates/aaos-runtime/src/plan/mod.rs`, find `pub struct Subtask`. Add a field after `ttl`:

```rust
    /// Ladder index for model routing (Phase F-b sub-project 2). Planner
    /// sets 0; executor increments on replan when an escalation signal
    /// fired. Back-compat: missing deserializes to 0.
    #[serde(default)]
    pub current_model_tier: u8,
```

- [ ] **Step 4: Fix existing `Subtask { ... }` literal constructors**

Run: `cargo build --workspace 2>&1 | grep "missing field" | head -20`

Expected: many hits across executor.rs tests, planner.rs tests, agentd/src/server.rs tests. For each, append:

```rust
            current_model_tier: 0,
```

Iterate until the build is clean.

- [ ] **Step 5: Run tests**

Run: `cargo test -p aaos-runtime --lib plan 2>&1 | tail -5` — all pass.

Run: `cargo test --workspace 2>&1 | grep "test result" | grep -v "ok" | head` — empty.

- [ ] **Step 6: Commit**

```bash
git add crates/
git commit -m "$(cat <<'EOF'
feat(runtime): Subtask.current_model_tier field

Per-subtask ladder index; planner sets 0, executor increments on
replan. Back-compat serde default keeps existing plan.json files
deserializable unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `PerModelLatencyTracker`

**Files:**
- Create: `crates/aaos-runtime/src/scheduler/per_model_latency.rs`
- Modify: `crates/aaos-runtime/src/scheduler/mod.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/aaos-runtime/src/scheduler/per_model_latency.rs`:

```rust
//! `PerModelLatencyTracker` — a second `LatencyTracker` impl keyed by
//! model name. Maintains a bounded ring of recent samples per model and
//! exposes p50/p95 queries for future cost-aware routing. Not consumed
//! by routing in Phase F-b sub-project 2; this is observability infra.

use std::sync::Mutex;
use std::time::Duration;

use dashmap::DashMap;

use super::LatencyTracker;

/// Ring buffer of recent durations, fixed capacity 256.
#[derive(Debug)]
pub struct ModelSampleRing {
    samples: Vec<Duration>,
    cap: usize,
    next: usize,
    full: bool,
}

impl ModelSampleRing {
    const CAPACITY: usize = 256;

    pub fn new() -> Self {
        Self {
            samples: Vec::with_capacity(Self::CAPACITY),
            cap: Self::CAPACITY,
            next: 0,
            full: false,
        }
    }

    pub fn push(&mut self, d: Duration) {
        if !self.full {
            self.samples.push(d);
            if self.samples.len() == self.cap {
                self.full = true;
                self.next = 0;
            }
        } else {
            self.samples[self.next] = d;
            self.next = (self.next + 1) % self.cap;
        }
    }

    pub fn percentile(&self, p: f64) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<Duration> = self.samples.clone();
        sorted.sort();
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        Some(sorted[idx.min(sorted.len() - 1)])
    }
}

impl Default for ModelSampleRing {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
pub struct PerModelLatencyTracker {
    /// subtask_id → model name. Populated on SchedulerView::new so the
    /// trait's `record(subtask_id, elapsed)` can route to the right model.
    subtask_models: DashMap<String, String>,
    samples: DashMap<String, Mutex<ModelSampleRing>>,
}

impl PerModelLatencyTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the subtask→model binding before any `record` call for
    /// this subtask. Idempotent; re-registration overwrites (the executor
    /// only re-registers when a subtask's tier changes).
    pub fn register(&self, subtask_id: &str, model: &str) {
        self.subtask_models
            .insert(subtask_id.to_string(), model.to_string());
    }

    pub fn p50(&self, model: &str) -> Option<Duration> {
        self.samples
            .get(model)
            .and_then(|ring| ring.lock().ok()?.percentile(0.50))
    }

    pub fn p95(&self, model: &str) -> Option<Duration> {
        self.samples
            .get(model)
            .and_then(|ring| ring.lock().ok()?.percentile(0.95))
    }
}

impl LatencyTracker for PerModelLatencyTracker {
    fn record(&self, subtask_id: &str, elapsed: Duration) {
        let Some(model) = self.subtask_models.get(subtask_id).map(|m| m.clone()) else {
            // Unregistered subtask — silently drop. SubtaskWallClockTracker
            // still gets the sample via the composite tracker; p50/p95 just
            // won't have the data. Warn-level trace would be noisy for the
            // first-call-before-register race, which we don't actually have
            // because SchedulerView::new registers synchronously before any
            // complete() call.
            return;
        };
        self.samples
            .entry(model)
            .or_default()
            .lock()
            .map(|mut ring| ring.push(elapsed))
            .ok();
    }

    fn wall_clock_elapsed(&self, _subtask_id: &str) -> Duration {
        // Not tracked by this impl. Callers wanting per-subtask cumulative
        // time must use SubtaskWallClockTracker (see composite tracker
        // wiring in SchedulerView).
        Duration::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as D;

    #[test]
    fn ring_push_and_percentile_under_capacity() {
        let mut ring = ModelSampleRing::new();
        for ms in [100, 200, 300, 400, 500] {
            ring.push(D::from_millis(ms));
        }
        assert_eq!(ring.percentile(0.50), Some(D::from_millis(300)));
        assert_eq!(ring.percentile(0.95), Some(D::from_millis(500)));
    }

    #[test]
    fn ring_evicts_oldest_when_full() {
        let mut ring = ModelSampleRing::new();
        // Fill with 1ms samples, then push 100 x 1000ms.
        for _ in 0..ModelSampleRing::CAPACITY {
            ring.push(D::from_millis(1));
        }
        for _ in 0..100 {
            ring.push(D::from_millis(1000));
        }
        // p95 must reflect the recent samples, not the initial 1ms noise.
        let p95 = ring.percentile(0.95).unwrap();
        assert!(
            p95 >= D::from_millis(500),
            "eviction failed — p95 stuck at old samples: {p95:?}"
        );
    }

    #[test]
    fn unregistered_subtask_is_noop() {
        let tracker = PerModelLatencyTracker::new();
        tracker.record("ghost", D::from_millis(100));
        assert!(tracker.p50("any-model").is_none());
        assert!(tracker.p95("any-model").is_none());
    }

    #[test]
    fn records_by_model_after_register() {
        let tracker = PerModelLatencyTracker::new();
        tracker.register("a", "deepseek-chat");
        tracker.register("b", "deepseek-reasoner");
        tracker.record("a", D::from_millis(100));
        tracker.record("a", D::from_millis(300));
        tracker.record("b", D::from_millis(2000));

        let p50_chat = tracker.p50("deepseek-chat").unwrap();
        assert!(
            p50_chat >= D::from_millis(100) && p50_chat <= D::from_millis(300),
            "got {p50_chat:?}"
        );
        let p50_reasoner = tracker.p50("deepseek-reasoner").unwrap();
        assert_eq!(p50_reasoner, D::from_millis(2000));
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/aaos-runtime/src/scheduler/mod.rs`, add alongside the existing `pub mod latency;` line:

```rust
pub mod per_model_latency;
```

And add a re-export alongside the existing `pub use latency::{...}`:

```rust
pub use per_model_latency::{ModelSampleRing, PerModelLatencyTracker};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p aaos-runtime --lib scheduler::per_model_latency 2>&1 | tail -10`
Expected: `test result: ok. 4 passed`.

Workspace clean: `cargo test --workspace 2>&1 | grep "test result" | grep -v "ok" | head` — empty.

- [ ] **Step 4: Commit**

```bash
git add crates/aaos-runtime/src/scheduler/per_model_latency.rs crates/aaos-runtime/src/scheduler/mod.rs
git commit -m "$(cat <<'EOF'
feat(runtime): PerModelLatencyTracker — bounded per-model sample rings

Second LatencyTracker impl alongside SubtaskWallClockTracker. Holds
256 recent durations per model in a bounded ring, exposes p50/p95.
register(subtask_id, model) binds the subtask→model mapping so the
trait's record(subtask_id, elapsed) can route. Not consumed by
routing in v1 — observability infra for a future cost-aware sub-
project.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `ToolRepeatGuardFired` audit emit

**Files:**
- Modify: `crates/aaos-tools/src/invocation.rs`

- [ ] **Step 1: Write the failing test**

In `crates/aaos-tools/src/invocation.rs`, find the existing test `third_call_injects_repeat_guard`. Add a new sibling test in the same module:

```rust
    #[tokio::test]
    async fn repeat_guard_emits_audit_event_on_third_call() {
        use aaos_core::{AuditEventKind, InMemoryAuditLog};

        let audit = Arc::new(InMemoryAuditLog::new());
        let (registry, tool_registry) = fixture_with_echo();
        let invocation = ToolInvocation::new(
            tool_registry,
            audit.clone() as Arc<dyn aaos_core::AuditLog>,
            registry.capability_registry().clone(),
        );

        let agent_id = fixture_spawn_agent_with_echo(&registry);
        let args = serde_json::json!({"message": "same"});

        for _ in 0..3 {
            let _ = invocation
                .invoke(agent_id, "echo", args.clone())
                .await;
        }

        let repeat_events: Vec<_> = audit
            .events()
            .into_iter()
            .filter(|e| {
                matches!(
                    &e.event,
                    AuditEventKind::ToolRepeatGuardFired { tool, attempt_count, .. }
                        if tool == "echo" && *attempt_count >= 3
                )
            })
            .collect();

        assert!(
            !repeat_events.is_empty(),
            "expected at least one ToolRepeatGuardFired event at attempt 3+"
        );
    }
```

If `fixture_with_echo` / `fixture_spawn_agent_with_echo` don't exist with those names, use the exact pattern from the existing `third_call_injects_repeat_guard` test — read that test first and mirror its setup. The key additions are: the `Arc<InMemoryAuditLog>` (instead of a simple `Arc<dyn AuditLog>` so we can query events) and the `.events()` filter.

- [ ] **Step 2: Run to verify test fails**

Run: `cargo test -p aaos-tools repeat_guard_emits_audit 2>&1 | tail -10`
Expected: compile error on `ToolRepeatGuardFired`-not-a-variant (if Task 2 wasn't applied) OR a runtime fail because the audit event isn't emitted yet.

- [ ] **Step 3: Emit the event at the guard firing site**

In `crates/aaos-tools/src/invocation.rs`, find the existing `if is_repeat {` block (around line 170 — the one that injects `_repeat_guard` into the successful result). **Before** the `if let Ok(ref mut v) = result {` mutation, add:

```rust
        if is_repeat {
            // Phase F-b sub-project 2: emit a dedicated audit event so the
            // plan executor can detect the signal from the broadcast
            // stream without introspecting tool-result JSON. The existing
            // `_repeat_guard` hint stays — it's LLM-visible and is what
            // actually nudges the agent.
            self.audit_log.record(aaos_core::AuditEvent::new(
                agent_id,
                aaos_core::AuditEventKind::ToolRepeatGuardFired {
                    agent_id,
                    tool: tool_name.to_string(),
                    attempt_count: attempt_count as u32,
                },
            ));

            let hint = format!(
                "You have called `{}` with these exact arguments {} times in this subtask. The previous attempts returned the same result. Try different arguments or a different tool.",
                tool_name, attempt_count
            );
            // ... existing mutation of `result` continues unchanged ...
```

Preserve every subsequent line of the existing `if is_repeat {` body.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p aaos-tools 2>&1 | tail -5`
Expected: all pass including the new one.

- [ ] **Step 5: Commit**

```bash
git add crates/aaos-tools/src/invocation.rs
git commit -m "$(cat <<'EOF'
feat(tools): emit ToolRepeatGuardFired audit event at repeat threshold

The existing tool-repeat guard mutates a `_repeat_guard` hint into
the LLM-visible tool result. This commit adds a parallel audit
event at the same firing site so the plan executor can detect
"repeat guard fired during this subtask" from the broadcast audit
stream — needed by the Phase F-b/2 model-escalation path.

Hint mutation is unchanged (LLM-visible behavior preserved).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `FailedSubtask` struct + `decide_escalation` + `carry_tiers_forward`

**Files:**
- Modify: `crates/aaos-runtime/src/plan/escalation.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/aaos-runtime/src/plan/escalation.rs`:

```rust
use crate::plan::{Plan, Subtask};
use aaos_core::{AuditEvent, AuditEventKind};

/// Information carried from a failed execution attempt back to the replan
/// loop so it can decide whether to escalate each failed subtask's model
/// tier. Attached to `ExecutorError::Correctable` instead of a free-form
/// reason string when replan-eligible failures are the cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedSubtask {
    pub subtask_id: String,
    pub role: String,
    /// What the executor saw for this subtask during the failed attempt.
    pub observed_signals: Vec<EscalationSignal>,
}

/// Pure function: given a failed subtask's context and the subtask's role
/// configuration, decide which escalation signal (if any) should trigger
/// a tier bump. Returns the highest-priority configured signal that
/// actually fired — priority order is ReplanRetry > MaxTokens >
/// ToolRepeatGuard (failure-first heuristic).
pub fn decide_escalation(
    failed: &FailedSubtask,
    configured: &[EscalationSignal],
    ladder_len: usize,
    current_tier: u8,
) -> Option<EscalationSignal> {
    if ladder_len <= 1 {
        return None;
    }
    if (current_tier as usize) >= ladder_len - 1 {
        return None;
    }
    // Fixed priority for deterministic escalation-event emission: whichever
    // signal best explains the failure wins.
    for candidate in &[
        EscalationSignal::ReplanRetry,
        EscalationSignal::MaxTokens,
        EscalationSignal::ToolRepeatGuard,
    ] {
        if configured.contains(candidate) && failed.observed_signals.contains(candidate) {
            return Some(*candidate);
        }
    }
    None
}

/// Scan an audit-event slice for per-subtask signals. Used by the executor
/// after a failed batch to populate `FailedSubtask::observed_signals`.
pub fn signals_for_subtask(
    subtask_id: &str,
    subtask_agent_ids: &[aaos_core::AgentId],
    events: &[AuditEvent],
) -> Vec<EscalationSignal> {
    let mut out = Vec::new();
    for ev in events {
        match &ev.event {
            AuditEventKind::SubtaskCompleted {
                subtask_id: sid,
                success: false,
            } if sid == subtask_id => {
                if !out.contains(&EscalationSignal::ReplanRetry) {
                    out.push(EscalationSignal::ReplanRetry);
                }
            }
            AuditEventKind::ToolRepeatGuardFired { agent_id, .. }
                if subtask_agent_ids.contains(agent_id) =>
            {
                if !out.contains(&EscalationSignal::ToolRepeatGuard) {
                    out.push(EscalationSignal::ToolRepeatGuard);
                }
            }
            AuditEventKind::AgentExecutionCompleted { stop_reason, .. }
                if stop_reason == "MaxTokens"
                    && subtask_agent_ids.contains(&ev.agent_id) =>
            {
                if !out.contains(&EscalationSignal::MaxTokens) {
                    out.push(EscalationSignal::MaxTokens);
                }
            }
            _ => {}
        }
    }
    out
}

/// After a replan produces a new plan, carry forward the escalated
/// `current_model_tier` from the old plan by matching subtask ids. Subtasks
/// in the new plan whose ids are NOT in the old plan keep tier 0. Any
/// `failed_tier_bumps` map takes precedence over the carryover — that's
/// where `decide_escalation`'s increment is applied.
pub fn carry_tiers_forward(
    new_plan: &mut Plan,
    old_plan: &Plan,
    failed_tier_bumps: &std::collections::HashMap<String, u8>,
) {
    use std::collections::HashMap;
    let old_tiers: HashMap<&str, u8> = old_plan
        .subtasks
        .iter()
        .map(|s| (s.id.as_str(), s.current_model_tier))
        .collect();
    for s in new_plan.subtasks.iter_mut() {
        if let Some(&bumped) = failed_tier_bumps.get(&s.id) {
            s.current_model_tier = bumped;
        } else if let Some(&prev) = old_tiers.get(s.id.as_str()) {
            s.current_model_tier = prev;
        }
    }
}

#[cfg(test)]
mod decide_tests {
    use super::*;

    #[test]
    fn ladder_too_short_returns_none() {
        let f = FailedSubtask {
            subtask_id: "a".into(),
            role: "writer".into(),
            observed_signals: vec![EscalationSignal::ReplanRetry],
        };
        assert_eq!(decide_escalation(&f, &default_escalation_signals(), 1, 0), None);
    }

    #[test]
    fn top_of_ladder_returns_none() {
        let f = FailedSubtask {
            subtask_id: "a".into(),
            role: "writer".into(),
            observed_signals: vec![EscalationSignal::ReplanRetry],
        };
        // Ladder len 2, already at tier 1 (top).
        assert_eq!(decide_escalation(&f, &default_escalation_signals(), 2, 1), None);
    }

    #[test]
    fn replan_retry_wins_over_tool_repeat_when_both_fired() {
        let f = FailedSubtask {
            subtask_id: "a".into(),
            role: "writer".into(),
            observed_signals: vec![
                EscalationSignal::ToolRepeatGuard,
                EscalationSignal::ReplanRetry,
            ],
        };
        assert_eq!(
            decide_escalation(&f, &default_escalation_signals(), 2, 0),
            Some(EscalationSignal::ReplanRetry)
        );
    }

    #[test]
    fn configured_off_signal_does_not_fire() {
        let f = FailedSubtask {
            subtask_id: "a".into(),
            role: "writer".into(),
            observed_signals: vec![EscalationSignal::ReplanRetry],
        };
        // Only MaxTokens is configured; ReplanRetry not listed.
        assert_eq!(
            decide_escalation(&f, &[EscalationSignal::MaxTokens], 2, 0),
            None
        );
    }

    #[test]
    fn carry_tiers_forward_preserves_survivors_and_applies_bumps() {
        use crate::plan::{Plan, Subtask};
        let old = Plan {
            subtasks: vec![
                Subtask {
                    id: "a".into(),
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                    current_model_tier: 1, // was tier 1
                },
                Subtask {
                    id: "b".into(),
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                    current_model_tier: 0,
                },
            ],
            final_output: "a".into(),
        };
        let mut new_plan = Plan {
            subtasks: vec![
                Subtask {
                    id: "a".into(), // survives
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                    current_model_tier: 0,
                },
                Subtask {
                    id: "b".into(), // survives, bumped
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                    current_model_tier: 0,
                },
                Subtask {
                    id: "c".into(), // brand new — stays at tier 0
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                    current_model_tier: 0,
                },
            ],
            final_output: "c".into(),
        };

        let mut bumps = std::collections::HashMap::new();
        bumps.insert("b".to_string(), 1u8);

        carry_tiers_forward(&mut new_plan, &old, &bumps);

        // "a" inherits its previous tier (1)
        assert_eq!(new_plan.subtasks[0].current_model_tier, 1);
        // "b" applies the bump (1), ignoring its previous tier
        assert_eq!(new_plan.subtasks[1].current_model_tier, 1);
        // "c" is brand new — stays at tier 0
        assert_eq!(new_plan.subtasks[2].current_model_tier, 0);
    }
}
```

- [ ] **Step 2: Run to verify tests pass**

Run: `cargo test -p aaos-runtime --lib plan::escalation 2>&1 | tail -15`
Expected: 8 tests pass (3 from Task 1 + 5 new).

- [ ] **Step 3: Commit**

```bash
git add crates/aaos-runtime/src/plan/escalation.rs
git commit -m "$(cat <<'EOF'
feat(runtime): FailedSubtask + decide_escalation + carry_tiers_forward

Pure decision logic for model-tier escalation:
- FailedSubtask carries observed signals from a failed attempt.
- decide_escalation picks the highest-priority configured signal that
  fired (ReplanRetry > MaxTokens > ToolRepeatGuard), respecting
  ladder length + current tier.
- signals_for_subtask scans an audit slice to populate
  FailedSubtask.observed_signals.
- carry_tiers_forward merges prior tiers into a replanned plan by
  subtask id, with explicit per-id bumps winning over carryover.

Pure functions — no IO, no deps beyond core. Five new unit tests.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Executor wiring — tier-selected model at spawn + escalation at replan

**Files:**
- Modify: `crates/aaos-runtime/src/plan/executor.rs`

- [ ] **Step 1: Write the failing integration test**

In the existing `#[cfg(test)] mod tests` block inside `crates/aaos-runtime/src/plan/executor.rs` (after the other wall-clock / hop-chain tests), add:

```rust
    #[tokio::test]
    async fn subtask_escalates_on_replan_retry() {
        use aaos_core::AuditEventKind;
        use crate::plan::{EscalationSignal, default_escalation_signals};
        use std::sync::atomic::{AtomicU32, Ordering};

        let mut catalog = RoleCatalog::default();
        let mut role = make_role_for_tests("writer");
        role.model = "fast-stub".into();
        role.model_ladder = vec!["fast-stub".into(), "slow-stub".into()];
        role.escalate_on = default_escalation_signals();
        catalog.roles_mut().insert("writer".into(), role);

        let attempt_counter = Arc::new(AtomicU32::new(0));
        let counter_inner = attempt_counter.clone();

        let runner: SubtaskRunner = Arc::new(move |id, manifest_yaml, _m, _o, _d| {
            let counter = counter_inner.clone();
            Box::pin(async move {
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                // Assertion-via-panic: first attempt must target fast-stub;
                // second attempt must target slow-stub.
                if attempt == 0 {
                    assert!(manifest_yaml.contains("model: fast-stub"), "attempt 0 must use fast-stub: {manifest_yaml}");
                    // Fail → triggers ReplanRetry signal.
                    Err(aaos_core::CoreError::Ipc("deliberate failure".into()))
                } else {
                    assert!(manifest_yaml.contains("model: slow-stub"), "attempt 1 must use slow-stub: {manifest_yaml}");
                    Ok(SubtaskResult {
                        subtask_id: id,
                        agent_id: AgentId::new(),
                        response: "ok".into(),
                        input_tokens: 0,
                        output_tokens: 0,
                    })
                }
            })
        });

        let audit: Arc<InMemoryAuditLog> = Arc::new(InMemoryAuditLog::new());
        let exec = PlanExecutor::new(
            Arc::new(catalog),
            Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into())),
            runner,
            audit.clone() as Arc<dyn AuditLog>,
            std::env::temp_dir(),
        );

        // One-subtask plan; the runner fails on tier 0 then succeeds on
        // tier 1. execute_plan goes through run(), which drives the replan
        // loop, so use that entry point.
        let result = exec.run("any goal", uuid::Uuid::new_v4()).await;
        assert!(result.is_ok(), "expected run to succeed after one escalation: {result:?}");

        let escalations: Vec<_> = audit
            .events()
            .into_iter()
            .filter(|e| matches!(&e.event, AuditEventKind::SubtaskModelEscalated { .. }))
            .collect();
        assert_eq!(escalations.len(), 1, "expected exactly one escalation event");
        if let AuditEventKind::SubtaskModelEscalated { from_model, to_model, reason, .. } =
            &escalations[0].event
        {
            assert_eq!(from_model, "fast-stub");
            assert_eq!(to_model, "slow-stub");
            assert_eq!(reason, "replan_retry");
        }
    }

    // Helper — produces a role with valid defaults, overridable by caller.
    fn make_role_for_tests(name: &str) -> Role {
        use crate::plan::{EscalationSignal, RoleBudget, RoleRetry};
        Role {
            name: name.into(),
            model: "stub".into(),
            parameters: Default::default(),
            capabilities: vec![],
            system_prompt: "x".into(),
            message_template: "y".into(),
            budget: RoleBudget { max_input_tokens: 1000, max_output_tokens: 1000 },
            retry: RoleRetry { max_attempts: 1, on: vec![] },
            priority: 128,
            scaffold: None,
            model_ladder: vec![],
            escalate_on: vec![
                EscalationSignal::ReplanRetry,
                EscalationSignal::ToolRepeatGuard,
                EscalationSignal::MaxTokens,
            ],
        }
    }
```

Note: the helper `make_role_for_tests` duplicates parts of `fetcher_catalog`. That's fine for now — it's the same constructor pattern, named differently to signal "use this in new tests." The existing `fetcher_catalog` stays unchanged.

Note: the test drives `run("goal", run_id)` rather than `execute_plan(&plan, ...)` because the replan path only triggers inside `run`. `Planner::replan` on `MockLlm` must return the same single-subtask plan (id "a"). If MockLlm doesn't do that, the test needs a PlannerStub that always returns the same plan; implement if needed:

```rust
// If MockLlm's replan returns a plan with a different id, the carry-tiers-
// forward logic will still apply the bump; the test assertion stays valid.
```

- [ ] **Step 2: Run to verify test fails**

Run: `cargo test -p aaos-runtime --lib subtask_escalates_on_replan_retry 2>&1 | tail -20`
Expected: fail because the executor doesn't read `current_model_tier` or emit `SubtaskModelEscalated` yet.

- [ ] **Step 3: Update `ExecutorError::Correctable` to carry `FailedSubtask`s**

In `crates/aaos-runtime/src/plan/executor.rs`, find the `pub enum ExecutorError` definition. Replace `Correctable(String)` with a structured form — BUT keep the string for human-readable replan input. Simplest evolution:

```rust
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    /// Planner-correctable failure. `reason` is the human-readable string
    /// the planner sees on replan. `failures` carries structured per-
    /// subtask metadata the replan loop uses for model-tier escalation.
    /// When the failure is not per-subtask (e.g. "empty plan"),
    /// `failures` is empty.
    #[error("planner-correctable: {reason}")]
    Correctable {
        reason: String,
        failures: Vec<crate::plan::escalation::FailedSubtask>,
    },
    #[error("terminal: {0}")]
    Terminal(#[from] CoreError),
}
```

Update every `ExecutorError::Correctable(...)` construction site in the file to use the struct form. Most sites have no per-subtask metadata — set `failures: vec![]`. The batch-failure path in `execute_plan` (around line 288) IS per-subtask — populate `failures` there by calling `signals_for_subtask` for each failed subtask id.

Specifically, in `execute_plan`'s batch loop, replace the `first_failure: Option<String>` local with two locals:

```rust
            let mut first_failure_reason: Option<String> = None;
            let mut failed: Vec<crate::plan::escalation::FailedSubtask> = Vec::new();
            let mut batch_events_snapshot: Vec<AuditEvent> = Vec::new();
            // Snapshot the audit log before the batch so signals_for_subtask
            // only sees events from this batch. InMemoryAuditLog::events()
            // returns Vec<AuditEvent>; grab the tail-end snapshot.
            let pre_len = self.audit_log.event_count(); // see below
```

This needs `AuditLog::event_count()` OR direct access to the broadcast. **Simpler**: skip the pre-snapshot. Instead, at the point you build the `FailedSubtask` list, just call `signals_for_subtask(subtask_id, &[subtask_agent_id], self.audit_log.events())` — the snapshot of recent events. Any false positives from prior batches are filtered by the `subtask_id` and `agent_id` match.

BUT `Arc<dyn AuditLog>` doesn't expose `.events()` — that's on `InMemoryAuditLog` specifically. The production daemon uses `BroadcastAuditLog`. **Correct answer**: add a method to the trait or downcast. Cleanest: add `fn events_snapshot(&self) -> Vec<AuditEvent>` to the `AuditLog` trait with a default returning empty — safe for broadcast-only impls where we can't enumerate — and override on `InMemoryAuditLog`. **Even simpler**: the executor already holds `broadcast_audit: Arc<BroadcastAuditLog>` in production. Instead, subscribe to the broadcast at `execute_plan` entry, collect all events into a local `Vec<AuditEvent>` for the duration of the batch. That's the cleanest architectural fit and doesn't require touching the trait.

Apply: at the top of `execute_plan`, subscribe to the broadcast via the audit_log's `subscribe()` method if available. If `Arc<dyn AuditLog>` doesn't expose `subscribe`, extend the trait with an optional `fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<AuditEvent>>` (default returns `None`; `BroadcastAuditLog` overrides). Spawn a short-lived task that drains the receiver into a local `Arc<Mutex<Vec<AuditEvent>>>` until the batch completes.

For the executor tests using `InMemoryAuditLog`, implement `subscribe` there too by storing a `tokio::sync::broadcast::Sender` internally.

**This is getting big. Compromise: for v1 of this feature, query via `InMemoryAuditLog::events()` ONLY, and expose a `fn as_in_memory(&self) -> Option<&InMemoryAuditLog>` on the trait.** That's ugly but minimal. In production the daemon's `broadcast_audit` wraps an `InMemoryAuditLog` internally anyway (verify: see `BroadcastAuditLog::new(inner_audit, ...)`). Add a passthrough `as_in_memory()` on `BroadcastAuditLog` that returns the inner.

Actually the cleanest narrow change: **extend the `AuditLog` trait with `fn events_snapshot(&self) -> Vec<AuditEvent> { Vec::new() }`** (default empty). Override on `InMemoryAuditLog` + `BroadcastAuditLog` (the latter forwards to its inner). Then executor calls `self.audit_log.events_snapshot()`.

**Decision: go with `events_snapshot()` on the trait.** Add it as a separate sub-step below, then use it in the executor.

- [ ] **Step 4: Add `events_snapshot` to `AuditLog` trait**

In `crates/aaos-core/src/audit.rs`, find the `pub trait AuditLog` definition. Add:

```rust
    /// Return a snapshot of all events recorded so far. Default returns
    /// empty (safe for write-only / broadcast-only impls that don't
    /// retain events). Implementations that retain events (in-memory
    /// store, broadcast wrapping an in-memory store) should override.
    fn events_snapshot(&self) -> Vec<AuditEvent> {
        Vec::new()
    }
```

Override on `InMemoryAuditLog`:

```rust
impl InMemoryAuditLog {
    // ... existing ...
}

impl AuditLog for InMemoryAuditLog {
    // ... existing record() ...

    fn events_snapshot(&self) -> Vec<AuditEvent> {
        self.events()
    }
}
```

In `crates/agentd/src/broadcast_audit.rs` (or wherever `BroadcastAuditLog` is defined), override `events_snapshot` to forward to the inner:

```rust
impl AuditLog for BroadcastAuditLog {
    // ... existing ...

    fn events_snapshot(&self) -> Vec<AuditEvent> {
        self.inner.events_snapshot()
    }
}
```

Check `cargo build --workspace` stays clean. One new unit test asserting the default behavior on a write-only audit:

```rust
#[test]
fn audit_log_events_snapshot_default_returns_empty() {
    struct WriteOnly;
    impl AuditLog for WriteOnly {
        fn record(&self, _: AuditEvent) {}
    }
    let log = WriteOnly;
    assert!(log.events_snapshot().is_empty());
}
```

- [ ] **Step 5: Populate `FailedSubtask`s in `execute_plan`**

In `crates/aaos-runtime/src/plan/executor.rs::execute_plan`, when building the `Correctable` error on batch failure, populate `failures` by iterating the batch's failed subtasks + calling `signals_for_subtask`:

```rust
            if let Some(reason) = first_failure_reason {
                // Populate structured per-subtask failure context for the
                // replan loop. For each failed subtask we know its id + the
                // agent_id of the attempt (nil UUID for pre-launch failures
                // like hop-exhaustion; real UUID for post-launch failures).
                let events = self.audit_log.events_snapshot();
                let failures: Vec<crate::plan::escalation::FailedSubtask> = failed_ids_with_agents
                    .iter()
                    .map(|(subtask_id, agent_id, role)| {
                        let observed = crate::plan::escalation::signals_for_subtask(
                            subtask_id,
                            &[*agent_id],
                            &events,
                        );
                        crate::plan::escalation::FailedSubtask {
                            subtask_id: subtask_id.clone(),
                            role: role.clone(),
                            observed_signals: observed,
                        }
                    })
                    .collect();
                return Err(ExecutorError::Correctable { reason, failures });
            }
```

You'll need to collect `failed_ids_with_agents: Vec<(String, AgentId, String)>` (subtask id, agent id, role name) alongside `first_failure_reason` as the batch loop iterates. Plumb that into the existing match arms: when an `Err(e)` arm fires, push `(subtask.id.clone(), AgentId::from_uuid(uuid::Uuid::nil()), subtask.role.clone())`; when the `check_declared_outputs_exist` path fires, push `(subtask.id.clone(), r.agent_id, subtask.role.clone())`.

- [ ] **Step 6: Apply `resolved_ladder` + `current_model_tier` in `spawn_subtask`**

In `spawn_subtask` (LLM-powered role branch, around line 393), replace:

```rust
        let manifest_yaml = role.render_manifest(&resolved_params);
```

with:

```rust
        let ladder = role.resolved_ladder().unwrap_or_else(|_| vec![role.model.clone()]);
        let tier = (subtask.current_model_tier as usize).min(ladder.len() - 1);
        let model_for_this_tier = &ladder[tier];
        let manifest_yaml = role.render_manifest_with_model(model_for_this_tier, &resolved_params);
```

(`unwrap_or_else` is defensive — `resolved_ladder` only errors on mismatched ladder[0]/model, which the catalog loader already rejected; this fallback keeps execution going if a role somehow slipped through.)

- [ ] **Step 7: Implement the escalation path in `run`**

Find the replan loop in `run` (lines 145-180). Replace the `Err(ExecutorError::Correctable(reason))` arm with:

```rust
                Err(ExecutorError::Correctable { reason, failures }) if replans_used < self.max_replans => {
                    self.audit_log.record(AuditEvent::new(
                        AgentId::from_uuid(uuid::Uuid::nil()),
                        AuditEventKind::PlanReplanned {
                            reason: reason.clone(),
                        },
                    ));

                    // Phase F-b/2: decide per-subtask escalation.
                    let mut tier_bumps: std::collections::HashMap<String, u8> =
                        std::collections::HashMap::new();
                    for f in &failures {
                        let Some(role) = self.catalog.get(&f.role) else { continue };
                        let ladder = role.resolved_ladder().unwrap_or_else(|_| vec![role.model.clone()]);
                        // Find the old subtask's current tier.
                        let current_tier = plan.subtasks.iter()
                            .find(|s| s.id == f.subtask_id)
                            .map(|s| s.current_model_tier)
                            .unwrap_or(0);
                        if let Some(signal) = crate::plan::escalation::decide_escalation(
                            f,
                            &role.escalate_on,
                            ladder.len(),
                            current_tier,
                        ) {
                            let new_tier = (current_tier + 1).min((ladder.len() - 1) as u8);
                            tier_bumps.insert(f.subtask_id.clone(), new_tier);
                            self.audit_log.record(AuditEvent::new(
                                AgentId::from_uuid(uuid::Uuid::nil()),
                                AuditEventKind::SubtaskModelEscalated {
                                    subtask_id: f.subtask_id.clone(),
                                    from_tier: current_tier,
                                    to_tier: new_tier,
                                    from_model: ladder[current_tier as usize].clone(),
                                    to_model: ladder[new_tier as usize].clone(),
                                    reason: signal.reason().to_string(),
                                },
                            ));
                        }
                    }

                    let new_plan_from_planner = self
                        .planner
                        .replan(goal, &self.catalog, &plan, &reason)
                        .await
                        .map_err(ExecutorError::from)?;

                    let mut new_plan = new_plan_from_planner;
                    crate::plan::escalation::carry_tiers_forward(&mut new_plan, &plan, &tier_bumps);
                    plan = new_plan;
                    self.write_plan_json(&run_root, &plan)?;
                    replans_used += 1;
                }
```

- [ ] **Step 8: Fix every `ExecutorError::Correctable` call-site**

Every existing `Err(ExecutorError::Correctable("msg".into()))` and `Err(ExecutorError::Correctable(format!("..." )))` needs to become `Err(ExecutorError::Correctable { reason: format!("..."), failures: vec![] })`. Run:

```
cargo build --workspace 2>&1 | grep "E0023\|expected" | head -20
```

Fix every hit. There are about 7-8 sites in executor.rs and possibly 1-2 in tests.

Also fix pattern-match sites: any `match .. { Err(ExecutorError::Correctable(r)) => .. }` becomes `Err(ExecutorError::Correctable { reason: r, .. })`.

- [ ] **Step 9: Run tests**

Run: `cargo test --workspace 2>&1 | grep "test result" | grep -v "ok" | head`
Expected: empty.

Run: `cargo test -p aaos-runtime --lib subtask_escalates_on_replan_retry 2>&1 | tail -10`
Expected: pass.

- [ ] **Step 10: Commit**

```bash
git add crates/
git commit -m "$(cat <<'EOF'
feat(runtime): tier-selected model at spawn + per-failure escalation on replan

Executor wiring for dynamic model routing:
- spawn_subtask uses role.resolved_ladder()[subtask.current_model_tier]
  to pick the model, feeds it into render_manifest_with_model.
- execute_plan now returns ExecutorError::Correctable with a structured
  Vec<FailedSubtask> (subtask id, role, observed signals). The string
  reason is preserved for the planner's replan prompt.
- run() drives the replan loop: for each FailedSubtask, consults
  decide_escalation, records the tier bump in a HashMap, emits
  SubtaskModelEscalated, then carry_tiers_forward applies the bumps
  to the planner's new plan.
- AuditLog::events_snapshot() added as a default-empty trait method,
  overridden on InMemoryAuditLog + BroadcastAuditLog, so the executor
  can slice recent events for signal detection.

One integration test exercises a 2-tier role end-to-end: runner fails
on tier 0, runner succeeds on tier 1, exactly one
SubtaskModelEscalated event fires with from=fast-stub to=slow-stub.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: More escalation integration tests

**Files:**
- Modify: `crates/aaos-runtime/src/plan/executor.rs`

- [ ] **Step 1: Write the failing tests**

Append to the same `#[cfg(test)] mod tests` block in `crates/aaos-runtime/src/plan/executor.rs`:

```rust
    #[tokio::test]
    async fn subtask_does_not_escalate_when_signal_disabled() {
        use crate::plan::EscalationSignal;
        use std::sync::atomic::{AtomicU32, Ordering};

        let mut catalog = RoleCatalog::default();
        let mut role = make_role_for_tests("writer");
        role.model = "fast-stub".into();
        role.model_ladder = vec!["fast-stub".into(), "slow-stub".into()];
        // ONLY max_tokens is configured — ReplanRetry will fire but be
        // ignored.
        role.escalate_on = vec![EscalationSignal::MaxTokens];
        catalog.roles_mut().insert("writer".into(), role);

        let attempt_counter = Arc::new(AtomicU32::new(0));
        let counter_inner = attempt_counter.clone();
        let runner: SubtaskRunner = Arc::new(move |id, manifest_yaml, _, _, _| {
            let counter = counter_inner.clone();
            Box::pin(async move {
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                // Every attempt (up to max_replans) should use fast-stub
                // because the only configured signal (MaxTokens) never fires.
                assert!(manifest_yaml.contains("model: fast-stub"), "all attempts must stay at fast-stub; attempt {attempt}");
                Err(aaos_core::CoreError::Ipc("fail again".into()))
            })
        });

        let audit: Arc<InMemoryAuditLog> = Arc::new(InMemoryAuditLog::new());
        let exec = PlanExecutor::new(
            Arc::new(catalog),
            Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into())),
            runner,
            audit.clone() as Arc<dyn AuditLog>,
            std::env::temp_dir(),
        );

        let _ = exec.run("any goal", uuid::Uuid::new_v4()).await;

        let escalations: Vec<_> = audit
            .events()
            .into_iter()
            .filter(|e| matches!(&e.event, AuditEventKind::SubtaskModelEscalated { .. }))
            .collect();
        assert!(
            escalations.is_empty(),
            "no escalation should fire when the only signal seen is not in escalate_on; got {}",
            escalations.len()
        );
    }

    #[tokio::test]
    async fn subtask_escalation_caps_at_ladder_top() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let mut catalog = RoleCatalog::default();
        let mut role = make_role_for_tests("writer");
        role.model = "fast-stub".into();
        role.model_ladder = vec!["fast-stub".into(), "slow-stub".into()];
        catalog.roles_mut().insert("writer".into(), role);

        let attempt_counter = Arc::new(AtomicU32::new(0));
        let counter_inner = attempt_counter.clone();
        let runner: SubtaskRunner = Arc::new(move |_id, _m, _msg, _o, _d| {
            let counter = counter_inner.clone();
            Box::pin(async move {
                let _ = counter.fetch_add(1, Ordering::SeqCst);
                Err(aaos_core::CoreError::Ipc("always fail".into()))
            })
        });

        let audit: Arc<InMemoryAuditLog> = Arc::new(InMemoryAuditLog::new());
        let exec = PlanExecutor::new(
            Arc::new(catalog),
            Arc::new(Planner::new(Arc::new(MockLlm), "deepseek-chat".into())),
            runner,
            audit.clone() as Arc<dyn AuditLog>,
            std::env::temp_dir(),
        );

        let _ = exec.run("any goal", uuid::Uuid::new_v4()).await;

        let escalations: Vec<_> = audit
            .events()
            .into_iter()
            .filter(|e| matches!(&e.event, AuditEventKind::SubtaskModelEscalated { .. }))
            .collect();
        assert_eq!(
            escalations.len(),
            1,
            "ladder length 2 → exactly one escalation possible; got {}",
            escalations.len()
        );
    }
```

- [ ] **Step 2: Run to verify tests pass**

Run: `cargo test -p aaos-runtime --lib subtask_does_not_escalate_when_signal_disabled subtask_escalation_caps_at_ladder_top 2>&1 | tail -10`
Expected: both pass.

- [ ] **Step 3: Commit**

```bash
git add crates/aaos-runtime/src/plan/executor.rs
git commit -m "$(cat <<'EOF'
test(runtime): escalation guardrails — signal-off + ladder-top cap

Two more integration tests for the escalation path:
- escalate_on=[MaxTokens] + failure-that-fires-ReplanRetry does NOT
  escalate — the filter is respected.
- 2-tier ladder with a always-fail runner produces exactly ONE
  escalation event, not one per replan attempt — ladder top caps.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Operator-visible events + agentd scheduler-view wiring

**Files:**
- Modify: `crates/agentd/src/cli/output.rs`
- Modify: `crates/agentd/src/server.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/agentd/src/cli/output.rs`'s existing `mod tests`:

```rust
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
```

- [ ] **Step 2: Run to verify compile/assert fail**

Run: `cargo test -p agentd model_escalated_is_operator_visible tool_repeat_guard_is_operator_visible 2>&1 | tail -10`
Expected: both fail (not in whitelist, no formatter branch).

- [ ] **Step 3: Whitelist + formatter**

In `crates/agentd/src/cli/output.rs`, extend `is_operator_visible` by adding both variants to the `true` arm:

```rust
pub fn is_operator_visible(event: &AuditEvent) -> bool {
    match &event.event {
        AuditEventKind::AgentSpawned { .. }
        | AuditEventKind::ToolInvoked { .. }
        | AuditEventKind::AgentExecutionCompleted { .. }
        | AuditEventKind::AgentLoopStopped { .. }
        | AuditEventKind::CapabilityDenied { .. }
        | AuditEventKind::SubtaskTtlExpired { .. }
        | AuditEventKind::SubtaskModelEscalated { .. }
        | AuditEventKind::ToolRepeatGuardFired { .. } => true,
        AuditEventKind::ToolResult { success, .. } => !success,
        _ => false,
    }
}
```

In `format_operator_line`'s big match, add two new arms:

```rust
        AuditEventKind::SubtaskModelEscalated {
            subtask_id,
            from_model,
            to_model,
            reason,
            ..
        } => {
            let label = format!(
                "model escalated ({reason}): {subtask_id} — {from_model} → {to_model}"
            );
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
```

- [ ] **Step 4: Thread `PerModelLatencyTracker` through agentd Server**

In `crates/agentd/src/server.rs`, find `build_scheduler_and_tracker`. Extend to return THREE things: the scheduler, the subtask-wall-clock tracker (for TTL), and the per-model tracker (for observability). Change the signature:

```rust
    fn build_scheduler_and_tracker() -> (
        Arc<aaos_runtime::scheduler::ReasoningScheduler>,
        Arc<dyn aaos_runtime::LatencyTracker>,       // subtask-wall-clock (existing)
        Arc<aaos_runtime::scheduler::PerModelLatencyTracker>,  // new
    ) {
        let max_concurrent = std::env::var("AAOS_MAX_CONCURRENT_INFERENCE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(3);
        (
            aaos_runtime::scheduler::ReasoningScheduler::new(max_concurrent),
            Arc::new(aaos_runtime::SubtaskWallClockTracker::new()),
            Arc::new(aaos_runtime::scheduler::PerModelLatencyTracker::new()),
        )
    }
```

Add a new field on `Server`:

```rust
    pub(crate) per_model_latency: Arc<aaos_runtime::scheduler::PerModelLatencyTracker>,
```

Initialize it in every constructor (just like `reasoning_scheduler` and `latency_tracker`):

```rust
let (reasoning_scheduler, latency_tracker, per_model_latency) = Self::build_scheduler_and_tracker();
```

Add `per_model_latency` to the `Self { ... }` literal in every constructor.

In `execute_agent_for_subtask`, after the existing `SchedulerView::new` wrap, register the subtask→model mapping with the per-model tracker:

```rust
        // Phase F-b/2: register subtask→model for per-model latency stats.
        // The manifest's `model:` field is the model the subtask will
        // actually call; parse it out (or pass it explicitly from the
        // caller — cleaner). Here we parse for minimum blast radius.
        if let Some(model_line) = manifest.model.as_deref() {
            self.per_model_latency.register(subtask_id, model_line);
        }
```

Wait — `manifest` here is `AgentManifest`, which has a `model: String` field. Use `manifest.model` directly:

```rust
        self.per_model_latency.register(subtask_id, &manifest.model);
```

Place this right before the `SchedulerView::new(...)` call.

Also add `per_model_latency` as a THIRD tracker to pass alongside `latency_tracker` to SchedulerView — but SchedulerView::new takes one tracker. Two options:

- **(a)** Change SchedulerView to take `Vec<Arc<dyn LatencyTracker>>` (composite).
- **(b)** Build a composite tracker wrapper that implements `LatencyTracker` and delegates to both.

Pick (b) for minimum disruption. Add a tiny new type in `crates/aaos-runtime/src/scheduler/latency.rs`:

```rust
/// Delegating LatencyTracker that forwards every record call to all
/// inner trackers. Used by agentd to feed both SubtaskWallClockTracker
/// (for TTL) and PerModelLatencyTracker (for observability) from one
/// SchedulerView::new wrap.
pub struct CompositeLatencyTracker {
    inner: Vec<Arc<dyn LatencyTracker>>,
}

impl CompositeLatencyTracker {
    pub fn new(inner: Vec<Arc<dyn LatencyTracker>>) -> Self {
        Self { inner }
    }
}

impl LatencyTracker for CompositeLatencyTracker {
    fn record(&self, subtask_id: &str, elapsed: std::time::Duration) {
        for t in &self.inner {
            t.record(subtask_id, elapsed);
        }
    }
    fn wall_clock_elapsed(&self, subtask_id: &str) -> std::time::Duration {
        // Return the first non-zero result from any inner tracker.
        for t in &self.inner {
            let d = t.wall_clock_elapsed(subtask_id);
            if d != std::time::Duration::ZERO {
                return d;
            }
        }
        std::time::Duration::ZERO
    }
}
```

Export it from `scheduler/mod.rs`. Then in `execute_agent_for_subtask`, build the composite on-the-fly:

```rust
        let composite: Arc<dyn aaos_runtime::LatencyTracker> =
            Arc::new(aaos_runtime::scheduler::CompositeLatencyTracker::new(vec![
                self.latency_tracker.clone(),
                self.per_model_latency.clone() as Arc<dyn aaos_runtime::LatencyTracker>,
            ]));

        let llm: Arc<dyn aaos_llm::LlmClient> = Arc::new(
            aaos_runtime::scheduler::SchedulerView::new(
                raw_llm,
                self.reasoning_scheduler.clone(),
                composite,
                subtask_id.to_string(),
                128,
                deadline,
            ),
        );
```

Add a one-test-file addition (in `per_model_latency.rs` or a new module) for `CompositeLatencyTracker`:

```rust
    #[test]
    fn composite_fans_out_records() {
        use super::super::{LatencyTracker, SubtaskWallClockTracker};
        let wall = Arc::new(SubtaskWallClockTracker::new());
        let per_model = Arc::new(PerModelLatencyTracker::new());
        per_model.register("a", "deepseek-chat");

        let composite: Arc<dyn LatencyTracker> = Arc::new(CompositeLatencyTracker::new(vec![
            wall.clone() as Arc<dyn LatencyTracker>,
            per_model.clone() as Arc<dyn LatencyTracker>,
        ]));

        composite.record("a", std::time::Duration::from_millis(100));

        assert_eq!(wall.wall_clock_elapsed("a"), std::time::Duration::from_millis(100));
        assert_eq!(
            per_model.p50("deepseek-chat").unwrap(),
            std::time::Duration::from_millis(100)
        );
    }
```

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace 2>&1 | grep "test result" | grep -v "ok" | head` — empty.

Run: `cargo fmt --all -- --check 2>&1 | tail -3` — clean.

Run: `cargo build --workspace 2>&1 | tail -3` — clean.

- [ ] **Step 6: Commit**

```bash
git add crates/
git commit -m "$(cat <<'EOF'
feat(agentd): operator-visible escalation + per-model latency wiring

Two new audit variants (SubtaskModelEscalated, ToolRepeatGuardFired)
now show in the default `agentd submit` stream, formatted as:
  "model escalated (replan_retry): analyze — deepseek-chat → deepseek-reasoner"  (cyan)
  "repeat guard: web_fetch (attempt 3)"                                          (yellow)

Server holds a new Arc<PerModelLatencyTracker>. Every
execute_agent_for_subtask call registers the subtask→model mapping
(from the rendered manifest) before wrapping the LLM client in
SchedulerView. The SchedulerView is fed a CompositeLatencyTracker
that fans out to both SubtaskWallClockTracker (TTL-consuming) and
PerModelLatencyTracker (observability-only).

Operator-visibility lesson from sub-project 1 BUG #7 applied from
day one.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Role YAML — declare ladder on writer + analyzer (optional but recommended)

**Files:**
- Modify: `packaging/roles/writer.yaml`
- Modify: `packaging/roles/analyzer.yaml`

- [ ] **Step 1: Update writer.yaml**

Find the `model: deepseek-chat` line in `packaging/roles/writer.yaml`. Leave it as-is; add a `model_ladder` block right after:

```yaml
model: deepseek-chat
model_ladder:
  - deepseek-chat        # tier 0: fine for typical synthesis
  - deepseek-reasoner    # tier 1: escalate on replan/repeat/max-tokens
```

Do NOT add `escalate_on` — the default (all three signals active) is what we want.

- [ ] **Step 2: Update analyzer.yaml**

Same pattern:

```yaml
model: deepseek-chat
model_ladder:
  - deepseek-chat
  - deepseek-reasoner
```

- [ ] **Step 3: Do NOT modify fetcher.yaml or generalist.yaml**

Fetcher is a scaffold — its `model` field is documentation-only. Generalist is a one-off fallback; the single model is fine.

- [ ] **Step 4: Run tests**

No Rust changes; workspace tests should pass trivially. Run `cargo test -p agentd submit_streaming 2>&1 | tail -5` — check the role-catalog-load tests still pass (they parse the YAMLs).

- [ ] **Step 5: Commit**

```bash
git add packaging/roles/writer.yaml packaging/roles/analyzer.yaml
git commit -m "$(cat <<'EOF'
roles: declare model_ladder on writer + analyzer

Two tiers each: deepseek-chat → deepseek-reasoner. Default
escalate_on (all three signals) applies. fetcher is a scaffold
(model is documentation-only) and generalist is a one-off fallback
— neither gains a ladder.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Docs — roadmap + architecture (with explicit scope)

**Files:**
- Modify: `docs/roadmap.md`
- Modify: `docs/architecture.md`

- [ ] **Step 1: Mark Gap 2 shipped in roadmap.md**

Find `**Gap 2 — Dynamic model routing.**` in `docs/roadmap.md`. Replace with:

```markdown
**Gap 2 — Dynamic model routing.** *Shipped 2026-04-XX (fill actual date from `git log --date=iso -1`); see `docs/phase-f-b2-design.md` + `docs/phase-f-b2-plan.md`.* Each `Role` declares an optional `model_ladder: Vec<String>` (defaults to `[role.model]`, keeping every pre-existing role back-compat) + `escalate_on: Vec<EscalationSignal>` (defaults to all three: `replan_retry`, `tool_repeat_guard`, `max_tokens`). `Subtask.current_model_tier: u8` tracks the ladder index; planner sets 0, executor increments on replan when a configured signal fired during the failed attempt. `SubtaskModelEscalated` + `ToolRepeatGuardFired` audit events fire on every bump and are operator-visible in the default `agentd submit` stream. A second `LatencyTracker` impl — `PerModelLatencyTracker` — collects per-model p50/p95 into 256-sample bounded rings; **v1 observability only**, no routing decisions consume it. **Scope note:** routing is purely signal-based in v1. No cost/price math, no classifier-based router, no cross-run persistent preference. A future sub-project can build cost-aware routing on top of `PerModelLatencyTracker` once there's real-world distribution data.
```

- [ ] **Step 2: Add a subsection to architecture.md**

Find `#### Reasoning-Slot Scheduler (Phase F-b sub-project 1)` in `docs/architecture.md`. After that subsection, add:

```markdown
#### Dynamic Model Routing (Phase F-b sub-project 2)

Each `Role` declares a `model_ladder` (ordered list; tier 0 == `role.model`). `Subtask.current_model_tier: u8` indexes into the ladder at spawn time — `role.render_manifest_with_model(ladder[tier], params)` produces the per-subtask manifest. The executor's replan path runs `decide_escalation` on each failed subtask against a structured `Vec<FailedSubtask>` (carried through `ExecutorError::Correctable.failures`); on a configured signal it bumps the tier up to `ladder.len() - 1`, emits `SubtaskModelEscalated`, and `carry_tiers_forward` merges the bump into the planner's new plan by subtask-id match.

Three escalation signals, in priority order (highest wins when multiple fired):
1. `ReplanRetry` — any `SubtaskCompleted{success: false}` for this subtask in the failed attempt.
2. `MaxTokens` — any `AgentExecutionCompleted{stop_reason: "MaxTokens"}` for this subtask's agent id.
3. `ToolRepeatGuard` — any `ToolRepeatGuardFired` for this subtask's agent id.

Signals are scanned from the audit broadcast via `AuditLog::events_snapshot()` (default-empty trait method; `InMemoryAuditLog` + `BroadcastAuditLog` override).

**Scope:** signal-based routing only; no cost math, no classifier. `PerModelLatencyTracker` collects per-model p50/p95 into 256-sample bounded rings but is not consumed by any routing decision in v1 — future cost-aware routing can read from it.
```

- [ ] **Step 3: Commit**

```bash
git add docs/roadmap.md docs/architecture.md
git commit -m "$(cat <<'EOF'
docs: Phase F-b sub-project 2 — dynamic model routing shipped

Roadmap: Gap 2 marked shipped with concrete implementation refs and
explicit scope note (signal-based, not cost-based; PerModelLatencyTracker
is observability-only).

Architecture: new subsection describing tier-at-spawn mechanic,
replan-path decision logic, three signals in priority order, and the
scope boundary (no cost math, no classifier).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: CI pass + push + verify green

- [ ] **Step 1: Final sanity sweep**

```bash
cargo build --workspace 2>&1 | tail -3
cargo test --workspace 2>&1 | grep "test result" | grep -v "ok" | head
cargo fmt --all -- --check 2>&1 | tail -3
```

All three must be clean. `cargo test --workspace` output must not contain any failing result lines.

- [ ] **Step 2: Push all commits**

```bash
git log --oneline origin/main..HEAD | head -15
git push
```

- [ ] **Step 3: Wait for CI, verify green**

```bash
sleep 15
gh run list --limit 1
```

Expected `success`. If `failure`, `gh run view <id> --log-failed | tail -40` and fix in place.

- [ ] **Step 4: No commit here** — this is verification only.

---

## Task 14: Fresh-droplet re-verification (Definition of Done)

Applies the lesson from sub-project 1's BUG #1 + #2 twice-broken fix sequence: production smoke before declaring the sub-project shipped.

- [ ] **Step 1: User provisions a fresh droplet + pastes IP + DeepSeek key**

Equivalent to the QA runs done for sub-project 1.

- [ ] **Step 2: Build `.deb` on droplet**

```bash
ssh root@$DROPLET "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal"
ssh root@$DROPLET ". ~/.cargo/env && cargo install cargo-deb --locked"
rsync -az --delete --exclude='target/' --exclude='.git/' --exclude='node_modules/' /root/apps/aaOS/ root@$DROPLET:/root/aaOS/
ssh root@$DROPLET "cd /root/aaOS && . ~/.cargo/env && ./packaging/build-deb.sh --features mcp --no-default-features"
```

Expected: `.deb` produced.

- [ ] **Step 3: Install + provision key**

```bash
ssh root@$DROPLET "apt install -y /root/aaOS/target/debian/aaos_0.0.0-1_amd64.deb"
ssh root@$DROPLET "echo 'DEEPSEEK_API_KEY=<KEY>' > /etc/default/aaos && chmod 0600 /etc/default/aaos && systemctl restart agentd"
ssh root@$DROPLET "usermod -aG aaos testop 2>/dev/null || adduser --disabled-password --gecos '' testop && usermod -aG aaos testop"
ssh root@$DROPLET "systemctl is-active agentd"
```

Expected: `active`.

- [ ] **Step 4: Trigger an escalation**

Submit a goal crafted to fail at tier 0 (writer role with a large output request that will exceed deepseek-chat's context or fail on first attempt):

```bash
ssh root@$DROPLET "sudo -u testop -g aaos agentd submit 'research the latest 30 papers on the arXiv category cs.AI, cross-reference their author affiliations with university rankings, produce a detailed 5000-word report comparing top-10-university research output, write to /tmp/report.md' 2>&1 | tee /tmp/submit-escalate.log | tail -30"
```

Expected: at least one line of the form `model escalated (<reason>): <subtask_id> — deepseek-chat → deepseek-reasoner` in the stream output.

- [ ] **Step 5: Verify audit trail**

```bash
ssh root@$DROPLET "grep -E 'model escalated|TTL expired|repeat guard' /tmp/submit-escalate.log | head -20"
```

Expected: one or more escalation events visible. If zero — the test goal didn't trigger a tier-0 failure; choose a harder prompt or add `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S=5` to force max_tokens/timeouts.

- [ ] **Step 6: Shred key, write reflection entry**

```bash
ssh root@$DROPLET "shred -u /etc/default/aaos"
```

Create `docs/reflection/YYYY-MM-DD-f-b2-e2e-qa.md` following the template in `docs/reflection/README.md`. Record: fresh droplet spec, artifact that shipped, observed escalation events, any surprises. Add one-line summary to `docs/reflection/README.md` index.

- [ ] **Step 7: Commit + push the reflection**

```bash
git add docs/reflection/
git commit -m "$(cat <<'EOF'
docs: reflection — F-b/2 dynamic-routing e2e on fresh droplet

First on-production exercise of the 11-commit dynamic-routing sub-
project. [Fill in: did escalation fire? which signal? any surprises?]

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
git push
```

---

## Self-review

**Spec coverage:**
- [x] `EscalationSignal` enum → Task 1.
- [x] `AuditEventKind::SubtaskModelEscalated` + `ToolRepeatGuardFired` → Task 2.
- [x] `Role.model_ladder` + `escalate_on` + `resolved_ladder()` → Task 3.
- [x] `Subtask.current_model_tier` field → Task 4.
- [x] `PerModelLatencyTracker` + `ModelSampleRing` → Task 5.
- [x] `ToolRepeatGuardFired` audit emit in `aaos-tools::ToolInvocation` → Task 6.
- [x] `FailedSubtask` + `decide_escalation` + `signals_for_subtask` + `carry_tiers_forward` → Task 7.
- [x] Executor wiring (tier at spawn + escalation at replan + `ExecutorError::Correctable` struct form) → Task 8.
- [x] Signal-disabled-does-not-escalate + ladder-top-caps tests → Task 9.
- [x] Operator-visibility for both new variants + `CompositeLatencyTracker` + agentd wiring → Task 10.
- [x] Role YAML declarations on writer + analyzer → Task 11.
- [x] Docs — roadmap + architecture with explicit scope → Task 12.
- [x] CI verification → Task 13.
- [x] Fresh-droplet re-verification (learned lesson from sub-project 1) → Task 14.

**Placeholder scan:** Step in Task 14 leaves `[Fill in: ...]` in the reflection commit message — that's an intentional template marker for the operator doing the run, not a plan failure.

**Type consistency:**
- `EscalationSignal`: Task 1 snake_case; Task 7 same variants used in decide_escalation; Task 10 operator-visibility arm uses `reason: String` from the audit event; consistent.
- `FailedSubtask { subtask_id, role, observed_signals }`: Task 7 defines, Task 8 populates in `execute_plan`, Task 7 `decide_escalation` reads `observed_signals`. Consistent.
- `role.resolved_ladder() -> Result<Vec<String>, String>`: Task 3 defines, Task 8 uses in spawn + escalation path with `unwrap_or_else(|_| vec![role.model.clone()])`. Consistent.
- `ExecutorError::Correctable { reason, failures }`: Task 8 defines the struct form, Task 8 also updates every call-site. `reason` remains the planner's replan prompt input — same string meaning as before.

Plan is internally consistent. Ready for implementation.

---

## Execution handoff

Plan complete and saved to `docs/phase-f-b2-plan.md`. Auto mode is active — proceeding to subagent-driven execution per sub-project 1's pattern.
