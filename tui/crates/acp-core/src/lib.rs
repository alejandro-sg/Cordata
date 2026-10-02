//! All ACP protocol logic for the sierralab-agents terminal clients. Zero UI
//! dependencies: `acp-tui` and `acp-stress` both consume `SessionHandle` +
//! `AppEvent`, so the connection/session/permission code exists exactly once.

pub mod config;
pub mod connection;
pub mod error;
pub mod events;
pub mod permission;
pub mod session;

pub use config::BackendConfig;
pub use error::{Error, Result};
pub use events::{
    AppEvent, ConfigChoice, ConfigOptionInfo, ModeInfo, SessionInfo, StopReason, ToolCallInfo,
    ToolCallPatch, ToolKind, ToolStatus,
};
pub use permission::{ApprovalDecision, PermissionInfo};
pub use session::{SessionHandle, SessionOptions};
