//! `scheduler` — reasoning-slot scheduler + latency tracker.
//!
//! `ReasoningScheduler` (Task 4) awards inference slots based on per-
//! subtask TTL deadlines. `LatencyTracker` records wall-clock LLM time.
//! `SchedulerView` (Task 5) wraps an `LlmClient` with per-subtask
//! context so the acquire-slot call is transparent to callers.

use aaos_core::AgentId;

pub mod latency;
pub mod view;

pub use latency::{LatencyTracker, SubtaskWallClockTracker};
pub use view::SchedulerView;

/// Priority level for agent scheduling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low = 0,
    #[default]
    Normal = 1,
    High = 2,
    Critical = 3,
}

/// A unit of work to be scheduled.
#[derive(Debug)]
pub struct ScheduleEntry {
    pub agent_id: AgentId,
    pub priority: Priority,
}

/// Trait for agent schedulers.
///
/// The scheduler determines which agent gets inference time next.
/// Initial implementation is round-robin with priority support.
pub trait Scheduler: Send + Sync {
    /// Add an agent to the schedule.
    fn enqueue(&self, entry: ScheduleEntry);

    /// Remove an agent from the schedule.
    fn dequeue(&self, agent_id: &AgentId);

    /// Get the next agent that should run.
    fn next(&self) -> Option<AgentId>;
}

/// Simple round-robin scheduler with priority support.
///
/// Higher-priority agents get more frequent turns.
pub struct RoundRobinScheduler {
    queue: std::sync::Mutex<Vec<ScheduleEntry>>,
}

impl RoundRobinScheduler {
    pub fn new() -> Self {
        Self {
            queue: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl Default for RoundRobinScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler for RoundRobinScheduler {
    fn enqueue(&self, entry: ScheduleEntry) {
        let mut queue = self.queue.lock().unwrap();
        queue.push(entry);
        // Sort by priority (highest first)
        queue.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    fn dequeue(&self, agent_id: &AgentId) {
        let mut queue = self.queue.lock().unwrap();
        queue.retain(|e| e.agent_id != *agent_id);
    }

    fn next(&self) -> Option<AgentId> {
        let mut queue = self.queue.lock().unwrap();
        if queue.is_empty() {
            return None;
        }
        // Take the first (highest priority) and rotate to back
        let entry = queue.remove(0);
        let id = entry.agent_id;
        queue.push(entry);
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_robin_basic() {
        let scheduler = RoundRobinScheduler::new();
        let a = AgentId::new();
        let b = AgentId::new();

        scheduler.enqueue(ScheduleEntry {
            agent_id: a,
            priority: Priority::Normal,
        });
        scheduler.enqueue(ScheduleEntry {
            agent_id: b,
            priority: Priority::Normal,
        });

        let first = scheduler.next().unwrap();
        let second = scheduler.next().unwrap();
        assert_ne!(first, second);

        // After full rotation, first should come back
        let third = scheduler.next().unwrap();
        assert_eq!(first, third);
    }

    #[test]
    fn priority_ordering() {
        let scheduler = RoundRobinScheduler::new();
        let low = AgentId::new();
        let high = AgentId::new();

        scheduler.enqueue(ScheduleEntry {
            agent_id: low,
            priority: Priority::Low,
        });
        scheduler.enqueue(ScheduleEntry {
            agent_id: high,
            priority: Priority::High,
        });

        assert_eq!(scheduler.next().unwrap(), high);
    }

    #[test]
    fn dequeue_removes() {
        let scheduler = RoundRobinScheduler::new();
        let a = AgentId::new();
        scheduler.enqueue(ScheduleEntry {
            agent_id: a,
            priority: Priority::Normal,
        });
        scheduler.dequeue(&a);
        assert!(scheduler.next().is_none());
    }
}

// ============================================================================
// ReasoningScheduler — Phase F-b sub-project 1 addition.
// Awards LLM inference slots based on per-subtask TTL deadlines.
// ============================================================================

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
    #[allow(dead_code)]
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
        let rx = self.enqueue(subtask_id.clone(), priority, deadline).await;
        // If the sender is dropped (dispatcher panicked), surface as a
        // panic — it means the runtime is unrecoverable. The dispatcher
        // is a simple infallible loop so this should never happen.
        rx.await.unwrap_or_else(|_| {
            panic!("reasoning scheduler dispatcher died — unrecoverable (subtask_id={subtask_id})")
        })
    }

    async fn enqueue(
        self: &Arc<Self>,
        subtask_id: String,
        priority: u8,
        deadline: Option<Instant>,
    ) -> oneshot::Receiver<OwnedSemaphorePermit> {
        let (tx, rx) = oneshot::channel();
        let effective_deadline =
            deadline.unwrap_or_else(|| Instant::now() + SYNTHETIC_DEADLINE_DEFAULT);
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
    /// `scheduler/reasoning_tests.rs`. Kept out of the public API.
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
        // Note: we released the queue lock when the inner loop popped `req`. A
        // higher-priority request may be enqueued during this await. For Phase
        // F-b, deadlines are measured in seconds-to-minutes, so the race window
        // (microseconds) is acceptable. If sub-second priority inversion becomes
        // a concern, reshape the loop — but be careful: holding the queue lock
        // across an `acquire_owned` await risks convoy effects.
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
mod reasoning_tests;
