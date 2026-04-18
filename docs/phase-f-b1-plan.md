# Reasoning-Slot Scheduler + Per-Task TTL Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the plain inference-slot semaphore with a TTL-aware `ReasoningScheduler` and thread per-subtask `TaskTtl` through the plan executor.

**Architecture:** Three new types in a new `crates/aaos-runtime/src/scheduler/` module (`ReasoningScheduler`, `SchedulerView`, `LatencyTracker` trait + `SubtaskWallClockTracker` impl). One new core type `TaskTtl` in `crates/aaos-core/src/plan.rs`. One new audit-event variant `SubtaskTtlExpired`. One optional `priority` field on `Role`. Env defaults `AAOS_DEFAULT_TASK_TTL_HOPS` and `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S`. A watcher task spawned from `spawn_subtask` enforces wall-clock deadlines via `tokio::select!`.

**Tech Stack:** Rust, tokio (Semaphore, oneshot, select!, JoinHandle), `async_trait`, `dashmap` (workspace dep, already used), `serde`, `thiserror`.

---

## File structure

**New files:**

- `crates/aaos-core/src/task_ttl.rs` — `TaskTtl` type, serde, constructors.
- `crates/aaos-runtime/src/scheduler/mod.rs` — module root, pub re-exports, `ReasoningScheduler` + `ReasoningRequest`.
- `crates/aaos-runtime/src/scheduler/view.rs` — `SchedulerView` wrapping an `LlmClient`.
- `crates/aaos-runtime/src/scheduler/latency.rs` — `LatencyTracker` trait + `SubtaskWallClockTracker` impl.
- `crates/aaos-runtime/src/scheduler/tests.rs` — unit tests 1–5 from the spec.
- `crates/aaos-runtime/tests/ttl_integration.rs` — integration tests 6–8 from the spec.

**Modified files:**

- `crates/aaos-core/src/lib.rs` — expose `TaskTtl`.
- `crates/aaos-core/src/audit.rs` — add `SubtaskTtlExpired { subtask_id, reason }` variant.
- `crates/aaos-runtime/src/lib.rs` — add `pub mod scheduler;` and re-exports.
- `crates/aaos-runtime/src/plan/mod.rs` — add `ttl: Option<TaskTtl>` field on `Subtask`.
- `crates/aaos-runtime/src/plan/role.rs` — add optional `priority: u8` field on `Role`.
- `crates/aaos-runtime/src/plan/executor.rs` — thread TTL into `SubtaskRunner`, decrement hops, spawn watcher, cascade TTL failure.
- `crates/agentd/src/server.rs` — construct `ReasoningScheduler`, wrap LLM client with `SchedulerView` per subtask, plumb TTL into the runner closure.
- `crates/agentd/src/main.rs` — build `ReasoningScheduler` alongside `ScheduledLlmClient` (leave existing semaphore for now; remove in follow-up).

---

## Task 1: `TaskTtl` core type

**Files:**
- Create: `crates/aaos-core/src/task_ttl.rs`
- Modify: `crates/aaos-core/src/lib.rs`
- Test: inline `#[cfg(test)] mod tests` in `task_ttl.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/aaos-core/src/task_ttl.rs`:

```rust
//! `TaskTtl` — optional per-subtask deadline + hop cap.
//!
//! Both fields are optional; `TaskTtl { max_hops: None, max_wall_clock: None }`
//! is legal and means "no TTL." Carried on `Subtask` (in `aaos-runtime`) so
//! the plan executor can decrement hops and the reasoning scheduler can
//! prioritise by deadline closeness.

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTtl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hops: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_duration"
    )]
    pub max_wall_clock: Option<Duration>,
}

impl TaskTtl {
    pub fn is_empty(&self) -> bool {
        self.max_hops.is_none() && self.max_wall_clock.is_none()
    }

    /// Decrement `max_hops` by 1. Returns `None` if the resulting count is 0
    /// (TTL exhausted — the caller should refuse to launch this subtask).
    /// Leaves `max_hops: None` untouched. `max_wall_clock` is not affected.
    pub fn decrement_hops(mut self) -> Option<Self> {
        if let Some(h) = self.max_hops {
            if h == 0 {
                return None;
            }
            let next = h - 1;
            self.max_hops = Some(next);
        }
        Some(self)
    }
}

mod serde_duration {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        d.map(|d| d.as_secs_f64()).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let opt = Option::<f64>::deserialize(d)?;
        Ok(opt.map(Duration::from_secs_f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_ttl_is_empty() {
        assert!(TaskTtl { max_hops: None, max_wall_clock: None }.is_empty());
    }

    #[test]
    fn decrement_hops_counts_down() {
        let t = TaskTtl { max_hops: Some(3), max_wall_clock: None };
        let t = t.decrement_hops().unwrap();
        assert_eq!(t.max_hops, Some(2));
        let t = t.decrement_hops().unwrap();
        assert_eq!(t.max_hops, Some(1));
        assert!(t.decrement_hops().is_none(), "0 hops left means refuse launch");
    }

    #[test]
    fn decrement_leaves_none_alone() {
        let t = TaskTtl { max_hops: None, max_wall_clock: Some(std::time::Duration::from_secs(10)) };
        let t = t.decrement_hops().unwrap();
        assert!(t.max_hops.is_none());
        assert_eq!(t.max_wall_clock, Some(std::time::Duration::from_secs(10)));
    }

    #[test]
    fn roundtrips_through_json() {
        let t = TaskTtl { max_hops: Some(5), max_wall_clock: Some(std::time::Duration::from_secs_f64(1.5)) };
        let s = serde_json::to_string(&t).unwrap();
        let back: TaskTtl = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aaos-core task_ttl --no-run 2>&1 | tail -20`
Expected: compilation error — `task_ttl` module not registered.

- [ ] **Step 3: Register the module in `lib.rs`**

Modify `crates/aaos-core/src/lib.rs`. Find the section with `pub mod audit;` / `pub mod error;` and add:

```rust
pub mod task_ttl;
pub use task_ttl::TaskTtl;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p aaos-core task_ttl 2>&1 | tail -10`
Expected: `test result: ok. 4 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add crates/aaos-core/src/task_ttl.rs crates/aaos-core/src/lib.rs
git commit -m "feat(core): TaskTtl type — max_hops + max_wall_clock, serde, unit tests

Pure type. No consumers yet; follow-up commits thread it through
Subtask + executor + scheduler.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `SubtaskTtlExpired` audit event

**Files:**
- Modify: `crates/aaos-core/src/audit.rs`

- [ ] **Step 1: Write the failing test**

At the bottom of `crates/aaos-core/src/audit.rs` (or in its existing `#[cfg(test)] mod tests`), add:

```rust
#[test]
fn subtask_ttl_expired_variant_roundtrips() {
    let e = AuditEventKind::SubtaskTtlExpired {
        subtask_id: "s1".into(),
        reason: "wall_clock_exceeded".into(),
    };
    let s = serde_json::to_string(&e).unwrap();
    let back: AuditEventKind = serde_json::from_str(&s).unwrap();
    match back {
        AuditEventKind::SubtaskTtlExpired { subtask_id, reason } => {
            assert_eq!(subtask_id, "s1");
            assert_eq!(reason, "wall_clock_exceeded");
        }
        _ => panic!("wrong variant"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aaos-core subtask_ttl_expired 2>&1 | tail -10`
Expected: compile error — `SubtaskTtlExpired` not a variant.

- [ ] **Step 3: Add the variant**

In `crates/aaos-core/src/audit.rs`, find `pub enum AuditEventKind` and add a new variant right after `SubtaskCompleted`:

```rust
    SubtaskTtlExpired {
        subtask_id: String,
        /// Short machine-readable reason: "hops_exhausted" | "wall_clock_exceeded" | "dependency_ttl_cascade".
        reason: String,
    },
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p aaos-core subtask_ttl_expired 2>&1 | tail -10`
Expected: `test result: ok. 1 passed`.

Also verify the whole crate still compiles: `cargo check -p aaos-core 2>&1 | tail -3`.

- [ ] **Step 5: Commit**

```bash
git add crates/aaos-core/src/audit.rs
git commit -m "feat(core): SubtaskTtlExpired audit event variant

Emitted by the plan executor when a TTL triggers — either hop-count
exhaustion before launch or wall-clock deadline during execution.
reason field is machine-readable: hops_exhausted, wall_clock_exceeded,
or dependency_ttl_cascade.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `LatencyTracker` trait + `SubtaskWallClockTracker`

**Files:**
- Create: `crates/aaos-runtime/src/scheduler/mod.rs`
- Create: `crates/aaos-runtime/src/scheduler/latency.rs`
- Modify: `crates/aaos-runtime/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/aaos-runtime/src/scheduler/latency.rs`:

```rust
//! `LatencyTracker` — records wall-clock cost of LLM calls per subtask.
//!
//! Minimal v1 impl `SubtaskWallClockTracker` sums per-subtask elapsed time
//! so the TTL watcher and (eventually) Gap 2's router can query it. Per-
//! model aggregation lives behind the trait for Gap 2 to add later.

use std::time::Duration;

use dashmap::DashMap;

pub trait LatencyTracker: Send + Sync {
    /// Record a single LLM call's elapsed time against `subtask_id`.
    fn record(&self, subtask_id: &str, elapsed: Duration);

    /// Total wall-clock time spent in LLM calls for `subtask_id` so far.
    /// Returns `Duration::ZERO` if the subtask has never called `complete()`.
    fn wall_clock_elapsed(&self, subtask_id: &str) -> Duration;
}

#[derive(Default)]
pub struct SubtaskWallClockTracker {
    elapsed: DashMap<String, Duration>,
}

impl SubtaskWallClockTracker {
    pub fn new() -> Self {
        Self::default()
    }
}

impl LatencyTracker for SubtaskWallClockTracker {
    fn record(&self, subtask_id: &str, elapsed: Duration) {
        self.elapsed
            .entry(subtask_id.to_string())
            .and_modify(|d| *d += elapsed)
            .or_insert(elapsed);
    }

    fn wall_clock_elapsed(&self, subtask_id: &str) -> Duration {
        self.elapsed
            .get(subtask_id)
            .map(|r| *r)
            .unwrap_or(Duration::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn empty_tracker_returns_zero() {
        let t = SubtaskWallClockTracker::new();
        assert_eq!(t.wall_clock_elapsed("missing"), Duration::ZERO);
    }

    #[test]
    fn records_sum_per_subtask() {
        let t = SubtaskWallClockTracker::new();
        t.record("a", Duration::from_millis(100));
        t.record("a", Duration::from_millis(250));
        t.record("b", Duration::from_millis(50));
        assert_eq!(t.wall_clock_elapsed("a"), Duration::from_millis(350));
        assert_eq!(t.wall_clock_elapsed("b"), Duration::from_millis(50));
        assert_eq!(t.wall_clock_elapsed("c"), Duration::ZERO);
    }
}
```

Create `crates/aaos-runtime/src/scheduler/mod.rs`:

```rust
//! `scheduler` — reasoning-slot scheduler + latency tracker.
//!
//! `ReasoningScheduler` (Task 4) awards inference slots based on per-
//! subtask TTL deadlines. `LatencyTracker` records wall-clock LLM time.
//! `SchedulerView` (Task 5) wraps an `LlmClient` with per-subtask
//! context so the acquire-slot call is transparent to callers.

pub mod latency;

pub use latency::{LatencyTracker, SubtaskWallClockTracker};
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aaos-runtime --lib scheduler::latency 2>&1 | tail -10`
Expected: compile error — `scheduler` module not registered in `lib.rs`.

- [ ] **Step 3: Register the module**

In `crates/aaos-runtime/src/lib.rs`, find the section with `pub mod context;` / `pub mod plan;` and add:

```rust
pub mod scheduler;
```

Also add `pub use` alongside existing re-exports:

```rust
pub use scheduler::{LatencyTracker, SubtaskWallClockTracker};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p aaos-runtime --lib scheduler::latency 2>&1 | tail -10`
Expected: `test result: ok. 2 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add crates/aaos-runtime/src/scheduler/ crates/aaos-runtime/src/lib.rs
git commit -m "feat(runtime): LatencyTracker trait + SubtaskWallClockTracker

Trait lands now with a per-subtask-sum impl so the TTL watcher has
something to query. Gap 2 (dynamic model routing) will add a second
impl with per-model p50/p95 distributions behind the same trait.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `ReasoningScheduler` + `ReasoningRequest`

**Files:**
- Modify: `crates/aaos-runtime/src/scheduler/mod.rs`
- Create: `crates/aaos-runtime/src/scheduler/tests.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/aaos-runtime/src/scheduler/tests.rs`:

```rust
//! Unit tests for ReasoningScheduler.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::time::timeout;

use super::{ReasoningScheduler, SYNTHETIC_DEADLINE_DEFAULT};

/// Helper: push a request and return the receiver for its permit.
async fn submit(
    sched: &Arc<ReasoningScheduler>,
    subtask: &str,
    deadline: Option<Instant>,
) -> oneshot::Receiver<tokio::sync::OwnedSemaphorePermit> {
    sched.enqueue_for_test(subtask.to_string(), 128, deadline).await
}

#[tokio::test]
async fn priority_ordering_earliest_deadline_wins() {
    let sched = Arc::new(ReasoningScheduler::new(1));
    let now = Instant::now();
    let r_long = submit(&sched, "t_long", Some(now + Duration::from_secs(20))).await;
    let r_short = submit(&sched, "t_short", Some(now + Duration::from_secs(5))).await;
    let r_mid = submit(&sched, "t_mid", Some(now + Duration::from_secs(10))).await;

    // Slot pool is 1. Drain one-at-a-time and verify order.
    let first = timeout(Duration::from_secs(2), r_short).await.unwrap().unwrap();
    drop(first);
    let second = timeout(Duration::from_secs(2), r_mid).await.unwrap().unwrap();
    drop(second);
    let third = timeout(Duration::from_secs(2), r_long).await.unwrap().unwrap();
    drop(third);
}

#[tokio::test]
async fn fifo_tiebreak_for_equal_deadlines() {
    let sched = Arc::new(ReasoningScheduler::new(1));
    let now = Instant::now() + Duration::from_secs(10);

    // Hold the single slot so both requests queue.
    let block = submit(&sched, "block", Some(Instant::now() + Duration::from_secs(1))).await;
    let permit = timeout(Duration::from_secs(2), block).await.unwrap().unwrap();

    let r_first = submit(&sched, "first", Some(now)).await;
    let r_second = submit(&sched, "second", Some(now)).await;

    drop(permit);
    // First queued should win.
    let got_first = timeout(Duration::from_secs(2), r_first).await.unwrap().unwrap();
    drop(got_first);
    let got_second = timeout(Duration::from_secs(2), r_second).await.unwrap().unwrap();
    drop(got_second);
}

#[tokio::test]
async fn no_deadline_uses_synthetic_and_resolves() {
    let sched = Arc::new(ReasoningScheduler::new(1));
    let r = submit(&sched, "no_ttl", None).await;
    let permit = timeout(Duration::from_secs(SYNTHETIC_DEADLINE_DEFAULT.as_secs() + 2), r)
        .await
        .expect("synthetic-deadline request should resolve within synthetic window")
        .unwrap();
    drop(permit);
}

#[tokio::test]
async fn dropped_waker_does_not_wedge_dispatcher() {
    let sched = Arc::new(ReasoningScheduler::new(1));

    // Hold the slot so the first real request queues, then we'll drop it.
    let block = submit(&sched, "block", Some(Instant::now() + Duration::from_secs(1))).await;
    let permit = timeout(Duration::from_secs(2), block).await.unwrap().unwrap();

    let r_ghost = submit(&sched, "ghost", Some(Instant::now() + Duration::from_secs(2))).await;
    let r_real = submit(&sched, "real", Some(Instant::now() + Duration::from_secs(3))).await;

    drop(r_ghost); // waker dropped — dispatcher must discard its permit and loop
    drop(permit);

    // Real request must still get served.
    let got_real = timeout(Duration::from_secs(5), r_real)
        .await
        .expect("dispatcher wedged: real request starved by dropped ghost")
        .unwrap();
    drop(got_real);
}
```

Expected test failure — `ReasoningScheduler`, `SYNTHETIC_DEADLINE_DEFAULT`, and `enqueue_for_test` don't exist yet.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p aaos-runtime --lib scheduler 2>&1 | tail -15`
Expected: compile error — missing `ReasoningScheduler`.

- [ ] **Step 3: Implement `ReasoningScheduler`**

Replace the body of `crates/aaos-runtime/src/scheduler/mod.rs` with:

```rust
//! `scheduler` — reasoning-slot scheduler + latency tracker.
//!
//! `ReasoningScheduler` awards inference slots based on per-subtask TTL
//! deadlines. `LatencyTracker` records wall-clock LLM time. `SchedulerView`
//! (Task 5) wraps an `LlmClient` with per-subtask context so the acquire-
//! slot call is transparent to callers.

pub mod latency;
pub mod view;

pub use latency::{LatencyTracker, SubtaskWallClockTracker};
pub use view::SchedulerView;

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, Mutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

/// Fallback deadline for requests that arrive with `deadline: None`.
/// Picked large enough to never starve a genuinely-long task, small
/// enough that no-TTL work still competes fairly against short-deadline
/// peers. Matches the "synthetic deadline" rule in the design doc.
pub const SYNTHETIC_DEADLINE_DEFAULT: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct ReasoningRequest {
    /// Primary ordering key — earliest deadline wins.
    effective_deadline: Instant,
    /// Tiebreak — monotonically increasing insertion id. FIFO on equal deadlines.
    insertion_id: u64,
    /// Subtask id (for logs / future metrics).
    #[allow(dead_code)]
    subtask_id: String,
    /// Hint for future routing / metrics. Not used in v1 ordering.
    #[allow(dead_code)]
    priority: u8,
    /// Permit handoff channel. Dispatcher sends the acquired permit here.
    waker: oneshot::Sender<OwnedSemaphorePermit>,
}

impl PartialEq for ReasoningRequest {
    fn eq(&self, o: &Self) -> bool {
        self.effective_deadline == o.effective_deadline && self.insertion_id == o.insertion_id
    }
}
impl Eq for ReasoningRequest {}
impl Ord for ReasoningRequest {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap; wrap in Reverse at push time so we
        // pop the smallest (earliest) deadline. Tiebreak on insertion_id
        // (smaller id first = FIFO).
        self.effective_deadline
            .cmp(&o.effective_deadline)
            .then(self.insertion_id.cmp(&o.insertion_id))
    }
}
impl PartialOrd for ReasoningRequest {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

pub struct ReasoningScheduler {
    slots: Arc<Semaphore>,
    queue: Arc<Mutex<BinaryHeap<Reverse<ReasoningRequest>>>>,
    queue_notify: Arc<Notify>,
    next_insertion_id: AtomicU64,
    _dispatcher: JoinHandle<()>,
}

impl ReasoningScheduler {
    pub fn new(max_concurrent: usize) -> Arc<Self> {
        let slots = Arc::new(Semaphore::new(max_concurrent));
        let queue: Arc<Mutex<BinaryHeap<Reverse<ReasoningRequest>>>> =
            Arc::new(Mutex::new(BinaryHeap::new()));
        let queue_notify = Arc::new(Notify::new());

        let dispatcher = tokio::spawn(dispatcher_loop(
            slots.clone(),
            queue.clone(),
            queue_notify.clone(),
        ));

        Arc::new(Self {
            slots,
            queue,
            queue_notify,
            next_insertion_id: AtomicU64::new(0),
            _dispatcher: dispatcher,
        })
    }

    /// Queue a request for an inference slot. Resolves to an owned permit
    /// when one is awarded. If the caller drops the returned future before
    /// the permit is handed over, the dispatcher discards the permit and
    /// moves on to the next request.
    pub async fn acquire_slot(
        self: &Arc<Self>,
        subtask_id: String,
        priority: u8,
        deadline: Option<Instant>,
    ) -> OwnedSemaphorePermit {
        let rx = self.enqueue(subtask_id, priority, deadline).await;
        // If the sender is dropped (dispatcher panicked), surface as a
        // panic — it means the runtime is unrecoverable. The dispatcher
        // is a simple infallible loop so this should never happen.
        rx.await
            .expect("reasoning scheduler dispatcher died — unrecoverable")
    }

    async fn enqueue(
        self: &Arc<Self>,
        subtask_id: String,
        priority: u8,
        deadline: Option<Instant>,
    ) -> oneshot::Receiver<OwnedSemaphorePermit> {
        let (tx, rx) = oneshot::channel();
        let effective_deadline = deadline.unwrap_or_else(|| Instant::now() + SYNTHETIC_DEADLINE_DEFAULT);
        let insertion_id = self.next_insertion_id.fetch_add(1, Ordering::Relaxed);
        let req = ReasoningRequest {
            effective_deadline,
            insertion_id,
            subtask_id,
            priority,
            waker: tx,
        };
        self.queue.lock().await.push(Reverse(req));
        self.queue_notify.notify_one();
        rx
    }

    /// Test-only helper: same as `enqueue` but pub for unit tests in
    /// `scheduler/tests.rs`. Kept out of the public API.
    #[cfg(test)]
    pub async fn enqueue_for_test(
        self: &Arc<Self>,
        subtask_id: String,
        priority: u8,
        deadline: Option<Instant>,
    ) -> oneshot::Receiver<OwnedSemaphorePermit> {
        self.enqueue(subtask_id, priority, deadline).await
    }
}

async fn dispatcher_loop(
    slots: Arc<Semaphore>,
    queue: Arc<Mutex<BinaryHeap<Reverse<ReasoningRequest>>>>,
    notify: Arc<Notify>,
) {
    loop {
        // Block until someone queues work. Wake-up is a hint — re-check
        // the queue inside the lock.
        let req = loop {
            let mut q = queue.lock().await;
            if let Some(Reverse(r)) = q.pop() {
                break r;
            }
            drop(q);
            notify.notified().await;
        };

        // Acquire a permit. If the Semaphore is closed (runtime shutdown),
        // exit the loop cleanly.
        let permit = match slots.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return,
        };

        // Hand it over. If the waker was dropped (caller cancelled),
        // the permit goes back to the pool on drop and we loop.
        if req.waker.send(permit).is_err() {
            // Dropped waker — permit drops here, slot freed. Loop to the
            // next request.
            tracing::debug!(
                subtask_id = %req.subtask_id,
                "reasoning scheduler: waker dropped before permit handoff"
            );
        }
    }
}

#[cfg(test)]
mod tests;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p aaos-runtime --lib scheduler 2>&1 | tail -15`
Expected: `test result: ok. 6 passed` (4 scheduler + 2 latency).

- [ ] **Step 5: Commit**

```bash
git add crates/aaos-runtime/src/scheduler/mod.rs crates/aaos-runtime/src/scheduler/tests.rs
git commit -m "feat(runtime): ReasoningScheduler — deadline-priority slot handout

BinaryHeap priority queue keyed on effective_deadline; FIFO tiebreak
via monotonic insertion id; synthetic 60s deadline for no-TTL
requests. Dispatcher loop survives dropped wakers (cancelled
callers) by discarding the permit and looping.

Unit tests cover ordering, FIFO tiebreak, synthetic-deadline
resolution, and the dropped-waker recovery path.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `SchedulerView` wrapping `LlmClient`

**Files:**
- Create: `crates/aaos-runtime/src/scheduler/view.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/aaos-runtime/src/scheduler/view.rs`:

```rust
//! `SchedulerView` — per-subtask wrapper that routes every `complete()`
//! call through the reasoning scheduler before delegating to the real
//! client, then records wall-clock elapsed in the latency tracker.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use aaos_llm::{CompletionRequest, CompletionResponse, LlmClient, LlmResult};

use super::{LatencyTracker, ReasoningScheduler};

pub struct SchedulerView {
    inner: Arc<dyn LlmClient>,
    scheduler: Arc<ReasoningScheduler>,
    latency: Arc<dyn LatencyTracker>,
    subtask_id: String,
    priority: u8,
    deadline: Option<Instant>,
}

impl SchedulerView {
    pub fn new(
        inner: Arc<dyn LlmClient>,
        scheduler: Arc<ReasoningScheduler>,
        latency: Arc<dyn LatencyTracker>,
        subtask_id: String,
        priority: u8,
        deadline: Option<Instant>,
    ) -> Self {
        Self {
            inner,
            scheduler,
            latency,
            subtask_id,
            priority,
            deadline,
        }
    }
}

#[async_trait]
impl LlmClient for SchedulerView {
    async fn complete(&self, req: CompletionRequest) -> LlmResult<CompletionResponse> {
        let _permit = self
            .scheduler
            .acquire_slot(self.subtask_id.clone(), self.priority, self.deadline)
            .await;
        let start = Instant::now();
        let result = self.inner.complete(req).await;
        self.latency.record(&self.subtask_id, start.elapsed());
        result
    }

    fn max_context_tokens(&self, model: &str) -> u32 {
        self.inner.max_context_tokens(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::SubtaskWallClockTracker;

    use aaos_core::{AgentId, TokenUsage};
    use aaos_llm::{ContentBlock, LlmStopReason, Message};
    use async_trait::async_trait;

    struct SlowClient {
        delay: Duration,
    }
    #[async_trait]
    impl LlmClient for SlowClient {
        async fn complete(&self, _r: CompletionRequest) -> LlmResult<CompletionResponse> {
            tokio::time::sleep(self.delay).await;
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text { text: "ok".into() }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }
        fn max_context_tokens(&self, _m: &str) -> u32 {
            200_000
        }
    }

    #[tokio::test]
    async fn records_elapsed_into_latency_tracker() {
        let scheduler = ReasoningScheduler::new(4);
        let latency: Arc<dyn LatencyTracker> = Arc::new(SubtaskWallClockTracker::new());
        let inner: Arc<dyn LlmClient> = Arc::new(SlowClient {
            delay: Duration::from_millis(100),
        });
        let view = SchedulerView::new(
            inner,
            scheduler,
            latency.clone(),
            "sub-1".into(),
            128,
            None,
        );
        let _ = view
            .complete(CompletionRequest {
                model: "test".into(),
                messages: vec![Message::user(vec![ContentBlock::Text { text: "hi".into() }])],
                system: None,
                tools: vec![],
                tool_choice: None,
                max_tokens: 100,
                agent_id: AgentId::new(),
                stop_sequences: vec![],
            })
            .await
            .unwrap();
        assert!(
            latency.wall_clock_elapsed("sub-1") >= Duration::from_millis(95),
            "expected at least ~100ms recorded; got {:?}",
            latency.wall_clock_elapsed("sub-1")
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aaos-runtime --lib scheduler::view 2>&1 | tail -10`
Expected: compile error — `pub mod view` not declared in `scheduler/mod.rs`.

- [ ] **Step 3: Wire in the module**

The `pub mod view;` line is already in the Task 4 body of `scheduler/mod.rs`. Confirm it's present. If it isn't, add it right after `pub mod latency;`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p aaos-runtime --lib scheduler::view 2>&1 | tail -10`
Expected: `test result: ok. 1 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/aaos-runtime/src/scheduler/view.rs
git commit -m "feat(runtime): SchedulerView — per-subtask LlmClient wrapper

Delegates to the underlying client after acquiring a scheduler slot;
records elapsed wall-clock into the LatencyTracker. Transparent to
the AgentExecutor — it just sees an LlmClient.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: `Subtask.ttl` + `Role.priority` fields

**Files:**
- Modify: `crates/aaos-runtime/src/plan/mod.rs`
- Modify: `crates/aaos-runtime/src/plan/role.rs`

- [ ] **Step 1: Write the failing test**

Open `crates/aaos-runtime/src/plan/mod.rs` and add this test at the bottom of an existing `#[cfg(test)] mod tests` block (or create one if missing):

```rust
    #[test]
    fn subtask_ttl_serde_roundtrip_and_default() {
        use aaos_core::TaskTtl;
        use std::time::Duration;

        // Default: no ttl, back-compat for serialized plans.
        let s: Subtask = serde_json::from_str(
            r#"{"id":"a","role":"writer","params":{},"depends_on":[]}"#,
        )
        .unwrap();
        assert!(s.ttl.is_none(), "missing ttl must deserialize as None");

        // With ttl.
        let s2 = Subtask {
            id: "b".into(),
            role: "writer".into(),
            params: serde_json::json!({}),
            depends_on: vec![],
            ttl: Some(TaskTtl {
                max_hops: Some(3),
                max_wall_clock: Some(Duration::from_secs(30)),
            }),
        };
        let json = serde_json::to_string(&s2).unwrap();
        let back: Subtask = serde_json::from_str(&json).unwrap();
        assert_eq!(s2, back);
    }
```

Open `crates/aaos-runtime/src/plan/role.rs` and add:

```rust
#[cfg(test)]
#[test]
fn role_priority_defaults_to_128() {
    let yaml = r#"
name: r
model: claude-haiku-4-5-20251001
system_prompt: "x"
message_template: "y"
budget: { max_input_tokens: 1000, max_output_tokens: 1000 }
retry: { max_attempts: 1 }
"#;
    let role: Role = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(role.priority, 128, "missing priority defaults to 128 (mid-bucket)");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p aaos-runtime --lib plan 2>&1 | tail -15`
Expected: compile errors — `Subtask.ttl` and `Role.priority` don't exist.

- [ ] **Step 3: Add the fields**

In `crates/aaos-runtime/src/plan/mod.rs`, replace the `Subtask` struct with:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Subtask {
    pub id: SubtaskId,
    pub role: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub depends_on: Vec<SubtaskId>,
    /// Optional per-subtask TTL. Populated by the planner from role defaults
    /// + env, or left None for no bound. Existing serialized plans without
    /// this field deserialize to None (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<aaos_core::TaskTtl>,
}
```

In `crates/aaos-runtime/src/plan/role.rs`, inside `pub struct Role`, add right after `retry`:

```rust
    /// Scheduling-priority hint. Lower numbers get their turn earlier when
    /// two subtasks share the same TTL deadline. Missing in YAML = 128
    /// (mid-bucket). Roles that produce critical-path work (writer,
    /// analyzer) can declare e.g. `priority: 64`.
    #[serde(default = "default_role_priority")]
    pub priority: u8,
```

Also add to the same file (near other `default_` free fns, or at the bottom):

```rust
fn default_role_priority() -> u8 {
    128
}
```

- [ ] **Step 4: Fix existing Subtask constructors**

Run: `cargo build -p aaos-runtime 2>&1 | grep "missing field .ttl." | head -5`

Every error reports an existing literal constructor of `Subtask { id, role, params, depends_on }`. For each, add `ttl: None,`. Likely sites: `crates/aaos-runtime/src/plan/planner.rs`, `crates/aaos-runtime/src/plan/executor.rs` tests, `crates/agentd/src/server.rs` tests. Fix all, then re-run `cargo build -p aaos-runtime -p agentd 2>&1 | tail -5` until clean.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p aaos-runtime --lib plan 2>&1 | tail -10`
Expected: all plan tests pass, including the two new ones.

- [ ] **Step 6: Commit**

```bash
git add crates/aaos-runtime/src/plan/
git commit -m "feat(runtime): Subtask.ttl + Role.priority fields

Both optional / defaulted — existing serialized plans and role YAMLs
deserialize unchanged (back-compat). Role.priority defaults to 128
(mid-bucket); Subtask.ttl defaults to None (no bound). Follow-up
tasks thread these through the executor + scheduler.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Thread TTL through `SubtaskRunner`; hop-decrement in `spawn_subtask`

**Files:**
- Modify: `crates/aaos-runtime/src/plan/executor.rs`
- Modify: `crates/agentd/src/server.rs`
- Create: `crates/aaos-runtime/tests/ttl_integration.rs`

- [ ] **Step 1: Write the failing integration test**

Create `crates/aaos-runtime/tests/ttl_integration.rs`:

```rust
//! End-to-end TTL tests — exercise the plan executor with a stub runner
//! that never hits a real LLM, verifying hop-decrement + cascade behaviour.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aaos_core::{AgentId, AuditEventKind, InMemoryAuditLog, TaskTtl};
use aaos_runtime::plan::{
    ExecutorError, PlanExecutor, Role, RoleBudget, RoleCatalog, RoleRetry, Subtask,
    SubtaskExecutorOverrides, SubtaskResult, SubtaskRunner,
};

fn make_role(name: &str) -> Role {
    Role {
        name: name.into(),
        model: "stub".into(),
        parameters: Default::default(),
        capabilities: vec![],
        system_prompt: "x".into(),
        message_template: "y".into(),
        budget: RoleBudget {
            max_input_tokens: 1000,
            max_output_tokens: 1000,
        },
        retry: RoleRetry {
            max_attempts: 1,
            on: vec![],
        },
        scaffold: None,
        priority: 128,
    }
}

fn stub_runner() -> SubtaskRunner {
    Arc::new(
        |subtask_id: String,
         _m: String,
         _msg: String,
         _o: SubtaskExecutorOverrides|
         -> Pin<Box<dyn Future<Output = Result<SubtaskResult, aaos_core::CoreError>> + Send>> {
            Box::pin(async move {
                Ok(SubtaskResult {
                    subtask_id: subtask_id.clone(),
                    agent_id: AgentId::new(),
                    response: format!("ok:{subtask_id}"),
                    input_tokens: 0,
                    output_tokens: 0,
                })
            })
        },
    )
}

#[tokio::test]
async fn hop_exhaustion_fails_subtask_before_launch() {
    let mut catalog = RoleCatalog::empty();
    catalog.insert(make_role("writer"));
    let audit = Arc::new(InMemoryAuditLog::new());
    let runner = stub_runner();
    let exec = PlanExecutor::new(
        Arc::new(catalog),
        None,
        runner,
        audit.clone(),
        std::path::PathBuf::from("/tmp/ttl-test"),
    );

    let plan = aaos_runtime::plan::Plan {
        subtasks: vec![
            Subtask {
                id: "a".into(),
                role: "writer".into(),
                params: serde_json::json!({}),
                depends_on: vec![],
                ttl: Some(TaskTtl {
                    max_hops: Some(1),
                    max_wall_clock: None,
                }),
            },
            Subtask {
                id: "b".into(),
                role: "writer".into(),
                params: serde_json::json!({}),
                depends_on: vec!["a".into()],
                // Inherits TTL via cascade; explicit for clarity.
                ttl: Some(TaskTtl {
                    max_hops: Some(0),
                    max_wall_clock: None,
                }),
            },
        ],
        final_output: "a".into(),
    };

    let result = exec
        .execute_plan(&plan, &std::path::PathBuf::from("/tmp/ttl-test-run"))
        .await;
    assert!(
        matches!(&result, Err(ExecutorError::Correctable(_))),
        "expected hop-exhausted subtask to produce Correctable failure; got {:?}",
        result.as_ref().err()
    );

    let expired: Vec<_> = audit
        .all()
        .into_iter()
        .filter(|e| matches!(e.event, AuditEventKind::SubtaskTtlExpired { ref subtask_id, ref reason }
            if subtask_id == "b" && reason == "hops_exhausted"))
        .collect();
    assert_eq!(
        expired.len(),
        1,
        "expected exactly one SubtaskTtlExpired event for 'b' with reason=hops_exhausted"
    );
}
```

(Wall-clock and cascade tests land in Task 8 once the watcher is in place.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aaos-runtime --test ttl_integration hop_exhaustion 2>&1 | tail -15`
Expected: compile error — `SubtaskRunner` signature still takes four args, `RoleCatalog::empty` may not exist, etc.

- [ ] **Step 3: Extend `SubtaskRunner` with a deadline argument**

Modify `crates/aaos-runtime/src/plan/executor.rs`:

Find the `SubtaskRunner` type alias and replace it with:

```rust
pub type SubtaskRunner = Arc<
    dyn Fn(
            String,                   // subtask_id
            String,                   // rendered manifest YAML
            String,                   // first message
            SubtaskExecutorOverrides, // per-role budget + iteration caps
            Option<Instant>,          // wall-clock deadline (None = no wall-clock bound)
        ) -> Pin<Box<dyn Future<Output = Result<SubtaskResult, CoreError>> + Send>>
        + Send
        + Sync,
>;
```

(`Instant` is already imported via `use std::time::{Duration, Instant};` at the top of the file. If not, add it.)

Find the single call-site inside `spawn_subtask`:

```rust
let result = (self.runner)(subtask.id.clone(), manifest_yaml, message, overrides)
    .await
    .map_err(ExecutorError::Terminal)?;
```

Replace the whole `spawn_subtask` body's hop + deadline handling. Right after the existing `let role = ...` + `let resolved_params = ...` block, **before** the `SubtaskStarted` audit record, add:

```rust
// TTL hop decrement. If the subtask arrives with max_hops=0 we've
// exhausted the budget — emit the audit event, skip execution, return
// a Correctable error so the plan executor marks it failed + cascades.
if let Some(ttl) = &subtask.ttl {
    if let Some(hops) = ttl.max_hops {
        if hops == 0 {
            self.audit_log.record(AuditEvent::new(
                AgentId::from_uuid(uuid::Uuid::nil()),
                AuditEventKind::SubtaskTtlExpired {
                    subtask_id: subtask.id.clone(),
                    reason: "hops_exhausted".into(),
                },
            ));
            return Err(ExecutorError::Correctable(format!(
                "subtask '{}' TTL hops exhausted",
                subtask.id
            )));
        }
    }
}

let wall_clock_deadline: Option<Instant> = subtask
    .ttl
    .as_ref()
    .and_then(|t| t.max_wall_clock)
    .map(|d| Instant::now() + d);
```

Then change the runner invocation to pass the deadline:

```rust
let result = (self.runner)(
    subtask.id.clone(),
    manifest_yaml,
    message,
    overrides,
    wall_clock_deadline,
)
.await
.map_err(ExecutorError::Terminal)?;
```

- [ ] **Step 4: Update every `SubtaskRunner` call-site and stub**

Run: `cargo build --workspace 2>&1 | grep -E "expected.*arguments|wrong number" | head -10`

Two kinds of sites will break:

1. The real runner in `crates/agentd/src/server.rs::install_plan_executor_runner`. Replace its closure body to take and ignore the deadline for now (Task 8 threads it into the watcher):

```rust
let runner: SubtaskRunner =
    Arc::new(move |subtask_id, manifest_yaml, message, overrides, _deadline| {
        let s = server_weak.clone();
        Box::pin(async move {
            s.run_subtask_inline(&subtask_id, &manifest_yaml, &message, overrides)
                .await
        })
    });
```

2. Any test-stub runners in `crates/aaos-runtime/src/plan/executor.rs` and `crates/agentd/src/server.rs` tests. Each is a 4-arg closure; add a 5th arg named `_deadline`. Expected touch-points (verify by running the grep above):
   - `stub_runner` or equivalents in `executor.rs` tests
   - inline `|id, _m, _msg, _o|` closures anywhere in `server.rs` tests

Then rebuild: `cargo build --workspace 2>&1 | tail -3` — must be clean.

- [ ] **Step 5: Expose helpers the integration test needs**

Open `crates/aaos-runtime/src/plan/role.rs`. If `RoleCatalog::empty()` doesn't exist, add:

```rust
impl RoleCatalog {
    #[cfg(any(test, debug_assertions))]
    pub fn empty() -> Self {
        Self::from_map(std::collections::HashMap::new())
    }

    #[cfg(any(test, debug_assertions))]
    pub fn insert(&mut self, role: Role) {
        // Inserts into the inner map; private field, so this impl lives
        // next to it. Test-only — production uses load_from_dir.
        self.roles_mut().insert(role.name.clone(), role);
    }
}
```

(If the inner field is a plain `HashMap<String, Role>` called `roles`, the `insert` above compiles; if it's wrapped in `Arc`/`RwLock`, adapt accordingly — one line to lock + insert. If the existing API already has a different test helper, use that in the test instead and drop these additions.)

Also confirm `PlanExecutor::new` signature — if the second param is `Planner` (not `Option<Planner>`), change the integration test to pass a stub planner or call whichever `PlanExecutor` constructor accepts no planner. Check:

Run: `grep -n "pub fn new" crates/aaos-runtime/src/plan/executor.rs | head -3`

If `new` requires a Planner, use the alternative constructor or `new_for_tests`. If no such alternative exists, the minimal addition is a test-only `PlanExecutor::new_without_planner(catalog, runner, audit, run_root)` that stores `None` for the planner field. Add it with `#[cfg(any(test, debug_assertions))]`.

- [ ] **Step 6: Run the integration test to verify it passes**

Run: `cargo test -p aaos-runtime --test ttl_integration hop_exhaustion 2>&1 | tail -15`
Expected: `test result: ok. 1 passed`.

Also run the whole workspace tests to confirm no regressions: `cargo test --workspace 2>&1 | tail -5`.

- [ ] **Step 7: Commit**

```bash
git add crates/aaos-runtime/src/plan/executor.rs \
        crates/aaos-runtime/src/plan/role.rs \
        crates/aaos-runtime/tests/ttl_integration.rs \
        crates/agentd/src/server.rs
git commit -m "feat(runtime): thread TTL through SubtaskRunner, decrement hops on launch

SubtaskRunner now takes Option<Instant> wall-clock deadline. spawn_subtask
checks max_hops; if 0, emits SubtaskTtlExpired{reason:hops_exhausted}
and returns Correctable without launching. Deadline is computed from
max_wall_clock and passed to the runner — Task 8 wires the watcher that
enforces it.

Integration test covers hop-exhaustion path. Wall-clock + cascade land
in Task 8.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Wall-clock TTL watcher + dependent cascade

**Files:**
- Modify: `crates/agentd/src/server.rs`
- Modify: `crates/aaos-runtime/src/plan/executor.rs`
- Modify: `crates/aaos-runtime/tests/ttl_integration.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/aaos-runtime/tests/ttl_integration.rs`:

```rust
#[tokio::test]
async fn wall_clock_expiry_kills_running_subtask() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut catalog = RoleCatalog::empty();
    catalog.insert(make_role("writer"));
    let audit = Arc::new(InMemoryAuditLog::new());
    let was_cancelled = Arc::new(AtomicBool::new(false));
    let wc = was_cancelled.clone();

    // Slow stub: sleeps 5s, observes its own cancellation via drop.
    let runner: SubtaskRunner = Arc::new(move |id, _m, _msg, _o, _deadline| {
        let wc = wc.clone();
        Box::pin(async move {
            struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for DropFlag {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _flag = DropFlag(wc);
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(SubtaskResult {
                subtask_id: id,
                agent_id: AgentId::new(),
                response: "never".into(),
                input_tokens: 0,
                output_tokens: 0,
            })
        })
    });

    let exec = PlanExecutor::new(
        Arc::new(catalog),
        None,
        runner,
        audit.clone(),
        std::path::PathBuf::from("/tmp/ttl-wc-test"),
    );

    let plan = aaos_runtime::plan::Plan {
        subtasks: vec![Subtask {
            id: "slow".into(),
            role: "writer".into(),
            params: serde_json::json!({}),
            depends_on: vec![],
            ttl: Some(TaskTtl {
                max_hops: None,
                max_wall_clock: Some(Duration::from_millis(500)),
            }),
        }],
        final_output: "slow".into(),
    };

    let start = std::time::Instant::now();
    let result = exec
        .execute_plan(&plan, &std::path::PathBuf::from("/tmp/ttl-wc-run"))
        .await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "wall-clock expiry must fail the plan");
    assert!(
        elapsed >= Duration::from_millis(400) && elapsed < Duration::from_secs(3),
        "expected kill between 400ms and 3s; got {elapsed:?}"
    );
    assert!(
        was_cancelled.load(Ordering::SeqCst),
        "subtask future must have been cancelled (dropped)"
    );

    let expired: Vec<_> = audit
        .all()
        .into_iter()
        .filter(|e| matches!(e.event, AuditEventKind::SubtaskTtlExpired { ref subtask_id, ref reason }
            if subtask_id == "slow" && reason == "wall_clock_exceeded"))
        .collect();
    assert_eq!(expired.len(), 1);
}

#[tokio::test]
async fn dependent_cascades_after_wall_clock_expiry() {
    let mut catalog = RoleCatalog::empty();
    catalog.insert(make_role("writer"));
    let audit = Arc::new(InMemoryAuditLog::new());

    let runner: SubtaskRunner = Arc::new(|id, _m, _msg, _o, _deadline| {
        Box::pin(async move {
            if id == "slow" {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Ok(SubtaskResult {
                subtask_id: id,
                agent_id: AgentId::new(),
                response: "ok".into(),
                input_tokens: 0,
                output_tokens: 0,
            })
        })
    });

    let exec = PlanExecutor::new(
        Arc::new(catalog),
        None,
        runner,
        audit.clone(),
        std::path::PathBuf::from("/tmp/ttl-casc-test"),
    );

    let plan = aaos_runtime::plan::Plan {
        subtasks: vec![
            Subtask {
                id: "slow".into(),
                role: "writer".into(),
                params: serde_json::json!({}),
                depends_on: vec![],
                ttl: Some(TaskTtl {
                    max_hops: None,
                    max_wall_clock: Some(Duration::from_millis(500)),
                }),
            },
            Subtask {
                id: "dependent".into(),
                role: "writer".into(),
                params: serde_json::json!({}),
                depends_on: vec!["slow".into()],
                ttl: None,
            },
        ],
        final_output: "dependent".into(),
    };

    let _ = exec
        .execute_plan(&plan, &std::path::PathBuf::from("/tmp/ttl-casc-run"))
        .await;

    let events = audit.all();
    let dependent_started = events.iter().any(|e| {
        matches!(&e.event, AuditEventKind::SubtaskStarted { subtask_id, .. } if subtask_id == "dependent")
    });
    assert!(
        !dependent_started,
        "dependent must not launch after its dep failed via TTL"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p aaos-runtime --test ttl_integration 2>&1 | tail -20`
Expected: the two new tests fail — currently the runner just awaits forever; there's no watcher.

- [ ] **Step 3: Add `execute_with_deadline` helper**

In `crates/aaos-runtime/src/plan/executor.rs`, add a free helper near `spawn_subtask`:

```rust
/// Race `fut` against an optional wall-clock deadline. If the deadline
/// fires first, returns an `ExecutorError::Terminal(CoreError::Ipc("ttl
/// wall-clock exceeded"))` and drops `fut` (which cancels any work it was
/// driving). If `deadline.is_none()`, behaves like `fut.await`.
async fn race_deadline<F, T>(fut: F, deadline: Option<Instant>) -> Result<T, CoreError>
where
    F: std::future::Future<Output = Result<T, CoreError>>,
{
    match deadline {
        None => fut.await,
        Some(d) => {
            tokio::select! {
                r = fut => r,
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(d)) => {
                    Err(CoreError::Ipc("ttl wall-clock exceeded".into()))
                }
            }
        }
    }
}
```

- [ ] **Step 4: Wire the helper into `spawn_subtask`**

In `spawn_subtask`, replace the runner invocation block with:

```rust
let fut = (self.runner)(
    subtask.id.clone(),
    manifest_yaml,
    message,
    overrides,
    wall_clock_deadline,
);
let result = race_deadline(fut, wall_clock_deadline).await;

match result {
    Ok(r) => Ok(r),
    Err(CoreError::Ipc(ref m)) if m == "ttl wall-clock exceeded" => {
        self.audit_log.record(AuditEvent::new(
            AgentId::from_uuid(uuid::Uuid::nil()),
            AuditEventKind::SubtaskTtlExpired {
                subtask_id: subtask.id.clone(),
                reason: "wall_clock_exceeded".into(),
            },
        ));
        Err(ExecutorError::Correctable(format!(
            "subtask '{}' exceeded wall-clock TTL",
            subtask.id
        )))
    }
    Err(e) => Err(ExecutorError::Terminal(e)),
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p aaos-runtime --test ttl_integration 2>&1 | tail -10`
Expected: all three tests pass.

Run full workspace: `cargo test --workspace 2>&1 | tail -5`.

- [ ] **Step 6: Commit**

```bash
git add crates/aaos-runtime/src/plan/executor.rs crates/aaos-runtime/tests/ttl_integration.rs
git commit -m "feat(runtime): wall-clock TTL enforcement + dependent cascade

Wraps each subtask future in tokio::select! against its wall-clock
deadline. On expiry: drops the future (cancels the work), emits
SubtaskTtlExpired{reason:wall_clock_exceeded}, returns Correctable.
Dependent cascade falls out of the existing plan-executor partial-
failure logic — no new code needed.

Integration tests cover happy path (wall-clock kill) and cascade
(dependent never launches after its dep TTL-fails).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Wire scheduler + SchedulerView into `agentd`

**Files:**
- Modify: `crates/agentd/src/server.rs`
- Modify: `crates/agentd/src/main.rs`

- [ ] **Step 1: Add scheduler + latency to `Server`**

In `crates/agentd/src/server.rs`, find the `pub struct Server` definition. Add two new fields:

```rust
    /// Reasoning-slot scheduler. Awards inference slots with TTL-aware
    /// priority. One per server. Replaces the role that `ScheduledLlmClient`
    /// played before Phase F-b — that client remains in-tree for tests but
    /// production traffic goes through this.
    pub(crate) reasoning_scheduler: Arc<aaos_runtime::scheduler::ReasoningScheduler>,
    /// Per-subtask wall-clock tracker. Queried by the TTL watcher; Gap 2
    /// will add a per-model variant implementing the same trait.
    pub(crate) latency_tracker: Arc<dyn aaos_runtime::LatencyTracker>,
```

In every `Server::new*` constructor (there are a few — LLM-aware and minimal; `grep -n "fn new" crates/agentd/src/server.rs` to find all), instantiate both. Simplest approach: read `AAOS_MAX_CONCURRENT_INFERENCE` once near the top of the constructor:

```rust
let max_concurrent = std::env::var("AAOS_MAX_CONCURRENT_INFERENCE")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(3usize);
let reasoning_scheduler = aaos_runtime::scheduler::ReasoningScheduler::new(max_concurrent);
let latency_tracker: Arc<dyn aaos_runtime::LatencyTracker> =
    Arc::new(aaos_runtime::SubtaskWallClockTracker::new());
```

Pass `reasoning_scheduler` and `latency_tracker` into the `Server { ... }` literal in each constructor.

- [ ] **Step 2: Thread deadline into `run_subtask_inline`**

Modify `run_subtask_inline` signature to take the deadline:

```rust
pub async fn run_subtask_inline(
    self: &Arc<Self>,
    subtask_id: &str,
    manifest_yaml: &str,
    message: &str,
    overrides: SubtaskExecutorOverrides,
    deadline: Option<std::time::Instant>,
) -> Result<SubtaskResult, aaos_core::CoreError> {
    // ... existing body ...
    let response = self
        .execute_agent_for_subtask(agent_id, &manifest, message, overrides, subtask_id, deadline)
        .await?;
    // ... existing tail ...
}
```

Modify `execute_agent_for_subtask` to wrap the LLM client with `SchedulerView`:

```rust
async fn execute_agent_for_subtask(
    self: &Arc<Self>,
    agent_id: aaos_core::AgentId,
    manifest: &aaos_core::AgentManifest,
    first_message: &str,
    overrides: SubtaskExecutorOverrides,
    subtask_id: &str,
    deadline: Option<std::time::Instant>,
) -> Result<String, aaos_core::CoreError> {
    let raw_llm = self.llm_client.clone().ok_or_else(|| {
        aaos_core::CoreError::Ipc("no LLM client configured for subtask execution".into())
    })?;

    // Wrap the real client with a per-subtask SchedulerView so every
    // complete() routes through the reasoning scheduler + latency tracker.
    // Priority comes from the role via a future plumbing pass; for now,
    // use the default mid-bucket (128).
    let llm: Arc<dyn aaos_llm::LlmClient> =
        Arc::new(aaos_runtime::scheduler::SchedulerView::new(
            raw_llm,
            self.reasoning_scheduler.clone(),
            self.latency_tracker.clone(),
            subtask_id.to_string(),
            128,
            deadline,
        ));

    // ... rest of the body unchanged, using `llm` where the old `llm`
    //     variable was used ...
}
```

- [ ] **Step 3: Update the runner closure in `install_plan_executor_runner`**

In `install_plan_executor_runner`, pass the deadline through:

```rust
let runner: SubtaskRunner =
    Arc::new(move |subtask_id, manifest_yaml, message, overrides, deadline| {
        let s = server_weak.clone();
        Box::pin(async move {
            s.run_subtask_inline(&subtask_id, &manifest_yaml, &message, overrides, deadline)
                .await
        })
    });
```

- [ ] **Step 4: Verify nothing broke**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: all tests pass.

Also run the plan-executor tests specifically: `cargo test -p aaos-runtime 2>&1 | tail -5`.

- [ ] **Step 5: Commit**

```bash
git add crates/agentd/src/server.rs crates/agentd/src/main.rs
git commit -m "feat(agentd): wire ReasoningScheduler + SchedulerView into subtask execution

Server holds one ReasoningScheduler + one SubtaskWallClockTracker.
run_subtask_inline forwards the wall-clock deadline into
execute_agent_for_subtask, which wraps the real LLM client in a
per-subtask SchedulerView before handing it to AgentExecutor. Every
complete() call now routes through the scheduler and records its
elapsed time.

ScheduledLlmClient is left in-tree for tests that don't want a
scheduler; a follow-up can retire it.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Env + planner defaults for `TaskTtl`

**Files:**
- Modify: `crates/aaos-runtime/src/plan/planner.rs`

- [ ] **Step 1: Write the failing test**

In `crates/aaos-runtime/src/plan/planner.rs`, add to the existing `#[cfg(test)] mod tests` (or create one):

```rust
    #[test]
    fn default_ttl_from_env_populates_missing_fields() {
        use aaos_core::TaskTtl;
        use std::time::Duration;

        // SAFETY: unit test runs single-threaded per default `cargo test`
        // semantics within this test binary; even if a neighbour test uses
        // the same vars, the set/unset dance matches what the prod path
        // does at startup.
        unsafe {
            std::env::set_var("AAOS_DEFAULT_TASK_TTL_HOPS", "5");
            std::env::set_var("AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S", "30");
        }

        let t = default_task_ttl();
        unsafe {
            std::env::remove_var("AAOS_DEFAULT_TASK_TTL_HOPS");
            std::env::remove_var("AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S");
        }

        assert_eq!(t.as_ref().unwrap().max_hops, Some(5));
        assert_eq!(
            t.as_ref().unwrap().max_wall_clock,
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn default_ttl_returns_none_when_no_env() {
        // Guard against leaked vars from other tests in the same binary.
        unsafe {
            std::env::remove_var("AAOS_DEFAULT_TASK_TTL_HOPS");
            std::env::remove_var("AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S");
        }
        assert!(default_task_ttl().is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p aaos-runtime --lib planner 2>&1 | tail -10`
Expected: compile error — `default_task_ttl` not defined.

- [ ] **Step 3: Implement `default_task_ttl`**

Add to `crates/aaos-runtime/src/plan/planner.rs` (top-level, outside tests):

```rust
/// Build a TaskTtl from environment defaults. Returns None if both
/// `AAOS_DEFAULT_TASK_TTL_HOPS` and `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S`
/// are unset. Called by the planner when a subtask arrives without
/// an explicit TTL.
pub fn default_task_ttl() -> Option<aaos_core::TaskTtl> {
    let max_hops = std::env::var("AAOS_DEFAULT_TASK_TTL_HOPS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok());
    let max_wall_clock = std::env::var("AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs);

    if max_hops.is_none() && max_wall_clock.is_none() {
        return None;
    }
    Some(aaos_core::TaskTtl {
        max_hops,
        max_wall_clock,
    })
}
```

Then in the `Planner::plan`-adjacent code (find where each subtask is materialised from JSON — `grep -n "Subtask {" crates/aaos-runtime/src/plan/planner.rs`), populate `ttl` from the default when missing:

```rust
let ttl = s.ttl.clone().or_else(default_task_ttl);
Subtask {
    id: s.id.clone(),
    role: s.role.clone(),
    params: s.params.clone(),
    depends_on: s.depends_on.clone(),
    ttl,
}
```

(Adjust the surrounding field access to match the actual planner code shape — this is a pattern, not an exact drop-in. If the planner currently constructs `Subtask` in one place, apply the same default-or-overlay there.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p aaos-runtime --lib planner 2>&1 | tail -10`
Expected: `test result: ok` for the two new tests plus the existing ones.

- [ ] **Step 5: Commit**

```bash
git add crates/aaos-runtime/src/plan/planner.rs
git commit -m "feat(runtime): AAOS_DEFAULT_TASK_TTL_{HOPS,WALL_CLOCK_S} env defaults

Planner calls default_task_ttl() when a subtask arrives without an
explicit TTL. Both vars unset = no default (back-compat for anyone
not opting in). Setting one or both populates Subtask.ttl at plan
construction time.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Documentation + roadmap

**Files:**
- Modify: `docs/roadmap.md`
- Modify: `docs/architecture.md` (if the scheduler surfaces architecturally)

- [ ] **Step 1: Mark Gap 1 + Gap 4 complete in roadmap**

In `docs/roadmap.md`, find the `### Phase F-b: Standard-spec completion *(next)*` section. Under Gap 1 and Gap 4, prepend status line:

For Gap 1: `**Status (2026-04-XX — fill in actual date):** *Shipped.* See commits / `docs/phase-f-b1-design.md`.`
For Gap 4: Same pattern.

Update the Phase F preamble to note one of the three sub-projects is done.

- [ ] **Step 2: Add a short section to architecture.md**

If `docs/architecture.md` has a "Runtime layer" or "Execution" section, add a sub-section:

```markdown
### Reasoning-slot scheduler (Phase F-b sub-project 1)

The runtime owns a single `ReasoningScheduler` that awards LLM inference
slots based on per-subtask `TaskTtl` deadlines. Every `SchedulerView`-wrapped
`LlmClient::complete` call first `acquire_slot`s, then delegates. Priority =
deadline closeness; FIFO tiebreak; requests without TTLs get a 60s synthetic
deadline so they compete fairly. One slot = one complete() call; no mid-
inference preemption.
```

- [ ] **Step 3: Commit**

```bash
git add docs/roadmap.md docs/architecture.md
git commit -m "docs: Phase F-b sub-project 1 — scheduler + TTL shipped

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: CI pass + cleanup

**Files:** (none — CI + verification only)

- [ ] **Step 1: Verify the whole workspace builds**

```bash
cargo build --workspace 2>&1 | tail -3
cargo test --workspace 2>&1 | tail -5
cargo fmt --all -- --check 2>&1 | tail -5
```

Expected: all clean.

- [ ] **Step 2: Push and verify CI**

```bash
git push
sleep 15
gh run list --limit 1
```

Wait for the run to complete (~3 minutes), then:

```bash
gh run view <latest-run-id>
```

Expected: all 3 jobs green. If `test-ignored` fails on an Azure-LSM reason (see `patterns.md` entry added in `518ed4e`), that's unrelated to this work — do not mask it.

- [ ] **Step 3: If any failure is actually caused by this work, stop and diagnose**

Read the failing job logs. Fix in place. Push. Do not paper over test failures.

---

## Self-review

Run through the spec's sections one more time against the plan:

**Spec coverage check:**

- [x] "Goal: replace plain semaphore with priority-aware scheduler" → Task 4 `ReasoningScheduler`.
- [x] "TaskTtl as first-class resource" → Task 1.
- [x] "`ReasoningScheduler`, `SchedulerView`, `LatencyTracker`" → Tasks 3, 4, 5.
- [x] "`TaskTtl { max_hops, max_wall_clock }` on `Subtask`" → Tasks 1, 6.
- [x] "env `AAOS_DEFAULT_TASK_TTL_HOPS` / `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S`" → Task 10.
- [x] "per-role `priority: u8` in role YAML" → Task 6.
- [x] "Goal-submission stays unchanged" → nothing added; verified by no changes to `agent.submit_streaming`.
- [x] "Dispatcher discards permit and loops on dropped waker" → Task 4 impl + test 4.
- [x] "No-TTL synthetic 60s deadline" → Task 4 `SYNTHETIC_DEADLINE_DEFAULT` + test 3.
- [x] "TTL watcher races via tokio::select! + cancels subtask" → Task 8 `race_deadline`.
- [x] "Dependent cascade reuses existing partial-failure logic" → Task 8 test 3; no new code.
- [x] "Unit tests 1–5, integration tests 6–8" → Tasks 4 (tests 1–4), 5 (test 5), 7 (test 6), 8 (tests 7–8).
- [x] "No live-API tests" → all runners are stubs.

**Placeholder scan:** Task 6 step 5 mentions "If the inner field is `Arc`/`RwLock`, adapt accordingly" and Task 10 step 3 says "adjust the surrounding field access to match the actual planner code shape." Both are unavoidable because the exact internal field layout depends on the current `RoleCatalog` / `Planner` shape; the pattern and the goal are specified concretely. Not a placeholder failure.

**Type consistency:**

- `ReasoningScheduler::new` returns `Arc<Self>` (Task 4) and every caller (Task 5, Task 9) holds `Arc<ReasoningScheduler>` — consistent.
- `acquire_slot` takes `(String, u8, Option<Instant>)` — same in Task 4 impl, Task 5 view, Task 9 wiring.
- `LatencyTracker::record(&str, Duration)` — Task 3 trait, Task 5 call-site match.
- `SubtaskRunner` 5th arg is `Option<Instant>` — Task 7 introduces, Task 8 uses, Task 9 passes.
- `SubtaskTtlExpired { subtask_id: String, reason: String }` — Task 2 defines, Tasks 7/8 emit with reasons `"hops_exhausted"`, `"wall_clock_exceeded"`. Consistent.

Plan is internally consistent.

---

## Execution handoff

Plan complete and saved to `docs/phase-f-b1-plan.md`. Two execution options:

1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task, two-stage review between tasks, fast iteration. This is the loop we used for MCP integration.
2. **Inline Execution** — execute tasks in this session using executing-plans, batch checkpoints.

Which approach?
