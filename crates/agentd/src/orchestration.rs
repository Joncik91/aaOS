//! Per-submit orchestration mode selector.
//!
//! Defines the two orchestration paths available for `agentd submit`:
//! - [`OrchestrationMode::Plan`] — force classifier to `decompose` (Planner + PlanExecutor DAG).
//! - [`OrchestrationMode::Persistent`] — force `direct` (1-node inline plan through PlanExecutor).
//!
//! When no explicit `--orchestration` flag is passed the classifier auto-routes:
//! goals with independent parallelisable subtasks → `decompose`; everything else → `direct`.
//!
//! The enum is defined here so both the CLI layer (`cli/submit.rs`) and the
//! server layer (`server.rs`) can share it without duplication.

/// Selects which orchestration path handles a submitted goal.
///
/// Passed as `--orchestration <mode>` on the CLI and serialized into the
/// `orchestration` field of the `agent.submit_streaming` JSON-RPC params.
/// Missing field on the wire defaults to classifier auto-detection.
///
/// # Semantics change in v0.1.0
///
/// Prior to v0.1.0 `Plan` routed to the Planner+PlanExecutor DAG and
/// `Persistent` routed to the Bootstrap persistent agent (a separate code
/// path). In v0.1.0 both modes route through the unified PlanExecutor —
/// `Plan` forces multi-node DAG decomposition; `Persistent` forces a 1-node
/// inline plan that gives the generalist a full multi-turn iteration budget.
/// The Bootstrap agent is no longer used for per-submit work.
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
    /// Force classifier to `decompose`: run the Planner + PlanExecutor DAG.
    /// Best for structured goals with clear data flow (fetch/analyse/write).
    /// Requires a loaded role catalog; returns an error if absent.
    #[default]
    Plan,
    /// Force classifier to `direct`: construct a 1-node inline Plan and run it
    /// through PlanExecutor with the generalist role at full iteration budget.
    /// Best for open-ended exploration goals. Does NOT route to Bootstrap.
    Persistent,
}
