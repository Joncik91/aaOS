# Dynamic Model Routing — Role-Declared Ladder, Signal-Driven Escalation

**Status:** Draft (2026-04-18). Second sub-project of Phase F-b (Standard-spec completion).
**Scope:** Gap 2 from [`roadmap.md`](roadmap.md) Phase F-b. Gap 3 (worker-side tool confinement) remains deferred to its own spec.

## Goal

Let each role declare a **model ladder** instead of a single pinned model, and have the plan executor escalate up the ladder on three observable failure signals: planner replan-after-failure, tool-repeat guard firing, and `MaxTokens` stop reason. A task that a cheap model can complete on the first attempt keeps running on the cheap model; a task that exhibits evidence of being too hard walks one rung up to a stronger model on the next attempt.

A reader of the spec's "Dynamic Resource Allocation — cheap for mechanical, strong for reasoning, driven by live signals" should find shipped code — not role YAMLs that all pin `deepseek-chat`.

## Non-goals

- **No cost-aware routing.** Token→cost math requires provider-pricing tables that drift out of date, and `BudgetTracker` today guards token-usage runaway, not spend. This sub-project builds the observability infrastructure (`PerModelLatencyTracker`) but does not consume it for routing decisions. A future sub-project can add cost-aware routing once we have real distributions from production traffic.
- **No classifier-based routing.** Adding a separate "pick-a-model" LLM call would be a cost amplifier *and* a latency amplifier. The declarative ladder + signal escalation gives us the "cheap first, escalate on evidence" shape without a new inference step.
- **No cross-subtask state.** Each subtask starts at tier 0; escalation state is per-subtask, not per-agent or per-role-across-runs. A persistent model-preference store is a Phase G concern.
- **No per-provider model mixing in v1.** If a role's ladder is `[deepseek-chat, deepseek-reasoner]`, both tiers use the DeepSeek client. A role pinning `[deepseek-chat, claude-haiku-4-5]` *should work* because the LLM client is trait-polymorphic, but testing that cross-provider path is out of scope for v1 — document it as "works if the daemon is configured with both providers; we test single-provider ladders."

## Non-goals (carried forward from sub-project 1)

- No re-plan on model-tier exhaustion. If the top of the ladder also fails, behave exactly as today: `Correctable`, cascade to dependents.
- No mid-inference escalation. Tier decision is per-subtask-launch; the LLM call itself is atomic.

## Architecture

Three small additions on top of sub-project 1:

- **`Role.model_ladder: Vec<String>` + `Role.escalate_on: Vec<EscalationSignal>`** in `crates/aaos-runtime/src/plan/role.rs`. Both optional; missing fields default to `[role.model]` + all three signals enabled. Back-compat for every existing role YAML.
- **`Subtask.current_model_tier: u8`** in `crates/aaos-runtime/src/plan/mod.rs`. Default 0. Serializable. Planner sets to 0 on initial plan; executor increments on replan when any escalation signal fired during the failed attempt.
- **`PerModelLatencyTracker`** — a second `LatencyTracker` impl alongside `SubtaskWallClockTracker`, in `crates/aaos-runtime/src/scheduler/latency.rs`. Maintains per-model buckets of recent durations (bounded ring, ~256 samples), exposes `p50(model) -> Option<Duration>` + `p95(model) -> Option<Duration>` + `record(subtask_id, elapsed)` with the `model` looked up from a subtask→model map maintained by the `SchedulerView` when it's constructed.

One new audit variant + one existing-event extension:

- **`AuditEventKind::SubtaskModelEscalated { subtask_id, from_tier, to_tier, from_model, to_model, reason }`** in `crates/aaos-core/src/audit.rs`. Emitted when the replan loop bumps a subtask's tier.
- **`AuditEventKind::ToolRepeatGuardFired { agent_id, tool, attempt_count }`** — same file. The existing tool-repeat guard mutates a `_repeat_guard` hint into the LLM-visible tool result but emits no audit event; this sub-project adds one so the executor can count "did a repeat-guard fire during this subtask?" via the broadcast stream.

Data flow is otherwise unchanged from sub-project 1.

## Data flow

1. **Goal submission.** Same path as sub-project 1. Planner emits the DAG; each `Subtask` gets `current_model_tier: 0`.
2. **Subtask launch (first attempt).** `spawn_subtask` resolves the role, reads `role.model_ladder[0]` (falling back to `role.model` when ladder is absent or single-element), renders a manifest against that model, runs.
3. **Failure with signals.** One of the three signals fires during the subtask's execution:
   - Any `SubtaskCompleted { success: false }` in this subtask's audit stream.
   - Any `ToolRepeatGuardFired` with this subtask's agent_id.
   - Any `AgentExecutionCompleted { stop_reason: "MaxTokens" }` in this subtask's agent_id stream.
4. **Executor triggers replan** (existing behavior). Before handing back to the planner, the executor walks the list of failed subtasks and checks: did any configured escalation signal fire? If yes, increment `current_model_tier` up to `ladder.len() - 1`, emit `SubtaskModelEscalated`, continue.
5. **Replan produces updated plan.** Planner re-plans with the escalation context available in the replan prompt (the existing `prior_failures` structure, already part of the replan path from the 2026-04-17 replan work). Subtasks in the new plan inherit the escalated tier from their predecessors via id-matching — if a subtask id survives the replan, its `current_model_tier` carries over.
6. **Subtask relaunch.** Same flow as step 2, now with `ladder[current_model_tier]` — a different model.
7. **Latency observation (passive).** Regardless of escalation, `SchedulerView::complete` records elapsed-time against both trackers: the existing `SubtaskWallClockTracker` (keyed by subtask_id, used by TTL) AND the new `PerModelLatencyTracker` (keyed by model). No routing logic consumes `PerModelLatencyTracker` in v1 — it's observability infrastructure for a future sub-project.

## Components

### `EscalationSignal` enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationSignal {
    ReplanRetry,
    ToolRepeatGuard,
    MaxTokens,
}
```

Default set (when `escalate_on` is absent in YAML): all three.

### `Role` additions

```rust
pub struct Role {
    // ... existing fields ...
    pub model: String,   // UNCHANGED — tier 0, back-compat display field

    /// Ordered list of models to try for a subtask in this role. Tier 0 is
    /// always `role.model`; if `model_ladder` is absent or has a single
    /// element, routing is static. Missing = `[role.model]`.
    #[serde(default)]
    pub model_ladder: Vec<String>,

    /// Which observable signals trigger escalation to the next tier on replan.
    /// Missing = all three signals active.
    #[serde(default = "default_escalation_signals")]
    pub escalate_on: Vec<EscalationSignal>,
}

impl Role {
    /// Returns the ladder as a canonical Vec<String>, substituting
    /// [role.model] for missing/empty `model_ladder`. Also validates:
    /// if `model_ladder` is non-empty, its first element must equal
    /// `role.model` (they must be consistent; `role.model` is the display-
    /// only back-compat field). Returns an error string suitable for
    /// `RoleCatalog::load_from_dir` to surface.
    pub fn resolved_ladder(&self) -> Result<Vec<String>, String>;

    /// Tier-zero model — shortcut for `resolved_ladder()?[0]`, unless
    /// validation fails in which case returns role.model as a safe fallback.
    pub fn tier_zero_model(&self) -> &str { &self.model }
}
```

Rationale for the "first element equals `role.model`" invariant: `role.model` is the display-field operators expect to see in `agentd list` and audit events; having two sources of truth is a footgun. The validator surfaces any inconsistency at catalog-load time, not at spawn time.

### `Subtask.current_model_tier`

```rust
pub struct Subtask {
    // ... existing fields ...
    /// Ladder index for model routing. Planner sets to 0; executor
    /// increments up to ladder.len() - 1 on replan when an escalation
    /// signal fired during the failed attempt. Back-compat: missing
    /// deserializes to 0.
    #[serde(default)]
    pub current_model_tier: u8,
}
```

### `PerModelLatencyTracker`

```rust
pub struct PerModelLatencyTracker {
    /// Maps (subtask_id) → model. Populated by SchedulerView::new on wrap.
    subtask_models: DashMap<String, String>,

    /// Per-model bounded ring of recent durations. Oldest sample evicted
    /// when the ring is full. 256 samples per model is enough for stable
    /// p50/p95 over recent traffic without unbounded growth.
    samples: DashMap<String, ModelSampleRing>,
}

pub struct ModelSampleRing {
    samples: Vec<Duration>, // fixed capacity 256
    next_index: usize,
    full: bool,
}

impl PerModelLatencyTracker {
    pub fn new() -> Self;

    /// Called by SchedulerView::new to register the subtask→model mapping.
    pub fn register(&self, subtask_id: &str, model: &str);

    /// Returns the p50 of recent samples for this model. None if no data.
    pub fn p50(&self, model: &str) -> Option<Duration>;
    pub fn p95(&self, model: &str) -> Option<Duration>;
}

impl LatencyTracker for PerModelLatencyTracker {
    fn record(&self, subtask_id: &str, elapsed: Duration) {
        if let Some(model) = self.subtask_models.get(subtask_id) {
            self.samples
                .entry(model.clone())
                .or_insert_with(ModelSampleRing::new)
                .push(elapsed);
        }
    }

    fn wall_clock_elapsed(&self, subtask_id: &str) -> Duration {
        // This impl doesn't track per-subtask totals — that's SubtaskWallClockTracker.
        // Return ZERO for API completeness; callers wanting per-subtask must use
        // SubtaskWallClockTracker.
        Duration::ZERO
    }
}
```

The `Server` holds a **composite** tracker: `Vec<Arc<dyn LatencyTracker>>`. `SchedulerView::new` registers the subtask→model mapping with the composite before recording. Both the `SubtaskWallClockTracker` (for TTL) and the `PerModelLatencyTracker` (for observability) see every `record()`.

Alternative considered + rejected: make `LatencyTracker::record` take an explicit `model: &str` argument. Rejected because it would break sub-project 1's trait contract and require retrofitting the one existing impl. The subtask-to-model mapping via `register` is a bounded delta.

### Escalation-decision helper

```rust
/// Given a failed subtask and the set of audit events emitted during its
/// execution, decide whether to escalate and what reason to cite. Returns
/// None if no configured signal fired — the subtask replans at the same
/// tier.
fn decide_escalation(
    subtask: &Subtask,
    role: &Role,
    ladder: &[String],
    events_for_attempt: &[AuditEvent],
) -> Option<EscalationSignal>;
```

Pure function, testable in isolation. Called from the replan path in `PlanExecutor::execute_plan` after a failure, before the planner is re-invoked.

### Audit event variants

```rust
pub enum AuditEventKind {
    // ... existing ...
    SubtaskModelEscalated {
        subtask_id: String,
        from_tier: u8,
        to_tier: u8,
        from_model: String,
        to_model: String,
        /// Which signal fired. One of "replan_retry", "tool_repeat_guard", "max_tokens".
        reason: String,
    },
    ToolRepeatGuardFired {
        agent_id: AgentId,
        tool: String,
        attempt_count: u32,
    },
}
```

`SubtaskModelEscalated` added to `is_operator_visible` whitelist + formatter (learned lesson from sub-project 1's BUG #7). Default view shows: `model escalated (replan_retry): analyze — deepseek-chat → deepseek-reasoner`.

## Error handling

- **Ladder misconfigured.** `model_ladder[0] != role.model` → `RoleCatalog::load_from_dir` fails with a clear error naming the role and the two conflicting values. Same error shape as other YAML parse failures.
- **Tier exhausted.** `current_model_tier == ladder.len() - 1` and escalation signal fires → no further escalation; subtask replans at top tier. Log a tracing::warn noting "ladder exhausted for subtask {id}, staying at tier {n}". Not an error; the operator can widen the ladder if this becomes common.
- **Unknown signal string in YAML.** Standard serde deserialization error — catalog load fails with a specific line-level pointer.
- **Model name in ladder unknown to the LLM provider.** Surfaces at `complete()` time as an LLM provider error; same failure mode as today's misconfigured `role.model`. Not a new class of failure.
- **Concurrent escalation on sibling subtasks.** Each subtask's `current_model_tier` is independent. No shared state; no race.

## Testing

Unit tests in the new/touched modules:

1. **`Role::resolved_ladder`** — absent ladder returns `[role.model]`; explicit `[foo, bar]` with `role.model = foo` returns `[foo, bar]`; inconsistent `[bar, baz]` with `role.model = foo` errors.
2. **`EscalationSignal` serde** — roundtrip `replan_retry` / `tool_repeat_guard` / `max_tokens` as snake_case. Unknown string → error.
3. **`decide_escalation`** — happy cases: `SubtaskCompleted{success:false}` in the event stream triggers `ReplanRetry`; `ToolRepeatGuardFired` triggers `ToolRepeatGuard`; `AgentExecutionCompleted{stop_reason:"MaxTokens"}` triggers `MaxTokens`. Negative cases: configured-off signal does NOT trigger; top-of-ladder does NOT return a signal.
4. **`PerModelLatencyTracker`** — register + record two subtasks with different models; p50/p95 correct; ring eviction behavior (push 300 samples, expect only last 256 to influence p50); unregistered subtask_id is a no-op.
5. **`Subtask.current_model_tier` serde** — missing field deserializes to 0; explicit `"current_model_tier": 2` round-trips.

Integration tests in `crates/aaos-runtime/src/plan/executor.rs` tests module (same pattern as sub-project 1):

6. **`subtask_escalates_on_replan_retry`** — build a 2-tier role `[fast-stub, slow-stub]`, plan with one subtask, stub runner fails on tier 0, executor replans, stub runner succeeds on tier 1. Assert `SubtaskModelEscalated{from_model:"fast-stub", to_model:"slow-stub", reason:"replan_retry"}` fires exactly once.
7. **`subtask_does_not_escalate_when_signal_disabled`** — same setup but role declares `escalate_on: [max_tokens]` only. Failed subtask replans at tier 0 (no escalation). No `SubtaskModelEscalated` event.
8. **`subtask_escalation_caps_at_ladder_top`** — 2-tier role, subtask fails at tier 0 (escalates to 1), fails again at tier 1. Assert: replan happens, tier stays at 1, no second `SubtaskModelEscalated` event. Tracing warn line recorded (verify via `tracing_test` or env capture).

No live-API tests. All LLM client stubbing follows sub-project 1's pattern.

## Build sequence

1. **`EscalationSignal` type + serde + unit tests** — pure type, lands first.
2. **`AuditEventKind::SubtaskModelEscalated` + `::ToolRepeatGuardFired`** — audit variants + roundtrip tests.
3. **`Role.model_ladder` + `escalate_on` fields + `resolved_ladder()` helper** — YAML shape + validation + unit tests. Update all 4 existing role YAMLs to explicit `model_ladder: [deepseek-chat]` (optional — missing works too, explicit is more self-documenting).
4. **`Subtask.current_model_tier` field + serde** — standalone; backfill literal constructors.
5. **`PerModelLatencyTracker` impl** — new file alongside `scheduler/latency.rs`. Unit tests for register/record/p50/p95/eviction.
6. **`ToolRepeatGuardFired` audit emit** in `aaos-tools::ToolInvocation` — single line addition at the existing repeat-guard firing site. One test.
7. **`decide_escalation` helper** — pure function in `plan/executor.rs` or a sibling module. Unit tests.
8. **Executor wiring** — in `spawn_subtask`, consult `role.resolved_ladder()[subtask.current_model_tier]` for the model to inject into the manifest. In the replan path, call `decide_escalation` for every failed subtask + increment tier + emit audit.
9. **agentd integration** — `Server` holds a composite `Vec<Arc<dyn LatencyTracker>>` with both trackers. `SchedulerView::new` calls `register` on the `PerModelLatencyTracker` before recording. Operator-visible whitelist + formatter for `SubtaskModelEscalated`.
10. **docs/roadmap.md + docs/architecture.md** — mark Gap 2 shipped with honest scope notes (sub-project 1's BUG #4 lesson: name what's wrapped, name what isn't).
11. **Re-verify on fresh droplet** — `apt install` + a goal that intentionally fails at tier 0 and succeeds at tier 1 (role with ladder `[deepseek-chat, deepseek-reasoner]` and a prompt specifically crafted to exceed chat's output budget on first try). Success criterion: operator sees `model escalated (replan_retry): analyze — deepseek-chat → deepseek-reasoner` in the default CLI view, final output is produced.

Each step is one commit. Pattern mirrors sub-project 1. Touches three crates (`aaos-core`, `aaos-runtime`, `agentd`) + 4 role YAMLs.

## Configuration surface

**Env-driven:** no new env vars. Ladder + escalate_on are per-role YAML declarations.

**Role YAML example (writer.yaml, the role most likely to benefit):**

```yaml
name: writer
model: deepseek-chat
model_ladder:
  - deepseek-chat         # cheap, fine for most synthesis work
  - deepseek-reasoner     # escalate if the cheap tier gets stuck in a loop
                          # or runs out of output tokens
escalate_on:
  - replan_retry
  - tool_repeat_guard
  - max_tokens
system_prompt: "..."
# ... rest unchanged ...
```

A role that *doesn't* want escalation (e.g. `fetcher` — a scaffold, no LLM at all) keeps the current shape and works identically.

## What this spec does not address

- **Gap 3 (worker-side tool confinement).** Next sub-project; independent of this one.
- **Cost-aware routing.** Needs a `PricingTable` with per-provider per-model `$/token` rates and a decision rule. The `PerModelLatencyTracker` shipped here is the observability half; cost is the other half. Defer to a Phase F-b sub-project 4 or a Phase G item once we have real latency distributions to calibrate against.
- **Cross-provider ladders.** A ladder `[deepseek-chat, claude-haiku-4-5]` requires the daemon to have both provider clients configured. Works in theory; the trait is polymorphic. Not tested in this sub-project's acceptance criteria.
- **Persistent learning across runs.** Each run starts fresh at tier 0; no "this role tends to need tier 1, start there next time" memory. That's a learning-system feature, not a routing feature.
- **`SchedulerView` scope expansion.** Same as sub-project 1 — SchedulerView wraps only per-subtask LLM calls; Planner + Bootstrap still bypass it. That's a separate refactor, not part of this gap.

## Open questions

None that block implementation.

The biggest bet is **ladder[0] = role.model invariant**. If future experience shows operators want to switch tier-0 behavior without renaming the role, we'd relax this. For now, forbidding drift between the display field and the actual routed tier makes audit events easier to correlate.

The second bet is **three signals are enough**. If real traffic shows a common failure mode that none of them capture (e.g., "agent loops on the same tool without hitting the repeat-guard count threshold"), we add a signal — additive, no break.
