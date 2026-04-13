//! Per-agent token budget tracking and enforcement.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Configuration for token budget enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// Maximum tokens allowed per reset period.
    pub max_tokens: u64,
    /// Reset period in seconds (0 means no reset — one-time budget).
    #[serde(default = "default_reset_period")]
    pub reset_period_seconds: u64,
}

fn default_reset_period() -> u64 {
    3600
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_tokens: 1_000_000,
            reset_period_seconds: default_reset_period(),
        }
    }
}

/// Thread-safe per-agent token budget tracker.
///
/// Uses atomic operations — no locks on the hot path.
#[derive(Debug)]
pub struct BudgetTracker {
    config: BudgetConfig,
    used_tokens: AtomicU64,
    period_start: AtomicU64,
    last_reset_check: AtomicU64,
}

impl BudgetTracker {
    pub fn new(config: BudgetConfig) -> Self {
        let now = Self::now_secs();
        Self {
            config,
            used_tokens: AtomicU64::new(0),
            period_start: AtomicU64::new(now),
            last_reset_check: AtomicU64::new(now),
        }
    }

    /// Track token usage. Returns Ok(()) if within budget, Err if exceeded.
    pub fn track(&self, tokens: u64) -> Result<(), BudgetExceeded> {
        self.maybe_reset();

        let mut current = self.used_tokens.load(Ordering::Acquire);
        loop {
            let new_total = current + tokens;
            if new_total > self.config.max_tokens {
                return Err(BudgetExceeded {
                    used: current,
                    limit: self.config.max_tokens,
                    requested: tokens,
                });
            }
            match self.used_tokens.compare_exchange_weak(
                current,
                new_total,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    /// Check remaining budget without consuming tokens.
    pub fn remaining(&self) -> u64 {
        self.maybe_reset();
        let used = self.used_tokens.load(Ordering::Acquire);
        self.config.max_tokens.saturating_sub(used)
    }

    /// Current usage.
    pub fn used(&self) -> u64 {
        self.maybe_reset();
        self.used_tokens.load(Ordering::Acquire)
    }

    /// Budget limit.
    pub fn limit(&self) -> u64 {
        self.config.max_tokens
    }

    /// Force reset.
    pub fn reset(&self) {
        let now = Self::now_secs();
        self.used_tokens.store(0, Ordering::Release);
        self.period_start.store(now, Ordering::Release);
        self.last_reset_check.store(now, Ordering::Release);
    }

    fn maybe_reset(&self) {
        if self.config.reset_period_seconds == 0 {
            return;
        }
        let now = Self::now_secs();
        let last = self.last_reset_check.load(Ordering::Acquire);
        if now.saturating_sub(last) < 1 {
            return;
        }
        self.last_reset_check.store(now, Ordering::Release);
        let start = self.period_start.load(Ordering::Acquire);
        if now.saturating_sub(start) >= self.config.reset_period_seconds {
            self.reset();
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

/// Error returned when an agent exceeds its token budget.
#[derive(Debug, thiserror::Error)]
#[error("budget exceeded: used {used}/{limit} tokens, requested {requested} more")]
pub struct BudgetExceeded {
    pub used: u64,
    pub limit: u64,
    pub requested: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn within_budget() {
        let t = BudgetTracker::new(BudgetConfig { max_tokens: 100, reset_period_seconds: 0 });
        assert!(t.track(50).is_ok());
        assert!(t.track(30).is_ok());
        assert_eq!(t.used(), 80);
        assert_eq!(t.remaining(), 20);
    }

    #[test]
    fn exceeds_budget() {
        let t = BudgetTracker::new(BudgetConfig { max_tokens: 100, reset_period_seconds: 0 });
        assert!(t.track(80).is_ok());
        let err = t.track(30).unwrap_err();
        assert_eq!(err.used, 80);
        assert_eq!(err.limit, 100);
        assert_eq!(err.requested, 30);
        // Original usage unchanged after failed track
        assert_eq!(t.used(), 80);
    }

    #[test]
    fn reset_after_period() {
        let t = BudgetTracker::new(BudgetConfig { max_tokens: 100, reset_period_seconds: 1 });
        assert!(t.track(80).is_ok());
        std::thread::sleep(Duration::from_secs(2));
        // Trigger reset check
        let _ = t.remaining();
        assert!(t.track(80).is_ok());
    }

    #[test]
    fn thread_safety() {
        use std::sync::Arc;
        let t = Arc::new(BudgetTracker::new(BudgetConfig { max_tokens: 1000, reset_period_seconds: 0 }));
        let mut handles = vec![];
        for _ in 0..10 {
            let tc = t.clone();
            handles.push(std::thread::spawn(move || tc.track(100)));
        }
        let successes = handles.into_iter().filter_map(|h| h.join().unwrap().ok()).count();
        assert_eq!(successes, 10);
        assert_eq!(t.used(), 1000);
        assert!(t.track(1).is_err());
    }

    #[test]
    fn config_serde() {
        let c = BudgetConfig { max_tokens: 5000, reset_period_seconds: 7200 };
        let json = serde_json::to_string(&c).unwrap();
        let d: BudgetConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(d.max_tokens, 5000);
        assert_eq!(d.reset_period_seconds, 7200);
    }
}
