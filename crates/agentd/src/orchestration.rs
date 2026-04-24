//! Per-submit orchestration mode selector.
//!
//! Defines the two orchestration paths available for `agentd submit`:
//! - [`OrchestrationMode::Plan`] — the default Planner + PlanExecutor DAG path.
//! - [`OrchestrationMode::Persistent`] — routes to the Bootstrap persistent agent.
//!
//! The enum is defined here so both the CLI layer (`cli/submit.rs`) and the
//! server layer (`server.rs`) can share it without duplication.

/// Selects which orchestration path handles a submitted goal.
///
/// Passed as `--orchestration <mode>` on the CLI and serialized into the
/// `orchestration` field of the `agent.submit_streaming` JSON-RPC params.
/// Missing field on the wire defaults to [`OrchestrationMode::Plan`] for
/// backwards compatibility with clients that predate this flag.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum OrchestrationMode {
    /// Route through the Planner + PlanExecutor DAG (default). Best for
    /// structured goals with declared outputs per subtask (fetch, analyse,
    /// write). Requires a loaded role catalog (`/etc/aaos/roles/` or
    /// `AAOS_ROLES_DIR`); returns an error if the catalog failed to load.
    #[default]
    Plan,
    /// Route to the Bootstrap persistent agent. Best for open-ended,
    /// exploratory, or long-context goals where a single multi-turn agent
    /// manages its own context and spawns children as needed.
    Persistent,
}
