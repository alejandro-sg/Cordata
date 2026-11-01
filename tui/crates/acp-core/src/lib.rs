//! All ACP protocol logic for the Cordata terminal clients. Zero UI
//! dependencies: `acp-tui` and `cordata-app` both consume `SessionHandle` +
//! `AppEvent`, so the connection/session/permission code exists exactly once.

pub mod error;
pub mod events;
pub mod native;
pub mod permission;
pub mod session;
pub mod transport;

pub use error::{Error, Result};
pub use events::{
    AppEvent, CONTEXT_METHOD, ConfigChoice, ConfigOptionInfo, FileDiff, GOAL_METHOD, HEAL_METHOD, HarnessEvent, HealInfo,
    LSP_METHOD, ModeInfo, PLUGINS_METHOD, SessionInfo, StopReason, TASKLIST_METHOD, TASK_METHOD, TaskInfo, TaskState, ToolCallInfo,
    ToolCallPatch, ToolKind, ToolStatus, parse_harness,
};
pub use permission::{ApprovalDecision, ChoiceKind, PermissionChoice, PermissionInfo, Unwrapped, unwrap_untrusted};
pub use session::{Attachment, SessionHandle, SessionOptions};
pub use transport::BackendTransport;
