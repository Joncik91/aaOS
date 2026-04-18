# Reasoning-Slot Scheduler + Per-Task TTL Design

**Status:** Draft (2026-04-18). First sub-project of Phase F-b (Standard-spec completion).
**Scope:** Gaps 1 and 4 from [`roadmap.md`](../../roadmap.md) Phase F-b. Gaps 2 (dynamic model routing) and 3 (worker-side tool confinement) are deferred to their own specs.

## Goal

Replace `ScheduledLlmClient`'s plain semaphore with a priority-aware `ReasoningScheduler` that awards inference slots based on per-task TTL pressure, and introduce `TaskTtl` as a first-class resource on subtasks so runaway work is bounded without relying on the existing spawn-depth hack.

A reader of the spec's "Agent Kernel swaps reasoning attention between agents" claim and its "Max Hop / TTL counter for every task" safety primitive should find shipped code behind those words, not deferred ideas.

## Non-goals

- No mid-inference preemption. Slots are one LLM call long; the scheduler decides who gets the *next* one, not who gets to interrupt an in-flight request. Killing in-flight HTTP calls wastes tokens and fights the provider API.
- No per-model latency distributions. That's Gap 2 infrastructure. A `LatencyTracker` trait lands now with a minimal subtask-wall-clock impl; per-model p50/p95 stays stubbed.
- No re-plan on TTL expiry. Hard-kill the subtask and its dependents; mark the plan partial. Re-plan is unbounded and defeats the point of a TTL.
- No changes to role YAMLs that break existing deployments. New fields are optional; missing fields fall back to env defaults.

## Architecture

Three new types in a new `crates/aaos-runtime/src/scheduler/` module:

- `ReasoningScheduler` — owns a priority queue of pending `ReasoningRequest`s, a `Semaphore` of size `max_concurrent`, and the slot-handout loop. Replaces the role that `ScheduledLlmClient` plays today. One per `Server`.
- `SchedulerView` — per-subtask wrapper around the underlying `LlmClient`. Implements `LlmClient::complete` by first calling `scheduler.acquire_slot(subtask_id, priority, deadline).await`, then delegating. Built at subtask-runner construction time so the wrapped client already knows which subtask it's serving.
- `LatencyTracker` — trait with a minimal `SubtaskWallClockTracker` impl for Gap 4. Per-model aggregation stays behind the trait for Gap 2 to fill in. Records `complete()` durations; exposes `wall_clock_elapsed(subtask_id) -> Duration`.

One new core type in `crates/aaos-core`:

- `TaskTtl { max_hops: Option<u32>, max_wall_clock: Option<Duration> }` — both fields optional; a `TaskTtl` with both `None` means no TTL. Serde-serializable. Added as `Subtask.ttl: Option<TaskTtl>` so existing serialized plans deserialize cleanly.

Env-driven defaults, no new config file:

- `AAOS_MAX_CONCURRENT_INFERENCE` (already exists) — scheduler slot count.
- `AAOS_DEFAULT_TASK_TTL_HOPS` — default for `TaskTtl.max_hops` when a plan doesn't specify. Default: unset (= no limit).
- `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S` — default for `TaskTtl.max_wall_clock`. Default: unset.

Per-role priority override via optional `priority: u8` field in role YAML; missing field = default bucket.

## Data flow

1. **Goal submission.** `agent.submit_streaming` reaches the planner. Planner builds the DAG. Each `Subtask` gets `ttl: Option<TaskTtl>` — populated from the role's YAML or env defaults.
2. **Subtask launch.** `executor.spawn_subtask` decrements `max_hops` by 1 (if set); if it hits zero before the subtask starts, emit `SubtaskTtlExpired` audit event, mark the subtask failed, cascade to dependents. Otherwise, compute the wall-clock deadline (`now + max_wall_clock`) and pass both into the `SubtaskRunner` call.
3. **Subtask execution.** `SubtaskRunner` constructs a `SchedulerView` wrapping the real LLM client for this subtask. `SchedulerView` holds the `subtask_id`, the `priority` (from role YAML), and the `deadline`. Every `complete()` call routes through `scheduler.acquire_slot(...)` before hitting the real client.
4. **Slot handout.** `ReasoningScheduler::acquire_slot` pushes a `ReasoningRequest` into the priority queue, returns a `Future` that resolves when the scheduler awards a permit. Priority = deadline closeness (earlier deadlines go first); FIFO tiebreak; no-TTL requests go to the lowest bucket.
5. **TTL wall-clock enforcement.** When `SubtaskRunner` is spawned, a sibling watcher task is spawned with `tokio::select!` on `(subtask_completion_rx, tokio::time::sleep_until(deadline))`. If the sleep wins, the watcher cancels the subtask's join-handle, emits `SubtaskTtlExpired`, cascades to dependents.
6. **Latency tracking.** Every `SchedulerView::complete` call records the duration in the `LatencyTracker` keyed by `subtask_id`. Gap 2 will add per-model aggregation to the same tracker.

## Components

### `ReasoningScheduler`

```rust
pub struct ReasoningScheduler {
    slots: Arc<Semaphore>,
    queue: Arc<Mutex<BinaryHeap<ReasoningRequest>>>,
    dispatcher: JoinHandle<()>,
}

pub struct ReasoningRequest {
    subtask_id: SubtaskId,
    priority: u8,
    deadline: Option<Instant>,
    waker: oneshot::Sender<OwnedSemaphorePermit>,
}

impl ReasoningScheduler {
    pub fn new(max_concurrent: usize) -> Self;
    pub async fn acquire_slot(&self, subtask_id: SubtaskId, priority: u8, deadline: Option<Instant>) -> OwnedSemaphorePermit;
}
```

Dispatcher loop: `loop { permit = slots.acquire_owned().await; req = pop_highest_priority().await; req.waker.send(permit); }`. Priority ordering implemented via `BinaryHeap` with a custom `Ord` on `ReasoningRequest` (earliest deadline wins; no-deadline is Greatest so it sorts last).

### `SchedulerView`

```rust
pub struct SchedulerView {
    inner: Arc<dyn LlmClient>,
    scheduler: Arc<ReasoningScheduler>,
    latency: Arc<dyn LatencyTracker>,
    subtask_id: SubtaskId,
    priority: u8,
    deadline: Option<Instant>,
}

#[async_trait]
impl LlmClient for SchedulerView {
    async fn complete(&self, req: CompletionRequest) -> LlmResult<CompletionResponse> {
        let _permit = self.scheduler.acquire_slot(self.subtask_id, self.priority, self.deadline).await;
        let start = Instant::now();
        let result = self.inner.complete(req).await;
        self.latency.record(self.subtask_id, start.elapsed());
        result
    }
    fn max_context_tokens(&self, model: &str) -> u32 { self.inner.max_context_tokens(model) }
}
```

### `LatencyTracker` trait

```rust
pub trait LatencyTracker: Send + Sync {
    fn record(&self, subtask_id: SubtaskId, elapsed: Duration);
    fn wall_clock_elapsed(&self, subtask_id: SubtaskId) -> Duration;
}

pub struct SubtaskWallClockTracker {
    elapsed: DashMap<SubtaskId, Duration>,
}
impl LatencyTracker for SubtaskWallClockTracker { /* sum; lookup */ }
```

Gap 2 will add `PerModelLatencyTracker` that both implements `LatencyTracker` and exposes model-scoped queries. Both can coexist; the scheduler can hold a `Vec<Arc<dyn LatencyTracker>>` or a composite.

### `TaskTtl`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTtl {
    pub max_hops: Option<u32>,
    pub max_wall_clock: Option<Duration>,
}
```

On `Subtask`: `pub ttl: Option<TaskTtl>`. Serde optional; existing plans deserialize fine.

### TTL watcher

Spawned from `executor::spawn_subtask` alongside the subtask's join-handle:

```rust
tokio::spawn(async move {
    tokio::select! {
        _ = completion_rx => { /* subtask finished first; nothing to do */ }
        _ = tokio::time::sleep_until(deadline) => {
            abort_handle.abort();
            audit.emit(SubtaskTtlExpired { subtask_id, reason: "wall_clock_exceeded" });
            cascade_failure_to_dependents(subtask_id);
        }
    }
});
```

Only spawned if `deadline.is_some()`.

## Error handling

- **Slot-queue poisoning.** If the dispatcher task panics, the scheduler is wedged and all `acquire_slot` calls hang. Mitigation: the dispatcher is a simple loop with no fallible ops inside; any panic is a bug. Add a `#[tokio::test]` that asserts the dispatcher survives a `ReasoningRequest` with a dropped waker (common failure mode when a subtask is cancelled between `acquire_slot` push and permit handout).
- **Waker dropped before permit handed out.** If a subtask is cancelled between pushing its request and receiving the permit, the oneshot `send` fails. Dispatcher must handle this cleanly: discard the permit, loop back, pop the next request. Otherwise the permit leaks and the effective slot count drops over time.
- **TTL expiry races with completion.** Subtask finishes at the exact same tick as the deadline. `tokio::select!` is non-deterministic; either branch could win. Both paths must be idempotent: completion emits `SubtaskCompleted`, watcher emits `SubtaskTtlExpired` if it wins, neither fires twice. Achieved via oneshot-channel semantics (drop on first send).
- **Cascading failure with diamond dependencies.** If subtask C depends on both A and B, and A's TTL expires, C's dependency-on-A is failed but its dependency-on-B continues. Plan executor already handles partial-failure propagation for computed orchestration (commit `9b001cb` onward) — reuse that path.
- **No-TTL subtasks starving.** With priority = deadline-closeness, subtasks without TTLs always sort last. If all running tasks have TTLs, no-TTL subtasks never get a slot. Mitigation: no-TTL requests age — after N seconds in the queue, their effective priority rises to match deadline-closeness of a task due in M seconds. Simpler mitigation for v1: **no-TTL requests get a synthetic deadline of `now + AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S` or a hardcoded 60s if that's unset.** They still compete fairly.

## Testing

Unit tests in `scheduler/mod.rs`:

1. **Priority ordering.** Push three `ReasoningRequest`s with deadlines at t+10s, t+5s, t+20s. Assert they pop in order 5, 10, 20.
2. **FIFO tiebreak.** Two requests with the same deadline; first pushed comes first.
3. **No-TTL synthetic deadline.** A request with `deadline: None` still gets a slot in bounded time.
4. **Dropped waker.** Push a request, drop the receiver before the dispatcher pops it. Assert the dispatcher loops and pops the next request instead of hanging.
5. **`SchedulerView` records latency.** Stub `LlmClient` that sleeps 100ms. After one `complete()` call, `latency.wall_clock_elapsed(subtask_id) >= 100ms`.

Integration test in `crates/aaos-runtime/tests/ttl_integration.rs`:

6. **Hop expiry cascades.** Build a 3-subtask plan A → B → C with `max_hops=2`. Assert A runs, B runs, C is failed with `SubtaskTtlExpired` before launch.
7. **Wall-clock expiry kills running subtask.** Subtask with `max_wall_clock=500ms` and a stub `LlmClient` that sleeps 5s. Assert subtask fails with `SubtaskTtlExpired` between 500ms and 1s.
8. **Dependent cascade.** Same plan as (7) but with a dependent D on the timing-out subtask. Assert D is marked failed without running.

No live-API tests — everything uses stub `LlmClient`s to keep the suite fast and key-free.

## Build sequence

1. **`TaskTtl` type + serde** — pure types, small PR, lands first so downstream work has the shape to target.
2. **`LatencyTracker` trait + `SubtaskWallClockTracker`** — independent, unit-tested alone.
3. **`ReasoningScheduler` + `ReasoningRequest`** — core type, unit tests 1-4.
4. **`SchedulerView` replacing `ScheduledLlmClient`** in the agent services construction; unit test 5; `ScheduledLlmClient` stays in-crate for tests that don't want a scheduler.
5. **Plan executor threads TTL into `SubtaskRunner`**; integration test 6 (hop expiry, no wall-clock needed).
6. **TTL watcher task in `spawn_subtask`**; integration tests 7 and 8.
7. **Env + role-YAML config**; no new tests beyond parsing.
8. **Remove `ScheduledLlmClient` or leave as deprecated wrapper** — leave for now, drop in a follow-up once everything's on `SchedulerView`.

Each step is a commit; each commit compiles and tests pass. This is seven touches across three crates (`aaos-core`, `aaos-runtime`, `agentd`) — not small, but bounded.

## What this spec does not address

- **Gap 2 (dynamic model routing).** `LatencyTracker` is the trait; Gap 2's spec will add `PerModelLatencyTracker` and the router that reads it.
- **Gap 3 (worker-side tool confinement).** Entirely in the `NamespacedBackend` / broker plane. Separate spec.
- **Observability exports.** `SubtaskTtlExpired` audit events are emitted; a Prometheus histogram of latencies and a counter of TTL kills are nice-to-have but not on the critical path. Add if a reflection run shows they're needed.
- **Per-model priority.** Env-wide `max_concurrent` is one number. If the user submits tasks that hit different providers, a "cheap model has its own slot pool" argument exists; Gap 2 can address it via provider-specific pools if pressure arises.

## Open questions

None that block implementation. The biggest assumption — that "priority = deadline closeness" is the right heuristic — is validated by the TTL design itself: if TTLs are accurate, deadline-closeness is the right priority. If TTLs are missing, the synthetic-deadline fallback keeps things fair. A reflection run after this ships can tell us whether a different heuristic is worth it.
