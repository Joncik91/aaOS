//! `LatencyTracker` — records wall-clock cost of LLM calls per subtask.
//!
//! Minimal v1 impl `SubtaskWallClockTracker` sums per-subtask elapsed time
//! so the TTL watcher and (eventually) Gap 2's router can query it. Per-
//! model aggregation lives behind the trait for Gap 2 to add later.

use std::sync::Arc;
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

    #[test]
    fn composite_fans_out_records_to_all_inner_trackers() {
        use super::*;
        use crate::scheduler::PerModelLatencyTracker;
        use std::sync::Arc;

        let wall = Arc::new(SubtaskWallClockTracker::new());
        let per_model = Arc::new(PerModelLatencyTracker::new());
        per_model.register("a", "deepseek-chat");

        let composite: Arc<dyn LatencyTracker> = Arc::new(CompositeLatencyTracker::new(vec![
            wall.clone() as Arc<dyn LatencyTracker>,
            per_model.clone() as Arc<dyn LatencyTracker>,
        ]));

        composite.record("a", std::time::Duration::from_millis(100));

        assert_eq!(
            wall.wall_clock_elapsed("a"),
            std::time::Duration::from_millis(100)
        );
        assert_eq!(
            per_model.p50("deepseek-chat").unwrap(),
            std::time::Duration::from_millis(100)
        );
    }
}
