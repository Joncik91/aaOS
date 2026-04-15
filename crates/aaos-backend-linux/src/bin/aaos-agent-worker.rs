//! `aaos-agent-worker` — the binary that a [`NamespacedBackend`]
//! execs into a Linux user+mount+IPC namespace.
//!
//! All real logic lives in [`aaos_backend_linux::worker`]. This file
//! is the 10-line binary glue: initialise logging, call `run`.
//!
//! [`NamespacedBackend`]: aaos_backend_linux::NamespacedBackend

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    #[cfg(target_os = "linux")]
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async move {
            let config = match aaos_backend_linux::worker::WorkerConfig::from_env() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("worker config: {e}");
                    std::process::exit(2);
                }
            };
            if let Err(e) = aaos_backend_linux::worker::run(config).await {
                eprintln!("worker: {e}");
                std::process::exit(1);
            }
        });
    }

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("aaos-agent-worker only runs on Linux");
        std::process::exit(3);
    }
}
