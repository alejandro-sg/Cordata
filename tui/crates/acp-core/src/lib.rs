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
    AppEvent, ConfigChoice, ConfigOptionInfo, FileDiff, HEAL_METHOD, HarnessEvent, HealInfo,
    LAYA_METHOD, LSP_METHOD, LayaDecision, LayaEvent, LayaOutcome, ModeInfo, PLUGINS_METHOD,
    SessionInfo, StopReason, TASK_METHOD, TaskInfo, TaskState, ToolCallInfo, ToolCallPatch,
    ToolKind, ToolStatus, parse_harness, parse_laya,
};
pub use permission::{ApprovalDecision, PermissionInfo, Unwrapped, unwrap_untrusted};
pub use session::{SessionHandle, SessionOptions};
