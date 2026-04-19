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

    /// Decrement `max_hops` by 1.
    ///
    /// Returns `Some(self)` with the decremented state, unless `max_hops` was
    /// already `Some(0)` (exhausted), in which case returns `None` so the caller
    /// can distinguish "still launchable, now with fewer hops" from "out of
    /// hops, refuse launch." `max_hops == Some(0)` is a valid stored state
    /// meaning "this subtask will be refused on its next spawn"; the executor
    /// separately checks `hops == 0` before launching.
    ///
    /// Leaves `max_hops: None` untouched (means "no hop bound at all").
    /// `max_wall_clock` is not affected.
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
        /// Upper bound for max_wall_clock — 30 days. Longer than any reasonable
        /// subtask TTL, far from Instant-arithmetic overflow territory. Rejects
        /// adversarial or accidentally huge values at deserialize time so the
        /// executor's `Instant::now() + d` can't panic downstream.
        const MAX_WALL_CLOCK_SECS: f64 = 86_400.0 * 30.0;

        let opt = Option::<f64>::deserialize(d)?;
        opt.map(|secs| {
            if !secs.is_finite() || !(0.0..=MAX_WALL_CLOCK_SECS).contains(&secs) {
                return Err(serde::de::Error::custom(format!(
                    "TaskTtl.max_wall_clock must be a non-negative finite number of seconds \
                     no greater than {MAX_WALL_CLOCK_SECS} (30 days), got {secs}"
                )));
            }
            Ok(Duration::from_secs_f64(secs))
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_ttl_is_empty() {
        assert!(TaskTtl {
            max_hops: None,
            max_wall_clock: None
        }
        .is_empty());
    }

    #[test]
    fn decrement_hops_counts_down() {
        let t = TaskTtl {
            max_hops: Some(3),
            max_wall_clock: None,
        };
        let t = t.decrement_hops().unwrap();
        assert_eq!(t.max_hops, Some(2));
        let t = t.decrement_hops().unwrap();
        assert_eq!(t.max_hops, Some(1));
        let t = t.decrement_hops().unwrap();
        assert_eq!(t.max_hops, Some(0));
        // Now max_hops is Some(0); further decrement returns None.
        assert!(
            t.decrement_hops().is_none(),
            "0 hops left means refuse launch"
        );
    }

    #[test]
    fn decrement_leaves_none_alone() {
        let t = TaskTtl {
            max_hops: None,
            max_wall_clock: Some(std::time::Duration::from_secs(10)),
        };
        let t = t.decrement_hops().unwrap();
        assert!(t.max_hops.is_none());
        assert_eq!(t.max_wall_clock, Some(std::time::Duration::from_secs(10)));
    }

    #[test]
    fn roundtrips_through_json() {
        let t = TaskTtl {
            max_hops: Some(5),
            max_wall_clock: Some(std::time::Duration::from_secs_f64(1.5)),
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: TaskTtl = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn deserialize_rejects_excessive_wall_clock() {
        let excessive = r#"{"max_wall_clock": 1e17}"#;
        let result: Result<TaskTtl, _> = serde_json::from_str(excessive);
        assert!(result.is_err(), "1e17 seconds must be rejected (> 30 days)");

        let reasonable = r#"{"max_wall_clock": 86400.0}"#;
        let ok: TaskTtl = serde_json::from_str(reasonable).unwrap();
        assert_eq!(
            ok.max_wall_clock,
            Some(std::time::Duration::from_secs(86_400))
        );
    }
}
