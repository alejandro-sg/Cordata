//! `AppEvent`: the one vocabulary both binaries consume. This is the single
//! place that encodes backend-specific conventions (tool-kind mapping,
//! untrusted-marker detection, which updates carry replacement vs. appended
//! text) so neither the TUI nor the stress runner re-derives them.

use agent_client_protocol::schema::v1::{
    ContentBlock, SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions,
    SessionModeState, SessionUpdate, StopReason as SchemaStopReason, ToolCallContent,
    ToolCallStatus, ToolKind as SchemaToolKind,
};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::permission::{self, ApprovalDecision, PermissionInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

impl ToolKind {
    pub fn label(self) -> &'static str {
        match self {
            ToolKind::Read => "read",
            ToolKind::Edit => "edit",
            ToolKind::Delete => "delete",
            ToolKind::Move => "move",
            ToolKind::Search => "search",
            ToolKind::Execute => "execute",
            ToolKind::Think => "think",
            ToolKind::Fetch => "fetch",
            ToolKind::SwitchMode => "switch_mode",
            ToolKind::Other => "other",
        }
    }
}

impl From<SchemaToolKind> for ToolKind {
    fn from(kind: SchemaToolKind) -> Self {
        match kind {
            SchemaToolKind::Read => ToolKind::Read,
            SchemaToolKind::Edit => ToolKind::Edit,
            SchemaToolKind::Delete => ToolKind::Delete,
            SchemaToolKind::Move => ToolKind::Move,
            SchemaToolKind::Search => ToolKind::Search,
            SchemaToolKind::Execute => ToolKind::Execute,
            SchemaToolKind::Think => ToolKind::Think,
            SchemaToolKind::Fetch => ToolKind::Fetch,
            SchemaToolKind::SwitchMode => ToolKind::SwitchMode,
            _ => ToolKind::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ToolStatus {
    pub fn label(self) -> &'static str {
        match self {
            ToolStatus::Pending => "pending",
            ToolStatus::InProgress => "in_progress",
            ToolStatus::Completed => "completed",
            ToolStatus::Failed => "failed",
        }
    }
}

impl From<ToolCallStatus> for ToolStatus {
    fn from(status: ToolCallStatus) -> Self {
        match status {
            ToolCallStatus::Pending => ToolStatus::Pending,
            ToolCallStatus::InProgress => ToolStatus::InProgress,
            ToolCallStatus::Completed => ToolStatus::Completed,
            ToolCallStatus::Failed => ToolStatus::Failed,
            _ => ToolStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    Other(String),
}

impl StopReason {
    pub fn as_str(&self) -> &str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::MaxTurnRequests => "max_turn_requests",
            StopReason::Refusal => "refusal",
            StopReason::Cancelled => "cancelled",
            StopReason::Other(other) => other,
        }
    }

    /// `end_turn` is the only reason a scripted run treats as healthy.
    pub fn is_normal(&self) -> bool {
        matches!(self, StopReason::EndTurn)
    }
}

impl From<SchemaStopReason> for StopReason {
    fn from(reason: SchemaStopReason) -> Self {
        match reason {
            SchemaStopReason::EndTurn => StopReason::EndTurn,
            SchemaStopReason::MaxTokens => StopReason::MaxTokens,
            SchemaStopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
            SchemaStopReason::Refusal => StopReason::Refusal,
            SchemaStopReason::Cancelled => StopReason::Cancelled,
            other => StopReason::Other(format!("{other:?}")),
        }
    }
}

/// One file an edit tool call changes: ACP `diff` tool-call content. The
/// backend reads `old_text` from disk before the call runs (None: a new file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub id: String,
    /// The backend titles a tool-call start with the bare tool name.
    pub name: String,
    pub kind: ToolKind,
    pub status: ToolStatus,
    pub raw_input: Option<Value>,
    pub content: String,
    /// The call is to a tool whose output the backend wraps as untrusted.
    pub untrusted_source: bool,
    /// The result text actually contains the untrusted guard markers.
    pub untrusted: bool,
    pub diffs: Vec<FileDiff>,
}

/// A tool-call update. `content`, when present, *replaces* what was shown
/// (swarm progress is a cumulative log; results are final).
#[derive(Debug, Clone, Default)]
pub struct ToolCallPatch {
    pub id: String,
    pub status: Option<ToolStatus>,
    pub content: Option<String>,
    pub untrusted: bool,
    /// The call's complete input (the start event carries only what had streamed).
    pub raw_input: Option<Value>,
    /// What an edit will change, sent before it runs.
    pub diffs: Option<Vec<FileDiff>>,
}

#[derive(Debug, Clone)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConfigChoice {
    pub value: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct ConfigOptionInfo {
    pub id: String,
    pub name: String,
    pub current_value: String,
    pub choices: Vec<ConfigChoice>,
}

impl ConfigOptionInfo {
    pub fn current_name(&self) -> &str {
        self.choices
            .iter()
            .find(|choice| choice.value == self.current_value)
            .map_or(self.current_value.as_str(), |choice| choice.name.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    /// `"stdio"` (this process spawned the backend) or `"ws"` (attached to the
    /// daemon). Shown in the banner: whether a session survives the terminal is
    /// not something a user should have to infer from how they launched.
    pub transport: String,
    pub resumed: bool,
    pub current_mode: String,
    pub modes: Vec<ModeInfo>,
    pub config_options: Vec<ConfigOptionInfo>,
}

/// The extension notification method the backend sends Laya's live triage on.
/// (ACP requires extension methods to start with `_`; a client that doesn't
/// know it must ignore it, so other clients are unaffected.)
pub const LAYA_METHOD: &str = "_cordata/laya";

/// Laya's decision for the turn that just started (backend `laya_triage.py`,
/// or the v2 router `laya_router.py`, which adds the policy fields below).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LayaDecision {
    /// `route` | `shadow`
    pub mode: String,
    /// `None` when Laya gave no decision (see `error`).
    pub route: Option<String>,
    pub route_confidence: f64,
    /// The confidence a route needs before a hint is sent.
    pub gate: f64,
    pub needs_team: f64,
    pub complexity: f64,
    pub complexity_confidence: f64,
    pub latency_ms: Option<f64>,
    /// How long the turn waited for Laya before going on.
    pub waited_ms: Option<f64>,
    /// The hint actually added to the orchestrator's prompt, if any.
    pub hint: Option<String>,
    /// Model tier suggested for sub-agents this turn, if Laya was sure enough.
    pub tier: Option<String>,
    /// Arrived after the turn had already moved on (timed out earlier).
    pub late: bool,
    pub error: Option<String>,
    /// v2: what the router did — `dispatch` | `scope` | `hint` | `escalate` | `none`.
    pub action: Option<String>,
    /// v2: Laya abstained (fail closed): nothing was routed on a guess.
    pub abstained: bool,
    pub reason: Option<String>,
    /// v2: confidence after the online binning map.
    pub calibrated: Option<f64>,
    /// v2: `orchestrator` | `specialist` | `plugin`.
    pub target_kind: Option<String>,
    pub risk: Option<String>,
    /// v2: a confidently destructive turn asks for approval even in auto mode.
    pub armed: bool,
    pub escalate_model: Option<String>,
}

/// What the LLM then actually did, for comparison.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LayaOutcome {
    pub actual_route: String,
    /// `None` when there was no Laya decision to compare against.
    pub agreed: Option<bool>,
    pub tools: Vec<String>,
    pub tiers: Vec<(String, Option<String>)>,
    pub stop_reason: Option<String>,
    pub turn_ms: Option<f64>,
    /// v2: the turn ended without unresolved failures or rejected approvals.
    pub clean: Option<bool>,
    /// v2: log loss of the calibrated route prediction for this turn.
    pub log_loss: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LayaEvent {
    Decision(LayaDecision),
    Outcome(LayaOutcome),
    /// Laya's P(this shell command succeeds), before it runs.
    Command {
        tool_use_id: String,
        p_ok: f64,
    },
    /// The command's real outcome, scored against that prediction.
    CommandOutcome {
        tool_use_id: String,
        ok: bool,
        p_ok: Option<f64>,
        log_loss: Option<f64>,
    },
}

/// Parse the notification's params. Tolerant by design (a backend a version
/// ahead or behind must not break the client): unknown/missing fields default,
/// and anything unrecognisable is `None`.
pub fn parse_laya(params: &Value) -> Option<LayaEvent> {
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
    let number = |key: &str| params.get(key).and_then(Value::as_f64);
    let flag = |key: &str| params.get(key).and_then(Value::as_bool).unwrap_or(false);
    match params.get("event")?.as_str()? {
        "command" => Some(LayaEvent::Command {
            tool_use_id: text("tool_use_id")?,
            p_ok: number("p_ok")?,
        }),
        "command_outcome" => Some(LayaEvent::CommandOutcome {
            tool_use_id: text("tool_use_id")?,
            ok: params.get("ok")?.as_bool()?,
            p_ok: number("p_ok"),
            log_loss: number("log_loss"),
        }),
        "decision" => Some(LayaEvent::Decision(LayaDecision {
            mode: text("mode").unwrap_or_default(),
            route: text("route"),
            route_confidence: number("route_confidence").unwrap_or(0.0),
            gate: number("gate").unwrap_or(0.0),
            needs_team: number("needs_team").unwrap_or(0.0),
            complexity: number("complexity").unwrap_or(0.0),
            complexity_confidence: number("complexity_confidence").unwrap_or(0.0),
            latency_ms: number("latency_ms"),
            waited_ms: number("waited_ms"),
            hint: text("hint"),
            tier: text("tier"),
            late: flag("late"),
            error: text("error"),
            action: text("action"),
            abstained: flag("abstained"),
            reason: text("reason"),
            calibrated: number("calibrated"),
            target_kind: text("target_kind"),
            risk: text("risk"),
            armed: flag("armed"),
            escalate_model: text("escalate_model"),
        })),
        "outcome" => Some(LayaEvent::Outcome(LayaOutcome {
            actual_route: text("actual_route")?,
            agreed: params.get("agreed").and_then(Value::as_bool),
            tools: params
                .get("tools")
                .and_then(Value::as_array)
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            tiers: params
                .get("tiers")
                .and_then(Value::as_object)
                .map(|tiers| {
                    tiers
                        .iter()
                        .map(|(node, tier)| (node.clone(), tier.as_str().map(str::to_owned)))
                        .collect()
                })
                .unwrap_or_default(),
            stop_reason: text("stop_reason"),
            turn_ms: number("turn_ms"),
            clean: params.get("clean").and_then(Value::as_bool),
            log_loss: number("log_loss"),
        })),
        _ => None,
    }
}

/// Harness extension notifications (backend `harness_events.py`).
pub const TASK_METHOD: &str = "_cordata/task";
pub const HEAL_METHOD: &str = "_cordata/heal";
pub const PLUGINS_METHOD: &str = "_cordata/plugins";
pub const LSP_METHOD: &str = "_cordata/lsp";
/// Project snapshots (backend `checkpoints.py`): said when a change could not be snapshotted, when a
/// call writes outside the project, and when a restore ran. Taking one is not an event.
pub const CHECKPOINT_METHOD: &str = "_cordata/checkpoint";
/// A loaded session has an interrupted turn: `auto` says the client may resume it on its own.
pub const RESUME_METHOD: &str = "_cordata/resume";
/// What a finished turn cost (backend `acp_server._announce_turn`): `seconds`, `calls`, `tokensIn`,
/// `tokensOut`, `stop`. Sent once per turn, so a status line can show it without a model call of its own.
pub const TURN_METHOD: &str = "_cordata/turn";
/// The orchestrator's run events (backend `orchestrate_tool.py`): `kind` is `state` | `step_dispatch` |
/// `step_result` | `handoff` | `rollback` | `paused` | `budget_exhausted`, with `run_id` and the fields of that kind.
pub const ORCHESTRATOR_METHOD: &str = "_cordata/orchestrator";
/// `/goal` progress (backend `goal.py`): `event` is `set` | `checking` | `continue` | `met` | `stopped` | `exhausted`,
/// with `condition`, `turn`, `max`, `reason` and `how` (`check` | `judge`).
pub const GOAL_METHOD: &str = "_cordata/goal";
/// How full the conversation is (backend `session_extras.context_snapshot`): `used`, `window`, `pct`, `messages`.
pub const CONTEXT_METHOD: &str = "_cordata/context";
/// The agent's own checklist (backend `task_tool.todo_write`): `items` of `{content, status}`, `done`, `total`.
pub const TASKLIST_METHOD: &str = "_cordata/tasklist";
/// The Experimental Lab's live state (backend `lab.py`): pods, A2A links, the
/// threat gauge, staged HITL rows, resources. Controls go back the other way as
/// `_cordata/lab/{select,isolate,release,dial,inject}` extension requests.
pub const LAB_METHOD: &str = "_cordata/lab";

/// What an agent pod is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PodStatus {
    #[default]
    Idle,
    /// The model call is in flight.
    Synthesizing,
    /// A tool call is running.
    Executing,
    /// Parked on a human decision (HITL) or held by Laya.
    Contained,
    /// E-Stopped: every model and tool call is refused.
    Isolated,
}

impl PodStatus {
    fn parse(text: &str) -> Self {
        match text {
            "synthesizing" => PodStatus::Synthesizing,
            "executing" => PodStatus::Executing,
            "contained" => PodStatus::Contained,
            "isolated" => PodStatus::Isolated,
            _ => PodStatus::Idle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabTrace {
    pub tool: String,
    pub status: String,
    pub input: String,
    pub ms: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabMemory {
    /// `lancedb` | `kg` | `qdrant`
    pub source: String,
    pub query: String,
    pub count: u64,
    /// `(label, score)` of the first few hits.
    pub hits: Vec<(String, Option<f64>)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabPod {
    pub id: String,
    pub name: String,
    pub role: String,
    pub parent: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub status: PodStatus,
    pub isolated: bool,
    /// This pod's Laya destructiveness dial (it only ever tightens the gate).
    pub dial: f64,
    pub dial_default: f64,
    pub calls: u64,
    pub busy_ms: u64,
    pub pending_tool: Option<String>,
    pub guidance_pending: u64,
    pub messages: u64,
    pub trace: Vec<LabTrace>,
    pub memory: Vec<LabMemory>,
    /// The tail of what the pod is writing (the whole text when inspected).
    pub stream: String,
    /// Only present for the pod the client selected (`lab/select`).
    pub system_prompt: Option<String>,
    pub guidance: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabLink {
    pub from: String,
    pub to: String,
    /// `delegation` | `dispatch` | `team` | `pipeline`
    pub kind: String,
    pub active: bool,
    pub count: u64,
    pub label: String,
}

/// Laya's reading for the pending tool call (the turn's destructiveness score).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabThreat {
    pub pod: String,
    pub tool: String,
    pub score: Option<f64>,
    pub confidence: Option<f64>,
    pub band: Option<String>,
    pub pending: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabHitl {
    pub id: String,
    pub pod: String,
    pub tool: String,
    pub input: String,
    /// `pending` | `approved` | `rejected`
    pub status: String,
    pub risk_score: Option<f64>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabResources {
    pub cpu_percent: f64,
    pub rss_mb: f64,
    pub mcp: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabState {
    pub seq: u64,
    pub session_id: Option<String>,
    pub focus: Option<String>,
    pub pods: Vec<LabPod>,
    pub links: Vec<LabLink>,
    pub threat: Option<LabThreat>,
    pub hitl: Vec<LabHitl>,
    pub resources: LabResources,
}

fn parse_lab(params: &Value) -> Option<LabState> {
    if params.get("event").and_then(Value::as_str) != Some("state") {
        return None;
    }
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let number = |value: &Value, key: &str| value.get(key).and_then(Value::as_f64);
    let count = |value: &Value, key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
    let list = |value: &Value, key: &str| -> Vec<Value> {
        value
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let pods = list(params, "pods")
        .iter()
        .filter_map(|pod| {
            let name = text(pod, "name")?;
            Some(LabPod {
                id: text(pod, "id").unwrap_or_else(|| name.clone()),
                role: text(pod, "role").unwrap_or_else(|| name.clone()),
                parent: text(pod, "parent"),
                model: text(pod, "model"),
                effort: text(pod, "effort"),
                status: PodStatus::parse(pod.get("status").and_then(Value::as_str).unwrap_or("")),
                isolated: pod
                    .get("isolated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                dial: number(pod, "dial").unwrap_or(0.8),
                dial_default: number(pod, "dial_default").unwrap_or(0.8),
                calls: count(pod, "calls"),
                busy_ms: count(pod, "busy_ms"),
                pending_tool: text(pod, "pending_tool"),
                guidance_pending: count(pod, "guidance_pending"),
                messages: count(pod, "messages"),
                trace: list(pod, "trace")
                    .iter()
                    .map(|row| LabTrace {
                        tool: text(row, "tool").unwrap_or_default(),
                        status: text(row, "status").unwrap_or_default(),
                        input: text(row, "input").unwrap_or_default(),
                        ms: count(row, "ms"),
                    })
                    .collect(),
                memory: list(pod, "memory")
                    .iter()
                    .map(|row| LabMemory {
                        source: text(row, "source").unwrap_or_default(),
                        query: text(row, "query").unwrap_or_default(),
                        count: count(row, "count"),
                        hits: list(row, "hits")
                            .iter()
                            .map(|hit| {
                                (
                                    text(hit, "label")
                                        .or_else(|| text(hit, "id"))
                                        .unwrap_or_default(),
                                    number(hit, "score"),
                                )
                            })
                            .collect(),
                    })
                    .collect(),
                stream: text(pod, "stream").unwrap_or_default(),
                system_prompt: text(pod, "system_prompt"),
                guidance: list(pod, "guidance")
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect(),
                name,
            })
        })
        .collect();
    let links = list(params, "links")
        .iter()
        .filter_map(|link| {
            Some(LabLink {
                from: text(link, "from")?,
                to: text(link, "to")?,
                kind: text(link, "kind").unwrap_or_default(),
                active: link.get("active").and_then(Value::as_bool).unwrap_or(false),
                count: count(link, "count"),
                label: text(link, "label").unwrap_or_default(),
            })
        })
        .collect();
    let threat = params
        .get("threat")
        .filter(|threat| threat.is_object())
        .map(|threat| LabThreat {
            pod: text(threat, "pod").unwrap_or_default(),
            tool: text(threat, "tool").unwrap_or_default(),
            score: number(threat, "score"),
            confidence: number(threat, "confidence"),
            band: text(threat, "band"),
            pending: threat
                .get("pending")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    let hitl = list(params, "hitl")
        .iter()
        .filter_map(|row| {
            Some(LabHitl {
                id: text(row, "id")?,
                pod: text(row, "pod").unwrap_or_default(),
                tool: text(row, "tool").unwrap_or_default(),
                input: text(row, "input").unwrap_or_default(),
                status: text(row, "status").unwrap_or_else(|| "pending".into()),
                risk_score: row.get("risk").and_then(|risk| number(risk, "score")),
                note: text(row, "note"),
            })
        })
        .collect();
    let resources = params
        .get("resources")
        .map(|resources| LabResources {
            cpu_percent: number(resources, "cpu_percent").unwrap_or(0.0),
            rss_mb: number(resources, "rss_mb").unwrap_or(0.0),
            mcp: list(resources, "mcp")
                .iter()
                .filter_map(|name| name.as_str().map(str::to_owned))
                .collect(),
        })
        .unwrap_or_default();
    Some(LabState {
        seq: count(params, "seq"),
        session_id: text(params, "sessionId"),
        focus: text(params, "focus"),
        pods,
        links,
        threat,
        hitl,
        resources,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Completed,
    Failed,
}

/// One unit of background work: plugin start-up, a language server indexing,
/// an MCP tool reporting progress, the self-heal loop diagnosing / verifying.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskInfo {
    pub id: String,
    pub title: String,
    pub done: f64,
    /// `None`: indeterminate (a spinner, not a bar).
    pub total: Option<f64>,
    pub message: String,
    pub state: TaskState,
    pub source: String,
}

impl TaskInfo {
    /// 0.0..=1.0 when the total is known.
    pub fn fraction(&self) -> Option<f64> {
        self.total
            .filter(|total| *total > 0.0)
            .map(|total| (self.done / total).clamp(0.0, 1.0))
    }
}

/// One phase of the self-healing loop (backend `self_heal.py`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HealInfo {
    /// detected | diagnosing | proposed | verifying | verified | verify_failed |
    /// no_patch | healed | escalated | continuing | state_mismatch | auto_approved
    pub phase: String,
    pub attempt: Option<u64>,
    pub max: Option<u64>,
    pub command: Option<String>,
    pub root_cause: Option<String>,
    pub patch: Option<String>,
    pub verify: Option<String>,
    pub output_tail: Option<String>,
    pub exit_code: Option<i64>,
    pub tool: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HarnessEvent {
    Task(TaskInfo),
    Heal(HealInfo),
    /// A plugin changed state (the backend's plugin row, as JSON).
    Plugin(Value),
    /// A language-server event (started / failed / diagnostics), as JSON.
    Lsp(Value),
    /// A checkpoint notice: `phase` is `unprotected` | `outside` | `restored`.
    Checkpoint(Value),
    /// The loaded session has an interrupted turn (`auto`, `where`, `attempts`, `max`).
    Resume(Value),
    /// A turn ended: what it cost (`seconds`, `calls`, `tokensIn`, `tokensOut`, `stop`).
    Turn(Value),
    /// The Experimental Lab's state: always a full snapshot, so a dropped
    /// notification is healed by the next one.
    Lab(Box<LabState>),
    /// An orchestrator run event (state change, step, handoff), as JSON.
    Orchestrator(Value),
    /// A `/goal` step, as JSON.
    Goal(Value),
    /// The context gauge, as JSON.
    Context(Value),
    /// The agent's checklist, as JSON.
    TaskList(Value),
}

/// Parse a harness notification; `None` for unknown methods or malformed params.
pub fn parse_harness(method: &str, params: &Value) -> Option<HarnessEvent> {
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
    let number = |key: &str| params.get(key).and_then(Value::as_f64);
    match method {
        TASK_METHOD => Some(HarnessEvent::Task(TaskInfo {
            id: text("id")?,
            title: text("title").unwrap_or_default(),
            done: number("done").unwrap_or(0.0),
            total: number("total"),
            message: text("message").unwrap_or_default(),
            state: match params.get("state").and_then(Value::as_str) {
                Some("completed") => TaskState::Completed,
                Some("failed") => TaskState::Failed,
                _ => TaskState::Running,
            },
            source: text("source").unwrap_or_default(),
        })),
        HEAL_METHOD => Some(HarnessEvent::Heal(HealInfo {
            phase: text("phase")?,
            attempt: params.get("attempt").and_then(Value::as_u64),
            max: params.get("max").and_then(Value::as_u64),
            command: text("command"),
            root_cause: text("root_cause"),
            patch: text("patch"),
            verify: text("verify"),
            output_tail: text("output_tail"),
            exit_code: params.get("exit_code").and_then(Value::as_i64),
            tool: text("tool"),
        })),
        PLUGINS_METHOD => params.get("plugin").cloned().map(HarnessEvent::Plugin),
        LSP_METHOD => Some(HarnessEvent::Lsp(params.clone())),
        CHECKPOINT_METHOD => text("phase").map(|_| HarnessEvent::Checkpoint(params.clone())),
        RESUME_METHOD => Some(HarnessEvent::Resume(params.clone())),
        TURN_METHOD => Some(HarnessEvent::Turn(params.clone())),
        LAB_METHOD => parse_lab(params).map(|state| HarnessEvent::Lab(Box::new(state))),
        ORCHESTRATOR_METHOD => text("kind").map(|_| HarnessEvent::Orchestrator(params.clone())),
        GOAL_METHOD => text("event").map(|_| HarnessEvent::Goal(params.clone())),
        CONTEXT_METHOD => params.get("used").map(|_| HarnessEvent::Context(params.clone())),
        TASKLIST_METHOD => params.get("items").filter(|items| items.is_array()).map(|_| HarnessEvent::TaskList(params.clone())),
        _ => None,
    }
}

#[derive(Debug)]
pub enum AppEvent {
    Connected(SessionInfo),
    /// History replayed by `session/load` (text only — the backend does not
    /// replay tool calls). Followed by `ReplayComplete`.
    UserText(String),
    AgentText(String),
    AgentThought(String),
    ReplayComplete,
    ToolCallStarted(ToolCallInfo),
    ToolCallUpdated(ToolCallPatch),
    TitleChanged(String),
    ModeChanged(String),
    ConfigOptionsChanged(Vec<ConfigOptionInfo>),
    /// The server is blocked until `decision` is answered. Dropping the
    /// sender counts as a cancellation (the tool is denied).
    PermissionRequested {
        info: PermissionInfo,
        decision: oneshot::Sender<ApprovalDecision>,
    },
    /// Laya's live triage for the turn (a backend extension; see `LayaEvent`).
    Laya(LayaEvent),
    /// Background tasks, self-heal phases, plugin and LSP changes.
    Harness(HarnessEvent),
    /// The answer to an extension request sent with `SessionHandle::ext`.
    ExtResult {
        tag: String,
        result: Result<Value, String>,
    },
    TurnComplete(StopReason),
    /// The turn's `session/prompt` request failed; the session stays usable.
    TurnFailed(String),
    /// Non-fatal problem (e.g. a rejected set_mode).
    Notice(String),
    /// The connection died; no more events follow except `Closed`.
    ConnectionError(String),
    Closed,
}

pub(crate) fn mode_infos(state: Option<&SessionModeState>) -> (String, Vec<ModeInfo>) {
    match state {
        Some(state) => (
            state.current_mode_id.to_string(),
            state
                .available_modes
                .iter()
                .map(|mode| ModeInfo {
                    id: mode.id.to_string(),
                    name: mode.name.clone(),
                    description: mode.description.clone(),
                })
                .collect(),
        ),
        None => (String::new(), Vec::new()),
    }
}

pub(crate) fn config_infos(options: &[SessionConfigOption]) -> Vec<ConfigOptionInfo> {
    options
        .iter()
        .filter_map(|option| {
            let SessionConfigKind::Select(select) = &option.kind else {
                return None;
            };
            let choices = match &select.options {
                SessionConfigSelectOptions::Ungrouped(list) => list
                    .iter()
                    .map(|choice| ConfigChoice {
                        value: choice.value.to_string(),
                        name: choice.name.clone(),
                    })
                    .collect(),
                SessionConfigSelectOptions::Grouped(groups) => groups
                    .iter()
                    .flat_map(|group| group.options.iter())
                    .map(|choice| ConfigChoice {
                        value: choice.value.to_string(),
                        name: choice.name.clone(),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            Some(ConfigOptionInfo {
                id: option.id.to_string(),
                name: option.name.clone(),
                current_value: select.current_value.to_string(),
                choices,
            })
        })
        .collect()
}

fn text_of(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    }
}

fn diffs_of(content: &[ToolCallContent]) -> Vec<FileDiff> {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Diff(diff) => Some(FileDiff {
                path: diff.path.display().to_string(),
                old_text: diff.old_text.clone(),
                new_text: diff.new_text.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn join_tool_content(content: &[ToolCallContent]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Content(content) => text_of(&content.content),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Translate one `session/update` payload into zero or more events.
pub fn translate_update(update: SessionUpdate) -> Vec<AppEvent> {
    match update {
        SessionUpdate::UserMessageChunk(chunk) => text_of(&chunk.content)
            .map(|text| vec![AppEvent::UserText(text.to_owned())])
            .unwrap_or_default(),
        SessionUpdate::AgentMessageChunk(chunk) => text_of(&chunk.content)
            .map(|text| vec![AppEvent::AgentText(text.to_owned())])
            .unwrap_or_default(),
        SessionUpdate::AgentThoughtChunk(chunk) => text_of(&chunk.content)
            .map(|text| vec![AppEvent::AgentThought(text.to_owned())])
            .unwrap_or_default(),
        SessionUpdate::ToolCall(call) => {
            let content = join_tool_content(&call.content);
            let name = call.name.clone().unwrap_or_else(|| call.title.clone());
            vec![AppEvent::ToolCallStarted(ToolCallInfo {
                id: call.tool_call_id.to_string(),
                untrusted_source: permission::tool_may_return_untrusted(&name),
                untrusted: permission::contains_untrusted_markers(&content),
                name,
                kind: call.kind.into(),
                status: call.status.into(),
                raw_input: call.raw_input,
                diffs: diffs_of(&call.content),
                content,
            })]
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let fields = update.fields;
            let diffs = fields
                .content
                .as_deref()
                .map(diffs_of)
                .filter(|diffs| !diffs.is_empty());
            // A diff-only update (the pre-run preview) must not blank the result text.
            let content = fields
                .content
                .as_deref()
                .filter(|content| {
                    content
                        .iter()
                        .any(|c| matches!(c, ToolCallContent::Content(_)))
                })
                .map(join_tool_content);
            let untrusted = content
                .as_deref()
                .is_some_and(permission::contains_untrusted_markers);
            vec![AppEvent::ToolCallUpdated(ToolCallPatch {
                id: update.tool_call_id.to_string(),
                status: fields.status.map(Into::into),
                content,
                untrusted,
                raw_input: fields.raw_input,
                diffs,
            })]
        }
        SessionUpdate::CurrentModeUpdate(update) => {
            vec![AppEvent::ModeChanged(update.current_mode_id.to_string())]
        }
        SessionUpdate::ConfigOptionUpdate(update) => vec![AppEvent::ConfigOptionsChanged(
            config_infos(&update.config_options),
        )],
        SessionUpdate::SessionInfoUpdate(info) => match info.title {
            agent_client_protocol::schema::MaybeUndefined::Value(title) => {
                vec![AppEvent::TitleChanged(title)]
            }
            _ => Vec::new(),
        },
        // Plan / available-commands / usage updates are never emitted by
        // acp_server.py; ignoring them keeps new protocol variants harmless.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod laya_tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn goal_context_and_checklist_events_parse_only_when_well_formed() {
        assert!(matches!(
            parse_harness(GOAL_METHOD, &serde_json::json!({"event": "continue", "turn": 1, "max": 8})),
            Some(HarnessEvent::Goal(_))
        ));
        assert!(parse_harness(GOAL_METHOD, &serde_json::json!({"turn": 1})).is_none());
        assert!(matches!(
            parse_harness(CONTEXT_METHOD, &serde_json::json!({"used": 1200, "window": 128000, "pct": 0.009})),
            Some(HarnessEvent::Context(_))
        ));
        assert!(parse_harness(CONTEXT_METHOD, &serde_json::json!({"window": 1})).is_none());
        assert!(matches!(
            parse_harness(TASKLIST_METHOD, &serde_json::json!({"items": [{"content": "a", "status": "pending"}]})),
            Some(HarnessEvent::TaskList(_))
        ));
        assert!(parse_harness(TASKLIST_METHOD, &serde_json::json!({"items": "nope"})).is_none());
    }

    #[test]
    fn an_orchestrator_event_parses_when_it_has_a_kind() {
        let event = parse_harness(ORCHESTRATOR_METHOD, &serde_json::json!({"kind": "handoff", "run_id": "r"}));
        assert!(matches!(event, Some(HarnessEvent::Orchestrator(_))));
        assert!(parse_harness(ORCHESTRATOR_METHOD, &serde_json::json!({"run_id": "r"})).is_none());
    }

    #[test]
    fn parses_a_decision() {
        let event = parse_laya(&json!({
            "event": "decision", "mode": "route", "route": "files_and_code", "route_confidence": 0.98,
            "gate": 0.85, "needs_team": 0.14, "complexity": 1.29, "complexity_confidence": 0.63,
            "latency_ms": 81.6, "waited_ms": 81.6, "hint": "use the file tools", "tier": null, "late": false,
            "sessionId": "s"
        }))
        .unwrap();
        let LayaEvent::Decision(decision) = event else {
            panic!()
        };
        assert_eq!(decision.route.as_deref(), Some("files_and_code"));
        assert_eq!((decision.route_confidence, decision.gate), (0.98, 0.85));
        assert_eq!(decision.hint.as_deref(), Some("use the file tools"));
        assert_eq!(decision.tier, None);
        assert!(!decision.late && decision.error.is_none());
    }

    #[test]
    fn parses_an_error_decision_and_an_outcome() {
        let LayaEvent::Decision(error) =
            parse_laya(&json!({"event": "decision", "error": "timed out after 2s"})).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (error.route, error.error.as_deref()),
            (None, Some("timed out after 2s"))
        );

        let LayaEvent::Outcome(outcome) = parse_laya(&json!({
            "event": "outcome", "actual_route": "team", "agreed": false, "tools": ["local_graph"],
            "tiers": {"a": "strong", "b": null}, "stop_reason": "end_turn", "turn_ms": 1200.5
        }))
        .unwrap() else {
            panic!()
        };
        assert_eq!(outcome.agreed, Some(false));
        assert_eq!(outcome.tools, ["local_graph"]);
        assert_eq!(
            outcome.tiers,
            [
                ("a".to_owned(), Some("strong".to_owned())),
                ("b".to_owned(), None)
            ]
        );
    }

    #[test]
    fn parses_v2_router_fields_and_command_predictions() {
        let LayaEvent::Decision(decision) = parse_laya(&json!({
            "event": "decision", "router": "v2", "route": "debugger", "route_confidence": 0.97,
            "calibrated": 0.95, "action": "dispatch", "abstained": false, "target_kind": "specialist",
            "risk": "edits_files", "armed": false, "reason": "dispatched to the debugger specialist"
        }))
        .unwrap() else {
            panic!()
        };
        assert_eq!(decision.action.as_deref(), Some("dispatch"));
        assert_eq!(decision.calibrated, Some(0.95));
        assert!(!decision.abstained);
        assert_eq!(
            parse_laya(&json!({"event": "command", "tool_use_id": "t1", "p_ok": 0.8})),
            Some(LayaEvent::Command {
                tool_use_id: "t1".into(),
                p_ok: 0.8
            })
        );
        assert_eq!(
            parse_laya(
                &json!({"event": "command_outcome", "tool_use_id": "t1", "ok": false, "p_ok": 0.8, "log_loss": 1.6})
            ),
            Some(LayaEvent::CommandOutcome {
                tool_use_id: "t1".into(),
                ok: false,
                p_ok: Some(0.8),
                log_loss: Some(1.6)
            })
        );
    }

    #[test]
    fn parses_harness_tasks_and_heal_phases() {
        let Some(HarnessEvent::Task(task)) = parse_harness(
            TASK_METHOD,
            &json!({"id": "task-3", "title": "lsp: indexing", "done": 30, "total": 120, "state": "running", "source": "lsp"}),
        ) else {
            panic!()
        };
        assert_eq!(task.fraction(), Some(0.25));
        assert_eq!(task.state, TaskState::Running);
        let Some(HarnessEvent::Heal(heal)) = parse_harness(
            HEAL_METHOD,
            &json!({"event": "heal", "phase": "verified", "attempt": 1, "max": 3, "command": "cargo test", "patch": "--- a/x\n+++ b/x\n"}),
        ) else {
            panic!()
        };
        assert_eq!(
            (heal.phase.as_str(), heal.attempt, heal.max),
            ("verified", Some(1), Some(3))
        );
        assert_eq!(parse_harness(TASK_METHOD, &json!({"title": "no id"})), None);
        assert_eq!(parse_harness("_cordata/unknown", &json!({})), None);
        let Some(HarnessEvent::Turn(turn)) = parse_harness(
            TURN_METHOD,
            &json!({"stop": "end_turn", "seconds": 41.3, "calls": 3, "tokensIn": 12400, "tokensOut": 1100}),
        ) else {
            panic!("a turn receipt parses");
        };
        assert_eq!(turn["tokensIn"], 12400);
    }

    #[test]
    fn parses_a_lab_snapshot() {
        let Some(HarnessEvent::Lab(lab)) = parse_harness(
            LAB_METHOD,
            &json!({
                "event": "state", "version": 1, "seq": 7, "sessionId": "s1", "focus": "debugger",
                "pods": [
                    {"id": "orchestrator", "name": "orchestrator", "role": "orchestrator", "parent": null,
                     "model": "glm-5.3-flash", "effort": "low", "status": "executing", "isolated": false,
                     "dial": 0.8, "dial_default": 0.8, "calls": 3, "busy_ms": 1200, "pending_tool": "shell",
                     "guidance_pending": 1, "messages": 4,
                     "trace": [{"tool": "shell", "status": "running", "input": "echo hi", "ms": 0}],
                     "memory": [{"source": "lancedb", "query": "cargo", "count": 2, "hits": [{"id": "t1", "label": "cargo test", "score": 0.12}]}],
                     "stream": "thinking…"},
                    {"id": "debugger", "name": "debugger", "role": "debugger", "parent": "orchestrator",
                     "model": "kimi-k2.7-code", "effort": "max", "status": "contained", "isolated": false,
                     "dial": 0.5, "system_prompt": "You are the Debugger", "guidance": ["use pytest"]}
                ],
                "links": [{"from": "orchestrator", "to": "debugger", "kind": "delegation", "active": true, "count": 2, "label": "fix"}],
                "threat": {"pod": "debugger", "tool": "shell", "score": 0.92, "confidence": 0.88, "band": "destructive", "pending": true},
                "hitl": [{"id": "i1", "pod": "debugger", "tool": "apply_patch", "input": "{}", "status": "pending", "risk": {"score": 0.9}}],
                "resources": {"cpu_percent": 12.5, "rss_mb": 480.0, "mcp": ["pihole"]}
            }),
        ) else {
            panic!()
        };
        assert_eq!((lab.seq, lab.focus.as_deref()), (7, Some("debugger")));
        assert_eq!(lab.pods.len(), 2);
        let orchestrator = &lab.pods[0];
        assert_eq!(orchestrator.status, PodStatus::Executing);
        assert_eq!(orchestrator.effort.as_deref(), Some("low"));
        assert_eq!(
            orchestrator.memory[0].hits[0],
            ("cargo test".to_owned(), Some(0.12))
        );
        assert_eq!(orchestrator.trace[0].tool, "shell");
        let debugger = &lab.pods[1];
        assert_eq!(
            (debugger.status, debugger.dial, debugger.parent.as_deref()),
            (PodStatus::Contained, 0.5, Some("orchestrator"))
        );
        assert_eq!(
            debugger.system_prompt.as_deref(),
            Some("You are the Debugger")
        );
        assert_eq!(lab.links[0].count, 2);
        assert!(lab.links[0].active);
        let threat = lab.threat.unwrap();
        assert_eq!((threat.score, threat.confidence), (Some(0.92), Some(0.88)));
        assert_eq!(lab.hitl[0].risk_score, Some(0.9));
        assert_eq!(lab.resources.mcp, ["pihole"]);
    }

    #[test]
    fn a_sparse_or_future_lab_payload_is_tolerated() {
        let Some(HarnessEvent::Lab(lab)) = parse_harness(
            LAB_METHOD,
            &json!({"event": "state", "pods": [{"name": "solo", "status": "from_the_future"}, {"no_name": 1}], "threat": null}),
        ) else {
            panic!()
        };
        assert_eq!(
            lab.pods.len(),
            1,
            "a pod without a name is skipped, not fatal"
        );
        assert_eq!(
            lab.pods[0].status,
            PodStatus::Idle,
            "an unknown status reads as idle"
        );
        assert!(lab.threat.is_none() && lab.links.is_empty());
        assert_eq!(
            parse_harness(LAB_METHOD, &json!({"event": "something_else"})),
            None
        );
    }

    #[test]
    fn unknown_or_malformed_payloads_are_ignored_not_fatal() {
        assert_eq!(parse_laya(&json!({"event": "from_the_future"})), None);
        assert_eq!(parse_laya(&json!({"no_event": 1})), None);
        assert_eq!(parse_laya(&json!("nonsense")), None);
        assert_eq!(
            parse_laya(&json!({"event": "outcome"})),
            None,
            "an outcome needs its actual route"
        );
    }
}
