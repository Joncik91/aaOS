//! Unit tests for ReasoningScheduler.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::time::timeout;

use super::ReasoningScheduler;

/// Helper: push a request and return the receiver for its permit.
async fn submit(
    sched: &Arc<ReasoningScheduler>,
    subtask: &str,
    deadline: Option<Instant>,
) -> oneshot::Receiver<tokio::sync::OwnedSemaphorePermit> {
    sched
        .enqueue_for_test(subtask.to_string(), 128, deadline)
        .await
}

#[tokio::test]
async fn priority_ordering_earliest_deadline_wins() {
    let sched = ReasoningScheduler::new(1);
    let now = Instant::now();
    let r_long = submit(&sched, "t_long", Some(now + Duration::from_secs(20))).await;
    let r_short = submit(&sched, "t_short", Some(now + Duration::from_secs(5))).await;
    let r_mid = submit(&sched, "t_mid", Some(now + Duration::from_secs(10))).await;

    // Slot pool is 1. Drain one-at-a-time and verify order.
    let first = timeout(Duration::from_secs(2), r_short)
        .await
        .unwrap()
        .unwrap();
    drop(first);
    let second = timeout(Duration::from_secs(2), r_mid)
        .await
        .unwrap()
        .unwrap();
    drop(second);
    let third = timeout(Duration::from_secs(2), r_long)
        .await
        .unwrap()
        .unwrap();
    drop(third);
}

#[tokio::test]
async fn fifo_tiebreak_for_equal_deadlines() {
    let sched = ReasoningScheduler::new(1);
    let now = Instant::now() + Duration::from_secs(10);

    // Hold the single slot so both requests queue.
    let block = submit(
        &sched,
        "block",
        Some(Instant::now() + Duration::from_secs(1)),
    )
    .await;
    let permit = timeout(Duration::from_secs(2), block)
        .await
        .unwrap()
        .unwrap();

    let r_first = submit(&sched, "first", Some(now)).await;
    let r_second = submit(&sched, "second", Some(now)).await;

    drop(permit);
    // First queued should win.
    let got_first = timeout(Duration::from_secs(2), r_first)
        .await
        .unwrap()
        .unwrap();
    drop(got_first);
    let got_second = timeout(Duration::from_secs(2), r_second)
        .await
        .unwrap()
        .unwrap();
    drop(got_second);
}

#[tokio::test]
async fn no_deadline_uses_synthetic_and_resolves() {
    let sched = ReasoningScheduler::new(1);
    let r = submit(&sched, "no_ttl", None).await;
    let permit = timeout(Duration::from_secs(2), r)
        .await
        .expect("no-TTL request should resolve")
        .unwrap();
    drop(permit);
}

#[tokio::test]
async fn dropped_waker_does_not_wedge_dispatcher() {
    let sched = ReasoningScheduler::new(1);

    // Hold the slot so the first real request queues, then we'll drop it.
    let block = submit(
        &sched,
        "block",
        Some(Instant::now() + Duration::from_secs(1)),
    )
    .await;
    let permit = timeout(Duration::from_secs(2), block)
        .await
        .unwrap()
        .unwrap();

    let r_ghost = submit(
        &sched,
        "ghost",
        Some(Instant::now() + Duration::from_secs(2)),
    )
    .await;
    let r_real = submit(
        &sched,
        "real",
        Some(Instant::now() + Duration::from_secs(3)),
    )
    .await;

    drop(r_ghost); // waker dropped — dispatcher must discard its permit and loop
    drop(permit);

    // Real request must still get served.
    let got_real = timeout(Duration::from_secs(5), r_real)
        .await
        .expect("dispatcher wedged: real request starved by dropped ghost")
        .unwrap();
    drop(got_real);
}
