//! Integration tests for [`NamespacedBackend`].
//!
//! Marked `#[ignore]` because the full clone + pivot_root + Landlock +
//! seccomp path needs Linux 5.13+ with Landlock enabled, unprivileged
//! user namespaces available, and the `aaos-agent-worker` binary built.
//! Run on a capable host with:
//!
//! ```bash
//! cargo build -p aaos-backend-linux --bin aaos-agent-worker
//! cargo test -p aaos-backend-linux --test namespaced_backend -- --ignored
//! ```

#![cfg(target_os = "linux")]

use aaos_backend_linux::{
    probe_mount_capable, NamespacedBackend, NamespacedBackendConfig, NamespacedState,
};
use aaos_core::{AgentBackend, AgentId, AgentLaunchSpec, AgentManifest, BackendHealth};
use std::path::{Path, PathBuf};

/// Returns true and prints a SKIP message if the host forbids mount
/// operations inside unprivileged user+mount namespaces (e.g. GitHub
/// Actions Azure runners with AppArmor restrictions). Tests should
/// early-return when this returns true. See `probe_mount_capable`
/// doc for background.
fn should_skip_namespaced_test() -> bool {
    if probe_mount_capable() {
        return false;
    }
    eprintln!(
        "SKIP: host forbids mount operations inside user namespaces \
         (likely GitHub Actions or other LSM-restricted CI). \
         Run this test on a real Linux host (dev box or DO droplet)."
    );
    true
}

/// Build a backend config with a session dir in a tempdir and the
/// worker binary resolved to an absolute path under the workspace's
/// `target/debug/`. Absolute path is required because the child
/// changes cwd during pivot_root.
fn test_config(tmp: &Path) -> NamespacedBackendConfig {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo test");
    let workspace_root = Path::new(&manifest_dir).parent().unwrap().parent().unwrap();
    let worker = workspace_root.join("target/debug/aaos-agent-worker");
    assert!(
        worker.exists(),
        "worker binary missing — run `cargo build -p aaos-backend-linux --bin aaos-agent-worker` first: {}",
        worker.display()
    );
    NamespacedBackendConfig {
        session_dir: tmp.join("sessions"),
        worker_binary: worker,
        ..NamespacedBackendConfig::default()
    }
}

fn sample_spec() -> AgentLaunchSpec {
    AgentLaunchSpec {
        agent_id: AgentId::new(),
        manifest: AgentManifest::from_yaml(
            r#"
name: integration
model: claude-haiku-4-5-20251001
system_prompt: "integration test"
"#,
        )
        .unwrap(),
        capability_handles: vec![],
        workspace_path: PathBuf::new(),
        budget_config: None,
    }
}

#[tokio::test]
#[ignore = "requires Linux 5.13+ with unprivileged user namespaces and the worker binary built; run manually"]
async fn launch_reaches_sandboxed_ready() {
    if should_skip_namespaced_test() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let backend =
        NamespacedBackend::new(test_config(tmp.path())).expect("Landlock supported on this kernel");
    let handle = backend
        .launch(sample_spec())
        .await
        .expect("worker must reach sandboxed-ready");
    assert_eq!(handle.backend_kind, "namespaced");
    backend.stop(&handle).await.expect("stop");
}

#[tokio::test]
#[ignore = "requires Linux 5.13+ with unprivileged user namespaces and the worker binary built; run manually"]
async fn stop_is_idempotent() {
    if should_skip_namespaced_test() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let backend = NamespacedBackend::new(test_config(tmp.path())).unwrap();
    let handle = backend.launch(sample_spec()).await.unwrap();
    backend.stop(&handle).await.expect("first stop");
    backend
        .stop(&handle)
        .await
        .expect("second stop must be a no-op");
}

#[tokio::test]
#[ignore = "requires Linux 5.13+ with unprivileged user namespaces and the worker binary built; run manually"]
async fn health_detects_exit() {
    // Launch, SIGKILL the child externally, verify health() reports
    // Signaled or Exited (not Healthy).
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    if should_skip_namespaced_test() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let backend = NamespacedBackend::new(test_config(tmp.path())).unwrap();
    let handle = backend.launch(sample_spec()).await.unwrap();

    let state = handle
        .state::<NamespacedState>()
        .expect("handle carries NamespacedState");
    let pid = state.pid;

    // Kill the worker externally.
    kill(Pid::from_raw(pid as i32), Signal::SIGKILL).expect("can send SIGKILL");

    // Give the parent a moment to reap / detect.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let health = backend.health(&handle).await;
    match health {
        BackendHealth::Signaled(_) | BackendHealth::Exited(_) | BackendHealth::Disconnected => {}
        BackendHealth::Healthy => panic!("health must not report Healthy after SIGKILL"),
        BackendHealth::Unknown(msg) => panic!("health returned Unknown: {msg}"),
    }

    // Idempotent cleanup.
    let _ = backend.stop(&handle).await;
}

#[tokio::test]
#[ignore = "requires Linux 5.13+ with unprivileged user namespaces and the worker binary built; run manually"]
async fn ping_roundtrips_over_persistent_stream() {
    // First real transport exercise of the broker→worker persistent
    // stream that's installed after `sandboxed-ready`. No sandbox
    // semantics tested here — just proof that a round-trip works at all.
    // Correlation on request id is implicit: `send_ping` takes a nonce,
    // the worker echoes it, `send_ping` asserts they match.
    use std::time::Duration;

    if should_skip_namespaced_test() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let backend = NamespacedBackend::new(test_config(tmp.path())).unwrap();
    let spec = sample_spec();
    let agent_id = spec.agent_id;
    let handle = backend.launch(spec).await.expect("launch");

    let session = backend
        .session(&agent_id)
        .expect("session must be present after launch");

    let elapsed = session
        .send_ping(0xdead_beef, Duration::from_secs(5))
        .await
        .expect("ping must round-trip");
    assert!(
        elapsed < Duration::from_secs(5),
        "ping reported elapsed >= timeout, something wrong with instrumentation: {elapsed:?}"
    );

    // A second ping with a different nonce confirms the correlation map
    // handles consecutive requests (not just one-shot).
    let _ = session
        .send_ping(0xcafe_babe, Duration::from_secs(5))
        .await
        .expect("second ping must also round-trip");

    backend.stop(&handle).await.expect("stop");
}

#[tokio::test]
#[ignore = "requires Linux 5.13+ with unprivileged user namespaces and the worker binary built; run manually"]
async fn worker_cannot_execve() {
    // The worker handles `Request::Poke(PokeOp::TryExecve)` via
    // `handle_poke_with_id` in worker.rs — seccomp kills it with SIGSYS
    // when it calls `execve`. We route the poke over the persistent
    // post-handshake stream and observe either (a) a SIGSYS death that
    // closes the socket (the positive outcome), or (b) an error
    // response if execve was somehow allowed (the negative outcome
    // that would fail this assertion).
    use aaos_backend_linux::broker_protocol::PokeOp;
    use std::time::Duration;

    if should_skip_namespaced_test() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let backend = NamespacedBackend::new(test_config(tmp.path())).unwrap();
    let spec = sample_spec();
    let agent_id = spec.agent_id;
    let handle = backend.launch(spec).await.expect("launch");
    let session = backend.session(&agent_id).expect("session");

    // Either the poke round-trips with an error (execve didn't SIGSYS
    // — sandbox broken, this test fails) or the worker dies mid-execve
    // and the send times out / errors because the socket closes. Either
    // way a successful "denied" response is a bug.
    let result = session
        .send_poke(PokeOp::TryExecve, Duration::from_secs(3))
        .await;
    match result {
        Ok(resp) => {
            assert!(
                resp.error.is_some(),
                "execve did not SIGSYS — sandbox broken? result: {resp:?}"
            );
        }
        Err(_) => {
            // Socket closed mid-request — expected when seccomp kills
            // the worker with SIGSYS. Verify health reflects the death.
        }
    }

    // Give the parent a moment to observe the exit if the worker died.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let health = backend.health(&handle).await;
    assert!(
        matches!(
            health,
            BackendHealth::Signaled(_) | BackendHealth::Exited(_) | BackendHealth::Disconnected
        ),
        "worker must be dead after attempted execve, got: {health:?}"
    );

    let _ = backend.stop(&handle).await;
}
