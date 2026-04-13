//! Token budget tracking and enforcement for agent operations.
//!
//! This module provides a `BudgetTracker` that enforces token usage limits
//! for agents. It tracks cumulative token usage and prevents agents from
//! exceeding their allocated budget.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Configuration for token budget enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BudgetConfig {
    /// Maximum tokens allowed per reset period.
    pub max_tokens: u64,
    /// Reset period in seconds (0 means no reset - one-time budget).
    #[serde(default = "default_reset_period")]
    pub reset_period_seconds: u64,
}

fn default_reset_period() -> u64 {
    3600 // 1 hour
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_tokens: 1000,
            reset_period_seconds: default_reset_period(),
        }
    }
}

/// Tracks token usage against a budget.
///
/// This struct is thread-safe and can be shared across multiple threads.
/// It uses atomic operations for counters to ensure thread safety without
/// requiring locks for the common case of tracking usage.
#[derive(Debug)]
pub struct BudgetTracker {
    config: BudgetConfig,
    used_tokens: AtomicU64,
    period_start: AtomicU64, // Stored as seconds since epoch
    last_reset_check: AtomicU64,
}

impl BudgetTracker {
    /// Creates a new BudgetTracker with the given configuration.
    pub fn new(config: BudgetConfig) -> Self {
        let now = Self::current_timestamp();
        Self {
            config,
            used_tokens: AtomicU64::new(0),
            period_start: AtomicU64::new(now),
            last_reset_check: AtomicU64::new(now),
        }
    }

    /// Tracks token usage and returns Ok(()) if within budget, Err if exceeded.
    ///
    /// # Arguments
    /// * `tokens` - Number of tokens to track
    ///
    /// # Returns
    /// * `Ok(())` if the tokens can be used within the budget
    /// * `Err(BudgetError::BudgetExceeded)` if the budget would be exceeded
    pub fn track(&self, tokens: u64) -> Result<(), BudgetError> {
        // First check if we need to reset based on period
        self.check_reset();
        
        // Use compare-and-swap loop to ensure atomic update
        let mut current = self.used_tokens.load(Ordering::Acquire);
        loop {
            let new_total = current + tokens;
            
            // Check if this would exceed the budget
            if new_total > self.config.max_tokens {
                return Err(BudgetError::BudgetExceeded {
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

    /// Checks the current usage without tracking new tokens.
    ///
    /// # Returns
    /// * `Ok(remaining)` if within budget, where remaining is tokens left
    /// * `Err(BudgetError::BudgetExceeded)` if already over budget
    pub fn check(&self) -> Result<u64, BudgetError> {
        self.check_reset();
        
        let used = self.used_tokens.load(Ordering::Acquire);
        if used > self.config.max_tokens {
            Err(BudgetError::BudgetExceeded {
                used,
                limit: self.config.max_tokens,
                requested: 0,
            })
        } else {
            Ok(self.config.max_tokens.saturating_sub(used))
        }
    }

    /// Gets the current token usage.
    pub fn get_usage(&self) -> u64 {
        self.check_reset();
        self.used_tokens.load(Ordering::Acquire)
    }

    /// Gets the maximum allowed tokens.
    pub fn get_limit(&self) -> u64 {
        self.config.max_tokens
    }

    /// Resets the budget tracker to start a new period.
    pub fn reset(&self) {
        let now = Self::current_timestamp();
        self.used_tokens.store(0, Ordering::Release);
        self.period_start.store(now, Ordering::Release);
        self.last_reset_check.store(now, Ordering::Release);
    }

    /// Checks if we need to reset based on the reset period.
    fn check_reset(&self) {
        // Only check once per second to avoid too many system calls
        let now = Self::current_timestamp();
        let last_check = self.last_reset_check.load(Ordering::Acquire);
        
        if now.saturating_sub(last_check) >= 1 {
            // Update last check time
            self.last_reset_check.store(now, Ordering::Release);
            
            // Check if reset period has elapsed
            let period_start = self.period_start.load(Ordering::Acquire);
            if self.config.reset_period_seconds > 0 && 
               now.saturating_sub(period_start) >= self.config.reset_period_seconds {
                self.reset();
            }
        }
    }

    /// Gets current timestamp in seconds since UNIX epoch.
    fn current_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

/// Errors that can occur during budget tracking.
#[derive(Debug, thiserror::Error)]
pub enum BudgetError {
    #[error("Budget exceeded: used {used}/{limit} tokens, requested {requested} more")]
    BudgetExceeded {
        used: u64,
        limit: u64,
        requested: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_tracker_within_limit() {
        let config = BudgetConfig {
            max_tokens: 100,
            reset_period_seconds: 0, // No reset for test
        };
        let tracker = BudgetTracker::new(config);
        
        assert!(tracker.track(50).is_ok());
        assert!(tracker.track(30).is_ok());
        assert_eq!(tracker.get_usage(), 80);
        
        let remaining = tracker.check().unwrap();
        assert_eq!(remaining, 20);
    }

    #[test]
    fn test_budget_tracker_exceeds_limit() {
        let config = BudgetConfig {
            max_tokens: 100,
            reset_period_seconds: 0,
        };
        let tracker = BudgetTracker::new(config);
        
        assert!(tracker.track(80).is_ok());
        assert!(tracker.track(30).is_err()); // Would exceed 110/100
        
        // Check error details
        match tracker.track(30) {
            Err(BudgetError::BudgetExceeded { used, limit, requested }) => {
                assert_eq!(used, 80);
                assert_eq!(limit, 100);
                assert_eq!(requested, 30);
            }
            _ => panic!("Expected BudgetExceeded error"),
        }
    }

    #[test]
    fn test_budget_tracker_reset() {
        let config = BudgetConfig {
            max_tokens: 100,
            reset_period_seconds: 1, // 1 second reset for test
        };
        let tracker = BudgetTracker::new(config);
        
        assert!(tracker.track(80).is_ok());
        assert_eq!(tracker.get_usage(), 80);
        
        // Wait for reset
        std::thread::sleep(Duration::from_secs(2));
        
        // Trigger a check which should reset
        let _ = tracker.check();
        
        // After reset, should be able to track more
        assert!(tracker.track(80).is_ok());
        assert_eq!(tracker.get_usage(), 80);
    }

    #[test]
    fn test_thread_safety() {
        use std::sync::Arc;
        use std::thread;
        
        let config = BudgetConfig {
            max_tokens: 1000,
            reset_period_seconds: 0,
        };
        let tracker = Arc::new(BudgetTracker::new(config));
        
        let mut handles = vec![];
        
        // Spawn 10 threads that each try to use 100 tokens
        for _ in 0..10 {
            let tracker_clone = Arc::clone(&tracker);
            handles.push(thread::spawn(move || {
                tracker_clone.track(100)
            }));
        }
        
        // Collect results
        let mut successes = 0;
        let mut failures = 0;
        for handle in handles {
            match handle.join().unwrap() {
                Ok(_) => successes += 1,
                Err(_) => failures += 1,
            }
        }
        
        // Should have exactly 10 successes (1000 total tokens)
        assert_eq!(successes, 10);
        assert_eq!(failures, 0);
        assert_eq!(tracker.get_usage(), 1000);
        
        // Next request should fail
        assert!(tracker.track(1).is_err());
    }

    #[test]
    fn test_default_config() {
        let config = BudgetConfig::default();
        assert_eq!(config.max_tokens, 1000);
        assert_eq!(config.reset_period_seconds, 3600);
    }

    #[test]
    fn test_serialization() {
        let config = BudgetConfig {
            max_tokens: 5000,
            reset_period_seconds: 7200,
        };
        
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: BudgetConfig = serde_json::from_str(&json).unwrap();
        
        assert_eq!(deserialized.max_tokens, 5000);
        assert_eq!(deserialized.reset_period_seconds, 7200);
    }

    #[test]
    fn test_check_without_tracking() {
        let config = BudgetConfig {
            max_tokens: 200,
            reset_period_seconds: 0,
        };
        let tracker = BudgetTracker::new(config);
        
        assert!(tracker.track(150).is_ok());
        let remaining = tracker.check().unwrap();
        assert_eq!(remaining, 50);
        
        assert!(tracker.track(60).is_err());
        let remaining_after_fail = tracker.check().unwrap();
        assert_eq!(remaining_after_fail, 50); // Still 50 remaining since last track failed
    }
}