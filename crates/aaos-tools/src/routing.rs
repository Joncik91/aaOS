//! Re-exports of tool-execution routing primitives from aaos-core.
//!
//! The canonical home is aaos-core::tool_surface — aaos-core is
//! below aaos-tools in the dep tree and AuditEventKind::ToolInvoked
//! needs to carry the enum.
pub use aaos_core::tool_surface::{route_for, ToolExecutionSurface, DAEMON_SIDE_TOOLS};
