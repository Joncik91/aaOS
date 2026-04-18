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
    pub const CAPACITY: usize = 256;

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
