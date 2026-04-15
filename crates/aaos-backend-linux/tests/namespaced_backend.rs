//! Integration tests for [`NamespacedBackend`].
//!
//! Marked `#[ignore]` per plan v4 commit-2 guidance: the full clone +
//! pivot_root + Landlock + seccomp path needs root (or
//! `CAP_SYS_ADMIN`) and a Linux kernel >= 5.13 with Landlock enabled.
//! Run on a deb test host with:
//!
//! ```bash
//! cargo test -p aaos-backend-linux --test namespaced_backend -- --ignored
//! ```

#![cfg(target_os = "linux")]

use aaos_backend_linux::{NamespacedBackend, NamespacedBackendConfig};
use aaos_core::{AgentBackend, AgentId, AgentLaunchSpec, AgentManifest};

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
        workspace_path: std::path::PathBuf::new(),
        budget_config: None,
    }
}

#[tokio::test]
#[ignore = "requires root + Linux >= 5.13 with Landlock; run manually per plan v4"]
async fn launch_reaches_sandboxed_ready() {
    let backend = NamespacedBackend::new(NamespacedBackendConfig::default())
        .expect("Landlock supported on this kernel");
    let handle = backend
        .launch(sample_spec())
        .await
        .expect("worker must reach sandboxed-ready");
    backend.stop(&handle).await.expect("stop");
}

#[tokio::test]
#[ignore = "requires root + Linux >= 5.13 with Landlock; run manually per plan v4"]
async fn stop_is_idempotent() {
    let backend = NamespacedBackend::new(NamespacedBackendConfig::default()).unwrap();
    let handle = backend.launch(sample_spec()).await.unwrap();
    backend.stop(&handle).await.unwrap();
    backend.stop(&handle).await.unwrap();
}

#[tokio::test]
#[ignore = "requires root + Linux >= 5.13 with Landlock; run manually per plan v4"]
async fn worker_cannot_execve() {
    // Launch, send a PokeOp::TryExecve over the broker socket,
    // observe that the worker exits with SIGSYS.
    //
    // Concrete implementation of this test needs the broker-side
    // agent loop to be wired — a follow-up to commit 2.
}

#[tokio::test]
#[ignore = "requires root + Linux >= 5.13 with Landlock; run manually per plan v4"]
async fn health_detects_exit() {
    // Launch, kill the child externally, observe
    // BackendHealth::Signaled / Exited.
}
