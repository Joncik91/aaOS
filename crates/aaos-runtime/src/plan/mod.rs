//! Computed orchestration — role catalog, planner, deterministic executor.
//!
//! Agents today are described by static YAML manifests that Bootstrap has
//! to improvise against at runtime. The plan module replaces that with a
//! typed two-phase boot: a Planner emits a structured Plan; a PlanExecutor
//! walks the DAG, instantiating children from per-role scaffolds. The LLM
//! reasons about content inside each child; orchestration is pure code.

pub mod role;

pub use role::{
    ParameterSchema, ParameterType, Role, RoleBudget, RoleCatalog, RoleCatalogError, RoleRetry,
};
