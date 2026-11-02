//! The agent engine and its session API, with no UI: `acp-tui` and `cordata-app` both consume `SessionHandle` and
//! `AppEvent`, so the engine, its tools and the approval round-trip exist exactly once.

pub mod error;
pub mod events;
pub mod native;
pub mod permission;
pub mod session;
pub mod transport;

pub use error::{Error, Result};
pub use events::{AppEvent, FileDiff, SessionInfo, StopReason, ToolCallInfo, ToolCallPatch, ToolKind, ToolStatus};
pub use permission::{ApprovalDecision, PermissionInfo};
pub use session::{Attachment, SessionHandle, SessionOptions};
pub use transport::BackendTransport;
