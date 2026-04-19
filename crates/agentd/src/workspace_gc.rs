//! Workspace garbage collection.
//!
//! Each `PlanExecutor` run creates a directory under
//! `/var/lib/aaos/workspace/<run-id>/` that holds the rendered `plan.json`
//! plus any fetched or written artifacts. Nothing in the runtime prunes
//! these; a long-running daemon accumulates one directory per run
//! forever until the disk fills (soak-test Bug 3 from 2026-04-19).
//!
//! This module adds a simple TTL-based sweep:
//!
//! * Run once at daemon startup (catches debris from prior runs).
//! * Run every hour afterward in a background tokio task.
//! * Prune any subdirectory whose mtime is older than
//!   `AAOS_WORKSPACE_TTL_DAYS` (default 7) days.
//!
//! Skipped when:
//! * `AAOS_WORKSPACE_TTL_DAYS=0` is explicitly set (opt-out).
//! * The workspace root doesn't exist (fresh install, first submit will
//!   create it).
//!
//! All logging is at `info!` / `warn!` so operators can see the GC is
//! active in `journalctl -u agentd`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How often the background task sweeps after startup. Hour-granular
/// because TTLs are day-granular; no need for tight polling.
const GC_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Read the configured TTL in days. `None` = disabled (operator set 0).
/// Default is 7 days.
fn ttl_days() -> Option<u64> {
    match std::env::var("AAOS_WORKSPACE_TTL_DAYS") {
        Ok(s) => match s.parse::<u64>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => Some(7),
        },
        Err(_) => Some(7),
    }
}

/// Prune workspace subdirectories older than `ttl_days` days. Returns
/// `(removed, total)` — how many were pruned vs how many existed. Errors
/// reading / removing individual entries are logged but do not abort the
/// sweep; the next tick will retry.
pub fn sweep_once(root: &Path, ttl_days: u64) -> (usize, usize) {
    let Ok(entries) = std::fs::read_dir(root) else {
        // Root missing or unreadable is expected on fresh installs.
        // Don't warn — first submit creates the dir.
        return (0, 0);
    };

    let ttl_secs = ttl_days.saturating_mul(24 * 60 * 60);
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(ttl_secs))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut removed = 0usize;
    let mut total = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        total += 1;
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "workspace-gc: stat failed");
                continue;
            }
        };
        if mtime >= cutoff {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                removed += 1;
                tracing::info!(path = %path.display(), "workspace-gc: removed stale run directory");
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "workspace-gc: remove_dir_all failed; will retry next tick"
                );
            }
        }
    }
    (removed, total)
}

/// Start the background GC task. The startup sweep runs synchronously
/// before this returns so boot-time cleanup is deterministic; the
/// periodic sweeps continue on a tokio timer.
///
/// No-op when the TTL is disabled via `AAOS_WORKSPACE_TTL_DAYS=0`.
pub fn spawn(root: PathBuf) {
    let Some(ttl) = ttl_days() else {
        tracing::info!(
            root = %root.display(),
            "workspace-gc: disabled (AAOS_WORKSPACE_TTL_DAYS=0)"
        );
        return;
    };

    // Startup sweep — synchronous so any pre-existing debris is handled
    // before we start serving.
    let (removed, total) = sweep_once(&root, ttl);
    tracing::info!(
        root = %root.display(),
        ttl_days = ttl,
        removed,
        total,
        "workspace-gc: startup sweep complete"
    );

    // Background periodic sweep. Uses tokio::spawn so it shares the
    // daemon's runtime; no need for a dedicated thread.
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(GC_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it since we just did the
        // startup sweep.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let (removed, total) = sweep_once(&root, ttl);
            if removed > 0 {
                tracing::info!(
                    root = %root.display(),
                    ttl_days = ttl,
                    removed,
                    total,
                    "workspace-gc: periodic sweep complete"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Backdate `path`'s mtime via `touch -d`. Used only by tests to
    /// simulate a stale run directory without adding a `filetime` crate
    /// dep just for tests. Returns true if the touch succeeded — on
    /// non-Linux CI this silently skips.
    fn backdate(path: &Path, iso_ago: &str) -> bool {
        std::process::Command::new("touch")
            .arg("-d")
            .arg(iso_ago)
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn sweep_removes_old_dirs_and_keeps_fresh_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let stale = root.join("run-stale");
        let fresh = root.join("run-fresh");
        fs::create_dir_all(&stale).unwrap();
        fs::create_dir_all(&fresh).unwrap();
        fs::write(stale.join("plan.json"), b"{}").unwrap();
        fs::write(fresh.join("plan.json"), b"{}").unwrap();

        // Backdate the stale dir's mtime to 10 days ago.
        if !backdate(&stale, "10 days ago") {
            eprintln!("SKIP: `touch` not available to backdate test fixture");
            return;
        }

        let (removed, total) = sweep_once(root, 7);
        assert_eq!(removed, 1);
        assert_eq!(total, 2);
        assert!(!stale.exists(), "stale dir must have been removed");
        assert!(fresh.exists(), "fresh dir must remain");
    }

    #[test]
    fn sweep_no_op_on_missing_root() {
        let missing = std::path::PathBuf::from("/nonexistent/path/that/does/not/exist");
        let (removed, total) = sweep_once(&missing, 7);
        assert_eq!(removed, 0);
        assert_eq!(total, 0);
    }

    #[test]
    fn sweep_ignores_non_directory_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("stray-file.txt"), b"hi").unwrap();
        let (removed, total) = sweep_once(root, 7);
        assert_eq!(removed, 0);
        assert_eq!(total, 0);
    }

    // ttl_days reads a process-global env var; tests that mutate it
    // must serialize or they race. Combine into one test.
    #[test]
    fn ttl_env_parsing() {
        // SAFETY: these tests run sequentially within one test function,
        // so no other thread is reading AAOS_WORKSPACE_TTL_DAYS during
        // the set/remove windows.

        // Default when unset.
        std::env::remove_var("AAOS_WORKSPACE_TTL_DAYS");
        assert_eq!(ttl_days(), Some(7));

        // Explicit 0 disables.
        std::env::set_var("AAOS_WORKSPACE_TTL_DAYS", "0");
        assert!(ttl_days().is_none());

        // Nonsense → default.
        std::env::set_var("AAOS_WORKSPACE_TTL_DAYS", "nonsense");
        assert_eq!(ttl_days(), Some(7));

        // Valid explicit value.
        std::env::set_var("AAOS_WORKSPACE_TTL_DAYS", "30");
        assert_eq!(ttl_days(), Some(30));

        // Cleanup.
        std::env::remove_var("AAOS_WORKSPACE_TTL_DAYS");
    }
}
