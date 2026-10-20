from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class PodStatus(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    POD_STATUS_UNSPECIFIED: _ClassVar[PodStatus]
    POD_STATUS_IDLE: _ClassVar[PodStatus]
    POD_STATUS_SYNTHESIZING: _ClassVar[PodStatus]
    POD_STATUS_EXECUTING: _ClassVar[PodStatus]
    POD_STATUS_CONTAINED: _ClassVar[PodStatus]
    POD_STATUS_ISOLATED: _ClassVar[PodStatus]

class PayloadKind(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    PAYLOAD_KIND_UNSPECIFIED: _ClassVar[PayloadKind]
    PAYLOAD_KIND_SHELL_COMMAND: _ClassVar[PayloadKind]
    PAYLOAD_KIND_FILE_DIFF: _ClassVar[PayloadKind]
    PAYLOAD_KIND_OTHER: _ClassVar[PayloadKind]

class Outcome(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    OUTCOME_UNSPECIFIED: _ClassVar[Outcome]
    OUTCOME_APPROVED: _ClassVar[Outcome]
    OUTCOME_REJECTED: _ClassVar[Outcome]
    OUTCOME_ABORTED: _ClassVar[Outcome]

class PodAction(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    POD_ACTION_UNSPECIFIED: _ClassVar[PodAction]
    POD_ACTION_ISOLATE: _ClassVar[PodAction]
    POD_ACTION_RELEASE: _ClassVar[PodAction]
    POD_ACTION_STEER: _ClassVar[PodAction]
POD_STATUS_UNSPECIFIED: PodStatus
POD_STATUS_IDLE: PodStatus
POD_STATUS_SYNTHESIZING: PodStatus
POD_STATUS_EXECUTING: PodStatus
POD_STATUS_CONTAINED: PodStatus
POD_STATUS_ISOLATED: PodStatus
PAYLOAD_KIND_UNSPECIFIED: PayloadKind
PAYLOAD_KIND_SHELL_COMMAND: PayloadKind
PAYLOAD_KIND_FILE_DIFF: PayloadKind
PAYLOAD_KIND_OTHER: PayloadKind
OUTCOME_UNSPECIFIED: Outcome
OUTCOME_APPROVED: Outcome
OUTCOME_REJECTED: Outcome
OUTCOME_ABORTED: Outcome
POD_ACTION_UNSPECIFIED: PodAction
POD_ACTION_ISOLATE: PodAction
POD_ACTION_RELEASE: PodAction
POD_ACTION_STEER: PodAction

class TelemetryRequest(_message.Message):
    __slots__ = ("session_id",)
    SESSION_ID_FIELD_NUMBER: _ClassVar[int]
    session_id: str
    def __init__(self, session_id: _Optional[str] = ...) -> None: ...

class TelemetryFrame(_message.Message):
    __slots__ = ("seq", "ts_ms", "dropped", "session_id", "lab", "omlx", "daemon", "diffs", "pending", "memory_event")
    SEQ_FIELD_NUMBER: _ClassVar[int]
    TS_MS_FIELD_NUMBER: _ClassVar[int]
    DROPPED_FIELD_NUMBER: _ClassVar[int]
    SESSION_ID_FIELD_NUMBER: _ClassVar[int]
    LAB_FIELD_NUMBER: _ClassVar[int]
    OMLX_FIELD_NUMBER: _ClassVar[int]
    DAEMON_FIELD_NUMBER: _ClassVar[int]
    DIFFS_FIELD_NUMBER: _ClassVar[int]
    PENDING_FIELD_NUMBER: _ClassVar[int]
    MEMORY_EVENT_FIELD_NUMBER: _ClassVar[int]
    seq: int
    ts_ms: int
    dropped: int
    session_id: str
    lab: LabState
    omlx: OmlxState
    daemon: DaemonState
    diffs: DiffSet
    pending: PendingSet
    memory_event: MemoryEvent
    def __init__(self, seq: _Optional[int] = ..., ts_ms: _Optional[int] = ..., dropped: _Optional[int] = ..., session_id: _Optional[str] = ..., lab: _Optional[_Union[LabState, _Mapping]] = ..., omlx: _Optional[_Union[OmlxState, _Mapping]] = ..., daemon: _Optional[_Union[DaemonState, _Mapping]] = ..., diffs: _Optional[_Union[DiffSet, _Mapping]] = ..., pending: _Optional[_Union[PendingSet, _Mapping]] = ..., memory_event: _Optional[_Union[MemoryEvent, _Mapping]] = ...) -> None: ...

class TraceRow(_message.Message):
    __slots__ = ("tool", "status", "input", "ms", "ts_ms")
    TOOL_FIELD_NUMBER: _ClassVar[int]
    STATUS_FIELD_NUMBER: _ClassVar[int]
    INPUT_FIELD_NUMBER: _ClassVar[int]
    MS_FIELD_NUMBER: _ClassVar[int]
    TS_MS_FIELD_NUMBER: _ClassVar[int]
    tool: str
    status: str
    input: str
    ms: int
    ts_ms: int
    def __init__(self, tool: _Optional[str] = ..., status: _Optional[str] = ..., input: _Optional[str] = ..., ms: _Optional[int] = ..., ts_ms: _Optional[int] = ...) -> None: ...

class Pod(_message.Message):
    __slots__ = ("id", "name", "role", "parent", "model", "effort", "status", "isolated", "dial", "dial_default", "calls", "busy_ms", "pending_tool", "guidance_pending", "stream_tail", "trace", "effort_adjustable", "since_ms", "risk_last", "risk_peak", "auto_approved", "asked", "reasoning_tail", "task", "ephemeral")
    ID_FIELD_NUMBER: _ClassVar[int]
    NAME_FIELD_NUMBER: _ClassVar[int]
    ROLE_FIELD_NUMBER: _ClassVar[int]
    PARENT_FIELD_NUMBER: _ClassVar[int]
    MODEL_FIELD_NUMBER: _ClassVar[int]
    EFFORT_FIELD_NUMBER: _ClassVar[int]
    STATUS_FIELD_NUMBER: _ClassVar[int]
    ISOLATED_FIELD_NUMBER: _ClassVar[int]
    DIAL_FIELD_NUMBER: _ClassVar[int]
    DIAL_DEFAULT_FIELD_NUMBER: _ClassVar[int]
    CALLS_FIELD_NUMBER: _ClassVar[int]
    BUSY_MS_FIELD_NUMBER: _ClassVar[int]
    PENDING_TOOL_FIELD_NUMBER: _ClassVar[int]
    GUIDANCE_PENDING_FIELD_NUMBER: _ClassVar[int]
    STREAM_TAIL_FIELD_NUMBER: _ClassVar[int]
    TRACE_FIELD_NUMBER: _ClassVar[int]
    EFFORT_ADJUSTABLE_FIELD_NUMBER: _ClassVar[int]
    SINCE_MS_FIELD_NUMBER: _ClassVar[int]
    RISK_LAST_FIELD_NUMBER: _ClassVar[int]
    RISK_PEAK_FIELD_NUMBER: _ClassVar[int]
    AUTO_APPROVED_FIELD_NUMBER: _ClassVar[int]
    ASKED_FIELD_NUMBER: _ClassVar[int]
    REASONING_TAIL_FIELD_NUMBER: _ClassVar[int]
    TASK_FIELD_NUMBER: _ClassVar[int]
    EPHEMERAL_FIELD_NUMBER: _ClassVar[int]
    id: str
    name: str
    role: str
    parent: str
    model: str
    effort: str
    status: PodStatus
    isolated: bool
    dial: float
    dial_default: float
    calls: int
    busy_ms: int
    pending_tool: str
    guidance_pending: int
    stream_tail: str
    trace: _containers.RepeatedCompositeFieldContainer[TraceRow]
    effort_adjustable: bool
    since_ms: int
    risk_last: float
    risk_peak: float
    auto_approved: int
    asked: int
    reasoning_tail: str
    task: str
    ephemeral: bool
    def __init__(self, id: _Optional[str] = ..., name: _Optional[str] = ..., role: _Optional[str] = ..., parent: _Optional[str] = ..., model: _Optional[str] = ..., effort: _Optional[str] = ..., status: _Optional[_Union[PodStatus, str]] = ..., isolated: _Optional[bool] = ..., dial: _Optional[float] = ..., dial_default: _Optional[float] = ..., calls: _Optional[int] = ..., busy_ms: _Optional[int] = ..., pending_tool: _Optional[str] = ..., guidance_pending: _Optional[int] = ..., stream_tail: _Optional[str] = ..., trace: _Optional[_Iterable[_Union[TraceRow, _Mapping]]] = ..., effort_adjustable: _Optional[bool] = ..., since_ms: _Optional[int] = ..., risk_last: _Optional[float] = ..., risk_peak: _Optional[float] = ..., auto_approved: _Optional[int] = ..., asked: _Optional[int] = ..., reasoning_tail: _Optional[str] = ..., task: _Optional[str] = ..., ephemeral: _Optional[bool] = ...) -> None: ...

class Link(_message.Message):
    __slots__ = ("to", "kind", "active", "count", "label")
    FROM_FIELD_NUMBER: _ClassVar[int]
    TO_FIELD_NUMBER: _ClassVar[int]
    KIND_FIELD_NUMBER: _ClassVar[int]
    ACTIVE_FIELD_NUMBER: _ClassVar[int]
    COUNT_FIELD_NUMBER: _ClassVar[int]
    LABEL_FIELD_NUMBER: _ClassVar[int]
    to: str
    kind: str
    active: bool
    count: int
    label: str
    def __init__(self, to: _Optional[str] = ..., kind: _Optional[str] = ..., active: _Optional[bool] = ..., count: _Optional[int] = ..., label: _Optional[str] = ..., **kwargs) -> None: ...

class Threat(_message.Message):
    __slots__ = ("pod", "tool", "score", "confidence", "band", "pending")
    POD_FIELD_NUMBER: _ClassVar[int]
    TOOL_FIELD_NUMBER: _ClassVar[int]
    SCORE_FIELD_NUMBER: _ClassVar[int]
    CONFIDENCE_FIELD_NUMBER: _ClassVar[int]
    BAND_FIELD_NUMBER: _ClassVar[int]
    PENDING_FIELD_NUMBER: _ClassVar[int]
    pod: str
    tool: str
    score: float
    confidence: float
    band: str
    pending: bool
    def __init__(self, pod: _Optional[str] = ..., tool: _Optional[str] = ..., score: _Optional[float] = ..., confidence: _Optional[float] = ..., band: _Optional[str] = ..., pending: _Optional[bool] = ...) -> None: ...

class Resources(_message.Message):
    __slots__ = ("cpu_percent", "rss_mb", "mcp")
    CPU_PERCENT_FIELD_NUMBER: _ClassVar[int]
    RSS_MB_FIELD_NUMBER: _ClassVar[int]
    MCP_FIELD_NUMBER: _ClassVar[int]
    cpu_percent: float
    rss_mb: float
    mcp: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, cpu_percent: _Optional[float] = ..., rss_mb: _Optional[float] = ..., mcp: _Optional[_Iterable[str]] = ...) -> None: ...

class LabState(_message.Message):
    __slots__ = ("lab_seq", "focus", "pods", "links", "threat", "resources", "default_dial", "mode", "mission", "mission_ms", "memory")
    LAB_SEQ_FIELD_NUMBER: _ClassVar[int]
    FOCUS_FIELD_NUMBER: _ClassVar[int]
    PODS_FIELD_NUMBER: _ClassVar[int]
    LINKS_FIELD_NUMBER: _ClassVar[int]
    THREAT_FIELD_NUMBER: _ClassVar[int]
    RESOURCES_FIELD_NUMBER: _ClassVar[int]
    DEFAULT_DIAL_FIELD_NUMBER: _ClassVar[int]
    MODE_FIELD_NUMBER: _ClassVar[int]
    MISSION_FIELD_NUMBER: _ClassVar[int]
    MISSION_MS_FIELD_NUMBER: _ClassVar[int]
    MEMORY_FIELD_NUMBER: _ClassVar[int]
    lab_seq: int
    focus: str
    pods: _containers.RepeatedCompositeFieldContainer[Pod]
    links: _containers.RepeatedCompositeFieldContainer[Link]
    threat: Threat
    resources: Resources
    default_dial: float
    mode: str
    mission: str
    mission_ms: int
    memory: _containers.RepeatedCompositeFieldContainer[MemoryHit]
    def __init__(self, lab_seq: _Optional[int] = ..., focus: _Optional[str] = ..., pods: _Optional[_Iterable[_Union[Pod, _Mapping]]] = ..., links: _Optional[_Iterable[_Union[Link, _Mapping]]] = ..., threat: _Optional[_Union[Threat, _Mapping]] = ..., resources: _Optional[_Union[Resources, _Mapping]] = ..., default_dial: _Optional[float] = ..., mode: _Optional[str] = ..., mission: _Optional[str] = ..., mission_ms: _Optional[int] = ..., memory: _Optional[_Iterable[_Union[MemoryHit, _Mapping]]] = ...) -> None: ...

class OmlxModel(_message.Message):
    __slots__ = ("id", "loaded", "size_gb", "pinned")
    ID_FIELD_NUMBER: _ClassVar[int]
    LOADED_FIELD_NUMBER: _ClassVar[int]
    SIZE_GB_FIELD_NUMBER: _ClassVar[int]
    PINNED_FIELD_NUMBER: _ClassVar[int]
    id: str
    loaded: bool
    size_gb: float
    pinned: bool
    def __init__(self, id: _Optional[str] = ..., loaded: _Optional[bool] = ..., size_gb: _Optional[float] = ..., pinned: _Optional[bool] = ...) -> None: ...

class OmlxState(_message.Message):
    __slots__ = ("up", "pool_gb", "loaded_gb", "budget_gb", "used_gb", "free_gb", "models", "ollama_resident")
    UP_FIELD_NUMBER: _ClassVar[int]
    POOL_GB_FIELD_NUMBER: _ClassVar[int]
    LOADED_GB_FIELD_NUMBER: _ClassVar[int]
    BUDGET_GB_FIELD_NUMBER: _ClassVar[int]
    USED_GB_FIELD_NUMBER: _ClassVar[int]
    FREE_GB_FIELD_NUMBER: _ClassVar[int]
    MODELS_FIELD_NUMBER: _ClassVar[int]
    OLLAMA_RESIDENT_FIELD_NUMBER: _ClassVar[int]
    up: bool
    pool_gb: float
    loaded_gb: float
    budget_gb: float
    used_gb: float
    free_gb: float
    models: _containers.RepeatedCompositeFieldContainer[OmlxModel]
    ollama_resident: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, up: _Optional[bool] = ..., pool_gb: _Optional[float] = ..., loaded_gb: _Optional[float] = ..., budget_gb: _Optional[float] = ..., used_gb: _Optional[float] = ..., free_gb: _Optional[float] = ..., models: _Optional[_Iterable[_Union[OmlxModel, _Mapping]]] = ..., ollama_resident: _Optional[_Iterable[str]] = ...) -> None: ...

class DaemonState(_message.Message):
    __slots__ = ("version", "uptime_s", "sessions", "alive", "observer_enabled", "ingest_accepted", "ingest_rejected", "telemetry_subscribers")
    VERSION_FIELD_NUMBER: _ClassVar[int]
    UPTIME_S_FIELD_NUMBER: _ClassVar[int]
    SESSIONS_FIELD_NUMBER: _ClassVar[int]
    ALIVE_FIELD_NUMBER: _ClassVar[int]
    OBSERVER_ENABLED_FIELD_NUMBER: _ClassVar[int]
    INGEST_ACCEPTED_FIELD_NUMBER: _ClassVar[int]
    INGEST_REJECTED_FIELD_NUMBER: _ClassVar[int]
    TELEMETRY_SUBSCRIBERS_FIELD_NUMBER: _ClassVar[int]
    version: str
    uptime_s: float
    sessions: int
    alive: int
    observer_enabled: bool
    ingest_accepted: int
    ingest_rejected: int
    telemetry_subscribers: int
    def __init__(self, version: _Optional[str] = ..., uptime_s: _Optional[float] = ..., sessions: _Optional[int] = ..., alive: _Optional[int] = ..., observer_enabled: _Optional[bool] = ..., ingest_accepted: _Optional[int] = ..., ingest_rejected: _Optional[int] = ..., telemetry_subscribers: _Optional[int] = ...) -> None: ...

class DiffSet(_message.Message):
    __slots__ = ("items",)
    ITEMS_FIELD_NUMBER: _ClassVar[int]
    items: _containers.RepeatedCompositeFieldContainer[DiffRef]
    def __init__(self, items: _Optional[_Iterable[_Union[DiffRef, _Mapping]]] = ...) -> None: ...

class PendingSet(_message.Message):
    __slots__ = ("items",)
    ITEMS_FIELD_NUMBER: _ClassVar[int]
    items: _containers.RepeatedCompositeFieldContainer[PendingIntervention]
    def __init__(self, items: _Optional[_Iterable[_Union[PendingIntervention, _Mapping]]] = ...) -> None: ...

class DiffRef(_message.Message):
    __slots__ = ("id", "source", "files", "lines", "truncated", "ts_ms", "stat", "session_id")
    ID_FIELD_NUMBER: _ClassVar[int]
    SOURCE_FIELD_NUMBER: _ClassVar[int]
    FILES_FIELD_NUMBER: _ClassVar[int]
    LINES_FIELD_NUMBER: _ClassVar[int]
    TRUNCATED_FIELD_NUMBER: _ClassVar[int]
    TS_MS_FIELD_NUMBER: _ClassVar[int]
    STAT_FIELD_NUMBER: _ClassVar[int]
    SESSION_ID_FIELD_NUMBER: _ClassVar[int]
    id: str
    source: str
    files: _containers.RepeatedScalarFieldContainer[str]
    lines: int
    truncated: bool
    ts_ms: int
    stat: str
    session_id: str
    def __init__(self, id: _Optional[str] = ..., source: _Optional[str] = ..., files: _Optional[_Iterable[str]] = ..., lines: _Optional[int] = ..., truncated: _Optional[bool] = ..., ts_ms: _Optional[int] = ..., stat: _Optional[str] = ..., session_id: _Optional[str] = ...) -> None: ...

class MemoryEvent(_message.Message):
    __slots__ = ("event", "runtime", "model", "detail")
    EVENT_FIELD_NUMBER: _ClassVar[int]
    RUNTIME_FIELD_NUMBER: _ClassVar[int]
    MODEL_FIELD_NUMBER: _ClassVar[int]
    DETAIL_FIELD_NUMBER: _ClassVar[int]
    event: str
    runtime: str
    model: str
    detail: str
    def __init__(self, event: _Optional[str] = ..., runtime: _Optional[str] = ..., model: _Optional[str] = ..., detail: _Optional[str] = ...) -> None: ...

class MemoryHitEntry(_message.Message):
    __slots__ = ("id", "label", "score")
    ID_FIELD_NUMBER: _ClassVar[int]
    LABEL_FIELD_NUMBER: _ClassVar[int]
    SCORE_FIELD_NUMBER: _ClassVar[int]
    id: str
    label: str
    score: float
    def __init__(self, id: _Optional[str] = ..., label: _Optional[str] = ..., score: _Optional[float] = ...) -> None: ...

class MemoryHit(_message.Message):
    __slots__ = ("pod", "source", "query", "count", "hits", "ts_ms")
    POD_FIELD_NUMBER: _ClassVar[int]
    SOURCE_FIELD_NUMBER: _ClassVar[int]
    QUERY_FIELD_NUMBER: _ClassVar[int]
    COUNT_FIELD_NUMBER: _ClassVar[int]
    HITS_FIELD_NUMBER: _ClassVar[int]
    TS_MS_FIELD_NUMBER: _ClassVar[int]
    pod: str
    source: str
    query: str
    count: int
    hits: _containers.RepeatedCompositeFieldContainer[MemoryHitEntry]
    ts_ms: int
    def __init__(self, pod: _Optional[str] = ..., source: _Optional[str] = ..., query: _Optional[str] = ..., count: _Optional[int] = ..., hits: _Optional[_Iterable[_Union[MemoryHitEntry, _Mapping]]] = ..., ts_ms: _Optional[int] = ...) -> None: ...

class InterventionRequest(_message.Message):
    __slots__ = ("intervention_id", "session_id", "pod", "tool", "kind", "command", "diff", "input_json", "risk_score", "risk_confidence", "risk_band", "armed", "reason", "requested_ms", "redactions", "options", "goal", "said", "project", "tool_call_id", "conversation")
    INTERVENTION_ID_FIELD_NUMBER: _ClassVar[int]
    SESSION_ID_FIELD_NUMBER: _ClassVar[int]
    POD_FIELD_NUMBER: _ClassVar[int]
    TOOL_FIELD_NUMBER: _ClassVar[int]
    KIND_FIELD_NUMBER: _ClassVar[int]
    COMMAND_FIELD_NUMBER: _ClassVar[int]
    DIFF_FIELD_NUMBER: _ClassVar[int]
    INPUT_JSON_FIELD_NUMBER: _ClassVar[int]
    RISK_SCORE_FIELD_NUMBER: _ClassVar[int]
    RISK_CONFIDENCE_FIELD_NUMBER: _ClassVar[int]
    RISK_BAND_FIELD_NUMBER: _ClassVar[int]
    ARMED_FIELD_NUMBER: _ClassVar[int]
    REASON_FIELD_NUMBER: _ClassVar[int]
    REQUESTED_MS_FIELD_NUMBER: _ClassVar[int]
    REDACTIONS_FIELD_NUMBER: _ClassVar[int]
    OPTIONS_FIELD_NUMBER: _ClassVar[int]
    GOAL_FIELD_NUMBER: _ClassVar[int]
    SAID_FIELD_NUMBER: _ClassVar[int]
    PROJECT_FIELD_NUMBER: _ClassVar[int]
    TOOL_CALL_ID_FIELD_NUMBER: _ClassVar[int]
    CONVERSATION_FIELD_NUMBER: _ClassVar[int]
    intervention_id: str
    session_id: str
    pod: str
    tool: str
    kind: PayloadKind
    command: str
    diff: str
    input_json: str
    risk_score: float
    risk_confidence: float
    risk_band: str
    armed: bool
    reason: str
    requested_ms: int
    redactions: int
    options: _containers.RepeatedCompositeFieldContainer[ApprovalOption]
    goal: str
    said: str
    project: str
    tool_call_id: str
    conversation: str
    def __init__(self, intervention_id: _Optional[str] = ..., session_id: _Optional[str] = ..., pod: _Optional[str] = ..., tool: _Optional[str] = ..., kind: _Optional[_Union[PayloadKind, str]] = ..., command: _Optional[str] = ..., diff: _Optional[str] = ..., input_json: _Optional[str] = ..., risk_score: _Optional[float] = ..., risk_confidence: _Optional[float] = ..., risk_band: _Optional[str] = ..., armed: _Optional[bool] = ..., reason: _Optional[str] = ..., requested_ms: _Optional[int] = ..., redactions: _Optional[int] = ..., options: _Optional[_Iterable[_Union[ApprovalOption, _Mapping]]] = ..., goal: _Optional[str] = ..., said: _Optional[str] = ..., project: _Optional[str] = ..., tool_call_id: _Optional[str] = ..., conversation: _Optional[str] = ...) -> None: ...

class ApprovalOption(_message.Message):
    __slots__ = ("option_id", "name", "kind")
    OPTION_ID_FIELD_NUMBER: _ClassVar[int]
    NAME_FIELD_NUMBER: _ClassVar[int]
    KIND_FIELD_NUMBER: _ClassVar[int]
    option_id: str
    name: str
    kind: str
    def __init__(self, option_id: _Optional[str] = ..., name: _Optional[str] = ..., kind: _Optional[str] = ...) -> None: ...

class PendingIntervention(_message.Message):
    __slots__ = ("request",)
    REQUEST_FIELD_NUMBER: _ClassVar[int]
    request: InterventionRequest
    def __init__(self, request: _Optional[_Union[InterventionRequest, _Mapping]] = ...) -> None: ...

class InterventionResolved(_message.Message):
    __slots__ = ("intervention_id", "outcome", "decided_by")
    INTERVENTION_ID_FIELD_NUMBER: _ClassVar[int]
    OUTCOME_FIELD_NUMBER: _ClassVar[int]
    DECIDED_BY_FIELD_NUMBER: _ClassVar[int]
    intervention_id: str
    outcome: Outcome
    decided_by: str
    def __init__(self, intervention_id: _Optional[str] = ..., outcome: _Optional[_Union[Outcome, str]] = ..., decided_by: _Optional[str] = ...) -> None: ...

class Heartbeat(_message.Message):
    __slots__ = ("ts_ms",)
    TS_MS_FIELD_NUMBER: _ClassVar[int]
    ts_ms: int
    def __init__(self, ts_ms: _Optional[int] = ...) -> None: ...

class InterventionEvent(_message.Message):
    __slots__ = ("request", "resolved", "heartbeat")
    REQUEST_FIELD_NUMBER: _ClassVar[int]
    RESOLVED_FIELD_NUMBER: _ClassVar[int]
    HEARTBEAT_FIELD_NUMBER: _ClassVar[int]
    request: InterventionRequest
    resolved: InterventionResolved
    heartbeat: Heartbeat
    def __init__(self, request: _Optional[_Union[InterventionRequest, _Mapping]] = ..., resolved: _Optional[_Union[InterventionResolved, _Mapping]] = ..., heartbeat: _Optional[_Union[Heartbeat, _Mapping]] = ...) -> None: ...

class InterventionHello(_message.Message):
    __slots__ = ("client",)
    CLIENT_FIELD_NUMBER: _ClassVar[int]
    client: str
    def __init__(self, client: _Optional[str] = ...) -> None: ...

class InterventionDecision(_message.Message):
    __slots__ = ("intervention_id", "approve", "isolate", "note", "option_id")
    INTERVENTION_ID_FIELD_NUMBER: _ClassVar[int]
    APPROVE_FIELD_NUMBER: _ClassVar[int]
    ISOLATE_FIELD_NUMBER: _ClassVar[int]
    NOTE_FIELD_NUMBER: _ClassVar[int]
    OPTION_ID_FIELD_NUMBER: _ClassVar[int]
    intervention_id: str
    approve: bool
    isolate: bool
    note: str
    option_id: str
    def __init__(self, intervention_id: _Optional[str] = ..., approve: _Optional[bool] = ..., isolate: _Optional[bool] = ..., note: _Optional[str] = ..., option_id: _Optional[str] = ...) -> None: ...

class InterventionReply(_message.Message):
    __slots__ = ("hello", "decision")
    HELLO_FIELD_NUMBER: _ClassVar[int]
    DECISION_FIELD_NUMBER: _ClassVar[int]
    hello: InterventionHello
    decision: InterventionDecision
    def __init__(self, hello: _Optional[_Union[InterventionHello, _Mapping]] = ..., decision: _Optional[_Union[InterventionDecision, _Mapping]] = ...) -> None: ...

class AstSnippet(_message.Message):
    __slots__ = ("workspace", "path", "language", "node_kind", "name", "start_line", "end_line", "text", "content_hash", "change", "saved_ms")
    WORKSPACE_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    LANGUAGE_FIELD_NUMBER: _ClassVar[int]
    NODE_KIND_FIELD_NUMBER: _ClassVar[int]
    NAME_FIELD_NUMBER: _ClassVar[int]
    START_LINE_FIELD_NUMBER: _ClassVar[int]
    END_LINE_FIELD_NUMBER: _ClassVar[int]
    TEXT_FIELD_NUMBER: _ClassVar[int]
    CONTENT_HASH_FIELD_NUMBER: _ClassVar[int]
    CHANGE_FIELD_NUMBER: _ClassVar[int]
    SAVED_MS_FIELD_NUMBER: _ClassVar[int]
    workspace: str
    path: str
    language: str
    node_kind: str
    name: str
    start_line: int
    end_line: int
    text: str
    content_hash: str
    change: str
    saved_ms: int
    def __init__(self, workspace: _Optional[str] = ..., path: _Optional[str] = ..., language: _Optional[str] = ..., node_kind: _Optional[str] = ..., name: _Optional[str] = ..., start_line: _Optional[int] = ..., end_line: _Optional[int] = ..., text: _Optional[str] = ..., content_hash: _Optional[str] = ..., change: _Optional[str] = ..., saved_ms: _Optional[int] = ...) -> None: ...

class IngestSummary(_message.Message):
    __slots__ = ("received", "accepted", "deduplicated", "rejected", "rejections")
    RECEIVED_FIELD_NUMBER: _ClassVar[int]
    ACCEPTED_FIELD_NUMBER: _ClassVar[int]
    DEDUPLICATED_FIELD_NUMBER: _ClassVar[int]
    REJECTED_FIELD_NUMBER: _ClassVar[int]
    REJECTIONS_FIELD_NUMBER: _ClassVar[int]
    received: int
    accepted: int
    deduplicated: int
    rejected: int
    rejections: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, received: _Optional[int] = ..., accepted: _Optional[int] = ..., deduplicated: _Optional[int] = ..., rejected: _Optional[int] = ..., rejections: _Optional[_Iterable[str]] = ...) -> None: ...

class KnobRequest(_message.Message):
    __slots__ = ("session_id", "pod", "reasoning_effort", "laya_dial")
    SESSION_ID_FIELD_NUMBER: _ClassVar[int]
    POD_FIELD_NUMBER: _ClassVar[int]
    REASONING_EFFORT_FIELD_NUMBER: _ClassVar[int]
    LAYA_DIAL_FIELD_NUMBER: _ClassVar[int]
    session_id: str
    pod: str
    reasoning_effort: str
    laya_dial: float
    def __init__(self, session_id: _Optional[str] = ..., pod: _Optional[str] = ..., reasoning_effort: _Optional[str] = ..., laya_dial: _Optional[float] = ...) -> None: ...

class KnobReply(_message.Message):
    __slots__ = ("ok", "applied", "error")
    OK_FIELD_NUMBER: _ClassVar[int]
    APPLIED_FIELD_NUMBER: _ClassVar[int]
    ERROR_FIELD_NUMBER: _ClassVar[int]
    ok: bool
    applied: str
    error: str
    def __init__(self, ok: _Optional[bool] = ..., applied: _Optional[str] = ..., error: _Optional[str] = ...) -> None: ...

class ModeRequest(_message.Message):
    __slots__ = ("session_id", "mode")
    SESSION_ID_FIELD_NUMBER: _ClassVar[int]
    MODE_FIELD_NUMBER: _ClassVar[int]
    session_id: str
    mode: str
    def __init__(self, session_id: _Optional[str] = ..., mode: _Optional[str] = ...) -> None: ...

class ModeReply(_message.Message):
    __slots__ = ("ok", "mode", "error")
    OK_FIELD_NUMBER: _ClassVar[int]
    MODE_FIELD_NUMBER: _ClassVar[int]
    ERROR_FIELD_NUMBER: _ClassVar[int]
    ok: bool
    mode: str
    error: str
    def __init__(self, ok: _Optional[bool] = ..., mode: _Optional[str] = ..., error: _Optional[str] = ...) -> None: ...

class PodControlRequest(_message.Message):
    __slots__ = ("session_id", "pod", "action", "text")
    SESSION_ID_FIELD_NUMBER: _ClassVar[int]
    POD_FIELD_NUMBER: _ClassVar[int]
    ACTION_FIELD_NUMBER: _ClassVar[int]
    TEXT_FIELD_NUMBER: _ClassVar[int]
    session_id: str
    pod: str
    action: PodAction
    text: str
    def __init__(self, session_id: _Optional[str] = ..., pod: _Optional[str] = ..., action: _Optional[_Union[PodAction, str]] = ..., text: _Optional[str] = ...) -> None: ...

class PodControlReply(_message.Message):
    __slots__ = ("ok", "error", "affected", "queued")
    OK_FIELD_NUMBER: _ClassVar[int]
    ERROR_FIELD_NUMBER: _ClassVar[int]
    AFFECTED_FIELD_NUMBER: _ClassVar[int]
    QUEUED_FIELD_NUMBER: _ClassVar[int]
    ok: bool
    error: str
    affected: _containers.RepeatedScalarFieldContainer[str]
    queued: int
    def __init__(self, ok: _Optional[bool] = ..., error: _Optional[str] = ..., affected: _Optional[_Iterable[str]] = ..., queued: _Optional[int] = ...) -> None: ...

class UnloadRequest(_message.Message):
    __slots__ = ("reason", "include_ollama")
    REASON_FIELD_NUMBER: _ClassVar[int]
    INCLUDE_OLLAMA_FIELD_NUMBER: _ClassVar[int]
    reason: str
    include_ollama: bool
    def __init__(self, reason: _Optional[str] = ..., include_ollama: _Optional[bool] = ...) -> None: ...

class UnloadReply(_message.Message):
    __slots__ = ("ok", "unloaded", "loaded_gb_after", "errors")
    OK_FIELD_NUMBER: _ClassVar[int]
    UNLOADED_FIELD_NUMBER: _ClassVar[int]
    LOADED_GB_AFTER_FIELD_NUMBER: _ClassVar[int]
    ERRORS_FIELD_NUMBER: _ClassVar[int]
    ok: bool
    unloaded: _containers.RepeatedScalarFieldContainer[str]
    loaded_gb_after: float
    errors: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, ok: _Optional[bool] = ..., unloaded: _Optional[_Iterable[str]] = ..., loaded_gb_after: _Optional[float] = ..., errors: _Optional[_Iterable[str]] = ...) -> None: ...

class DiffRequest(_message.Message):
    __slots__ = ("id",)
    ID_FIELD_NUMBER: _ClassVar[int]
    id: str
    def __init__(self, id: _Optional[str] = ...) -> None: ...

class DiffArtifact(_message.Message):
    __slots__ = ("ref", "unified")
    REF_FIELD_NUMBER: _ClassVar[int]
    UNIFIED_FIELD_NUMBER: _ClassVar[int]
    ref: DiffRef
    unified: str
    def __init__(self, ref: _Optional[_Union[DiffRef, _Mapping]] = ..., unified: _Optional[str] = ...) -> None: ...

class PingRequest(_message.Message):
    __slots__ = ()
    def __init__(self) -> None: ...

class PingReply(_message.Message):
    __slots__ = ("version", "ts_ms", "daemon")
    VERSION_FIELD_NUMBER: _ClassVar[int]
    TS_MS_FIELD_NUMBER: _ClassVar[int]
    DAEMON_FIELD_NUMBER: _ClassVar[int]
    version: str
    ts_ms: int
    daemon: str
    def __init__(self, version: _Optional[str] = ..., ts_ms: _Optional[int] = ..., daemon: _Optional[str] = ...) -> None: ...

class OsContext(_message.Message):
    __slots__ = ("app_name", "app_bundle_id", "app_pid", "window_title", "selected_text", "screenshot", "screenshot_mime", "screenshot_width", "screenshot_height", "screenshot_cloud_ok", "client_redactions", "omitted", "captured_ms")
    APP_NAME_FIELD_NUMBER: _ClassVar[int]
    APP_BUNDLE_ID_FIELD_NUMBER: _ClassVar[int]
    APP_PID_FIELD_NUMBER: _ClassVar[int]
    WINDOW_TITLE_FIELD_NUMBER: _ClassVar[int]
    SELECTED_TEXT_FIELD_NUMBER: _ClassVar[int]
    SCREENSHOT_FIELD_NUMBER: _ClassVar[int]
    SCREENSHOT_MIME_FIELD_NUMBER: _ClassVar[int]
    SCREENSHOT_WIDTH_FIELD_NUMBER: _ClassVar[int]
    SCREENSHOT_HEIGHT_FIELD_NUMBER: _ClassVar[int]
    SCREENSHOT_CLOUD_OK_FIELD_NUMBER: _ClassVar[int]
    CLIENT_REDACTIONS_FIELD_NUMBER: _ClassVar[int]
    OMITTED_FIELD_NUMBER: _ClassVar[int]
    CAPTURED_MS_FIELD_NUMBER: _ClassVar[int]
    app_name: str
    app_bundle_id: str
    app_pid: int
    window_title: str
    selected_text: str
    screenshot: bytes
    screenshot_mime: str
    screenshot_width: int
    screenshot_height: int
    screenshot_cloud_ok: bool
    client_redactions: int
    omitted: _containers.RepeatedScalarFieldContainer[str]
    captured_ms: int
    def __init__(self, app_name: _Optional[str] = ..., app_bundle_id: _Optional[str] = ..., app_pid: _Optional[int] = ..., window_title: _Optional[str] = ..., selected_text: _Optional[str] = ..., screenshot: _Optional[bytes] = ..., screenshot_mime: _Optional[str] = ..., screenshot_width: _Optional[int] = ..., screenshot_height: _Optional[int] = ..., screenshot_cloud_ok: _Optional[bool] = ..., client_redactions: _Optional[int] = ..., omitted: _Optional[_Iterable[str]] = ..., captured_ms: _Optional[int] = ...) -> None: ...

class TurnRequest(_message.Message):
    __slots__ = ("session_id", "text", "context", "client", "conversation", "cwd")
    SESSION_ID_FIELD_NUMBER: _ClassVar[int]
    TEXT_FIELD_NUMBER: _ClassVar[int]
    CONTEXT_FIELD_NUMBER: _ClassVar[int]
    CLIENT_FIELD_NUMBER: _ClassVar[int]
    CONVERSATION_FIELD_NUMBER: _ClassVar[int]
    CWD_FIELD_NUMBER: _ClassVar[int]
    session_id: str
    text: str
    context: OsContext
    client: str
    conversation: str
    cwd: str
    def __init__(self, session_id: _Optional[str] = ..., text: _Optional[str] = ..., context: _Optional[_Union[OsContext, _Mapping]] = ..., client: _Optional[str] = ..., conversation: _Optional[str] = ..., cwd: _Optional[str] = ...) -> None: ...

class TurnReply(_message.Message):
    __slots__ = ("ok", "error", "session_id", "turn_id", "notices", "route", "conversation")
    OK_FIELD_NUMBER: _ClassVar[int]
    ERROR_FIELD_NUMBER: _ClassVar[int]
    SESSION_ID_FIELD_NUMBER: _ClassVar[int]
    TURN_ID_FIELD_NUMBER: _ClassVar[int]
    NOTICES_FIELD_NUMBER: _ClassVar[int]
    ROUTE_FIELD_NUMBER: _ClassVar[int]
    CONVERSATION_FIELD_NUMBER: _ClassVar[int]
    ok: bool
    error: str
    session_id: str
    turn_id: str
    notices: _containers.RepeatedScalarFieldContainer[str]
    route: str
    conversation: str
    def __init__(self, ok: _Optional[bool] = ..., error: _Optional[str] = ..., session_id: _Optional[str] = ..., turn_id: _Optional[str] = ..., notices: _Optional[_Iterable[str]] = ..., route: _Optional[str] = ..., conversation: _Optional[str] = ...) -> None: ...

class ConversationsRequest(_message.Message):
    __slots__ = ("cwd", "limit")
    CWD_FIELD_NUMBER: _ClassVar[int]
    LIMIT_FIELD_NUMBER: _ClassVar[int]
    cwd: str
    limit: int
    def __init__(self, cwd: _Optional[str] = ..., limit: _Optional[int] = ...) -> None: ...

class Conversation(_message.Message):
    __slots__ = ("handle", "title", "cwd", "origin", "last_used", "turns", "live", "busy", "turn_origin")
    HANDLE_FIELD_NUMBER: _ClassVar[int]
    TITLE_FIELD_NUMBER: _ClassVar[int]
    CWD_FIELD_NUMBER: _ClassVar[int]
    ORIGIN_FIELD_NUMBER: _ClassVar[int]
    LAST_USED_FIELD_NUMBER: _ClassVar[int]
    TURNS_FIELD_NUMBER: _ClassVar[int]
    LIVE_FIELD_NUMBER: _ClassVar[int]
    BUSY_FIELD_NUMBER: _ClassVar[int]
    TURN_ORIGIN_FIELD_NUMBER: _ClassVar[int]
    handle: str
    title: str
    cwd: str
    origin: str
    last_used: float
    turns: int
    live: bool
    busy: bool
    turn_origin: str
    def __init__(self, handle: _Optional[str] = ..., title: _Optional[str] = ..., cwd: _Optional[str] = ..., origin: _Optional[str] = ..., last_used: _Optional[float] = ..., turns: _Optional[int] = ..., live: _Optional[bool] = ..., busy: _Optional[bool] = ..., turn_origin: _Optional[str] = ...) -> None: ...

class ConversationList(_message.Message):
    __slots__ = ("conversations",)
    CONVERSATIONS_FIELD_NUMBER: _ClassVar[int]
    conversations: _containers.RepeatedCompositeFieldContainer[Conversation]
    def __init__(self, conversations: _Optional[_Iterable[_Union[Conversation, _Mapping]]] = ...) -> None: ...

class ConversationRef(_message.Message):
    __slots__ = ("handle",)
    HANDLE_FIELD_NUMBER: _ClassVar[int]
    handle: str
    def __init__(self, handle: _Optional[str] = ...) -> None: ...

class ConversationOpen(_message.Message):
    __slots__ = ("conversation", "cwd", "title", "origin")
    CONVERSATION_FIELD_NUMBER: _ClassVar[int]
    CWD_FIELD_NUMBER: _ClassVar[int]
    TITLE_FIELD_NUMBER: _ClassVar[int]
    ORIGIN_FIELD_NUMBER: _ClassVar[int]
    conversation: str
    cwd: str
    title: str
    origin: str
    def __init__(self, conversation: _Optional[str] = ..., cwd: _Optional[str] = ..., title: _Optional[str] = ..., origin: _Optional[str] = ...) -> None: ...

class ConversationEvent(_message.Message):
    __slots__ = ("handle", "kind", "origin", "text", "delta", "label", "stop", "ts_ms")
    HANDLE_FIELD_NUMBER: _ClassVar[int]
    KIND_FIELD_NUMBER: _ClassVar[int]
    ORIGIN_FIELD_NUMBER: _ClassVar[int]
    TEXT_FIELD_NUMBER: _ClassVar[int]
    DELTA_FIELD_NUMBER: _ClassVar[int]
    LABEL_FIELD_NUMBER: _ClassVar[int]
    STOP_FIELD_NUMBER: _ClassVar[int]
    TS_MS_FIELD_NUMBER: _ClassVar[int]
    handle: str
    kind: str
    origin: str
    text: str
    delta: str
    label: str
    stop: str
    ts_ms: int
    def __init__(self, handle: _Optional[str] = ..., kind: _Optional[str] = ..., origin: _Optional[str] = ..., text: _Optional[str] = ..., delta: _Optional[str] = ..., label: _Optional[str] = ..., stop: _Optional[str] = ..., ts_ms: _Optional[int] = ...) -> None: ...
