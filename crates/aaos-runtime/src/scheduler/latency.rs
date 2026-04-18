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
