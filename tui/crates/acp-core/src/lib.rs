//! All ACP protocol logic for the Cordata terminal clients. Zero UI
//! dependencies: `acp-tui` and `acp-stress` both consume `SessionHandle` +
//! `AppEvent`, so the connection/session/permission code exists exactly once.

pub mod config;
pub mod connection;
pub mod error;
pub mod events;
pub mod hub;
pub mod permission;
pub mod session;
pub mod transport;

pub use config::{BackendConfig, DaemonConfig};
pub use error::{Error, Result};
pub use events::{
    AppEvent, CONTEXT_METHOD, ConfigChoice, ConfigOptionInfo, FileDiff, GOAL_METHOD, HEAL_METHOD, HarnessEvent, HealInfo,
    LAB_METHOD, LAYA_METHOD, LSP_METHOD, LabHitl, LabLink, LabMemory, LabPod, LabResources,
    LabState, LabThreat, LabTrace, LayaDecision, LayaEvent, LayaOutcome, ModeInfo, PLUGINS_METHOD,
    PodStatus, SessionInfo, StopReason, TASKLIST_METHOD, TASK_METHOD, TaskInfo, TaskState, ToolCallInfo,
    ToolCallPatch, ToolKind, ToolStatus, parse_harness, parse_laya,
};
pub use permission::{ApprovalDecision, ChoiceKind, PermissionChoice, PermissionInfo, Unwrapped, unwrap_untrusted};
pub use session::{Attachment, SessionHandle, SessionOptions};
pub use transport::{BackendTransport, WsTransport};
