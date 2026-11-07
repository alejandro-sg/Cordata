//! The agent engine and its session API, with no UI: `acp-tui` and `cordata-app` both consume `SessionHandle` and
//! `AppEvent`, so the engine, its tools and the approval round-trip exist exactly once.

pub mod acp_registry;
pub mod auto;
pub mod error;
pub mod events;
pub mod history;
pub mod hooks;
pub mod library;
pub mod native;
pub mod permission;
pub mod policy;
pub mod session;
pub mod skills;
pub mod transport;

pub use error::{Error, Result};
pub use events::{
    AppEvent, FileDiff, PlanItem, PlanItemStatus, PlanUpdate, Restored, SessionInfo, StopReason,
    TokenUsage, ToolCallInfo, ToolCallPatch, ToolKind, ToolStatus,
};
pub use permission::{ApprovalDecision, PermissionInfo};
pub use policy::Mode;
pub use session::{Attachment, SessionHandle, SessionOptions};
pub use transport::BackendTransport;
