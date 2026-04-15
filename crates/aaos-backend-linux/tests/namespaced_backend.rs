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

use aaos_backend_linux::{NamespacedBackend, NamespacedBackendConfig, NamespacedState};
use aaos_core::{AgentBackend, AgentId, AgentLaunchSpec, AgentManifest, BackendHealth};
use std::path::{Path, PathBuf};

/// Build a backend config with a session dir in a tempdir and the
/// worker binary resolved to an absolute path under the workspace's
/// `target/debug/`. Absolute path is required because the child
/// changes cwd during pivot_root.
fn test_config(tmp: &Path) -> NamespacedBackendConfig {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo test");
    let workspace_root = Path::new(&manifest_dir)
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let worker = workspace_root.join("target/debug/aaos-agent-worker");
    assert!(
        worker.exists(),
        "worker binary missing — run `cargo build -p aaos-backend-linux --bin aaos-agent-worker` first: {}",
        worker.display()
    );
    let mut cfg = NamespacedBackendConfig::default();
    cfg.session_dir = tmp.join("sessions");
    cfg.worker_binary = worker;
    cfg
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
#[ignore = "scaffold: needs broker-side persistent stream to send PokeOp::TryExecve"]
async fn worker_cannot_execve() {
    // Placeholder. The worker already handles `Request::Poke(PokeOp::TryExecve)`
    // via `handle_poke` in worker.rs — seccomp kills it with SIGSYS when it
    // calls `execve`. What's missing is on the broker side: after
    // `run_handshake` returns, the connected `UnixStream` is dropped, so
    // there's no channel to send the poke through.
    //
    // The follow-up to fully wire this test:
    //
    // 1. BrokerSession stores the connected stream after handshake.
    // 2. NamespacedBackend exposes a `send_poke(agent_id, PokeOp)` method
    //    that takes the session's stored stream and writes a WireRequest.
    // 3. This test:
    //    a. `backend.launch(spec)` to sandboxed-ready.
    //    b. `backend.send_poke(agent_id, PokeOp::TryExecve)`.
    //    c. Wait briefly for the child to receive + attempt execve.
    //    d. Assert `backend.health(&handle) == Signaled(31)` (SIGSYS).
    //
    // For now this test launches and stops to prove the end-to-end setup
    // works, which is independently useful.
    let tmp = tempfile::tempdir().unwrap();
    let backend = NamespacedBackend::new(test_config(tmp.path())).unwrap();
    let handle = backend.launch(sample_spec()).await.unwrap();
    assert_eq!(handle.backend_kind, "namespaced");
    let _ = backend.stop(&handle).await;
}
