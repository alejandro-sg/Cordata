"""Stall detection: an agent going in circles should be told, then stopped, not left to burn the turn.

The failure this catches is not an error; every call "works". It is the same call made again and again with
the same result (a test that keeps failing the same way, a file read for the fifth time, a fix applied,
undone and applied again), or two calls alternating. Nothing in the loop is wrong enough to trip a gate, so
without this the only brake is the context window.

Three escalating responses, all about *this turn*:

1. **A note** (the third identical result): appended to the tool result, in the model's own channel —
   "you have run this three times with the same result; something has to change".
2. **A refusal** (the fifth): the next identical call is not run — ``Deny``, which the model reads as "was not
   run: …". It is a nudge, not a lock: after it the count restarts, so a deliberate retry is possible, and a
   command that is *waiting* for something should wait (``sleep``) rather than hammer.
3. **A person** (the third refusal in a turn, or a second edit cycle): a ``stalled`` event the client shows,
   and the model is told to stop and report. The outcome record carries the stall, and the reward falls.

It also notices an **edit cycle**: a file edited back to a version it had earlier in the turn (A → B → A).

Nothing here widens what an agent may do; it only ever refuses or annotates. Per-session state, bounded.
"""

from __future__ import annotations

import hashlib
import json
import logging
import re
import threading
from collections import deque
from dataclasses import dataclass, field
from typing import Any

import os

from strands.hooks import AfterToolCallEvent, BeforeToolCallEvent, HookProvider, HookRegistry

import harness_events
import outcome_hooks
import outcomes

logger = logging.getLogger(__name__)

NOTE_AT = 3
BLOCK_AT = 5
ESCALATE_AFTER_BLOCKS = 3
ALTERNATING_WINDOW = 8
MAX_KEYS = 200

_VOLATILE = [
    re.compile(r"\b\d+(?:\.\d+)?\s?(?:ms|µs|us|s|sec|seconds|minutes|min)\b", re.IGNORECASE),
    re.compile(r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}\S*"),
    re.compile(r"\b0x[0-9a-fA-F]+\b|\b[0-9a-f]{12,}\b"),
]


def _digest(text: str) -> str:
    for pattern in _VOLATILE:
        text = pattern.sub("~", text)
    return hashlib.sha1(" ".join(text.split())[:6000].encode("utf-8", "replace")).hexdigest()[:12]


def call_key(tool: str, tool_input: Any) -> str:
    if isinstance(tool_input, dict) and tool == "shell":
        command = tool_input.get("command")
        tool_input = {"command": " ".join(command.split()) if isinstance(command, str) else command}
    try:
        body = json.dumps(tool_input, sort_keys=True, default=str)
    except (TypeError, ValueError):
        body = str(tool_input)
    return f"{tool}:{hashlib.sha1(body.encode('utf-8', 'replace')).hexdigest()[:12]}"


@dataclass
class _Agent:
    #: call key → (result digest, how many times in a row that result came back)
    streaks: dict[str, tuple[str, int]] = field(default_factory=dict)
    recent: deque = field(default_factory=lambda: deque(maxlen=ALTERNATING_WINDOW))
    noted: set[str] = field(default_factory=set)
    blocks: int = 0
    notes: int = 0
    #: path → content hashes it has had this turn, in order
    versions: dict[str, list[str | None]] = field(default_factory=dict)
    cycles: int = 0
    alternation_noted: bool = False
    escalated: bool = False


class StallGuard:
    """One per session. State is per agent (a specialist's loop is not the orchestrator's) and per turn."""

    def __init__(self, session_id: str | None = None) -> None:
        self.session_id = session_id
        self._lock = threading.RLock()
        self._turn: str | None = None
        self._agents: dict[str, _Agent] = {}

    # ------------------------------------------------------------------ bookkeeping

    def _agent(self, name: str) -> _Agent:
        recorder = outcomes.active(self.session_id)
        turn = recorder.id if recorder is not None else None
        if turn != self._turn:  # a new turn forgives the last one
            self._turn, self._agents = turn, {}
        return self._agents.setdefault(name or "agent", _Agent())

    def _stall(self, agent: str, kind: str, what: str) -> None:
        recorder = outcomes.active(self.session_id)
        if recorder is not None:
            recorder.stall(kind)
        payload = {"event": "heal", "phase": "stalled", "tool": agent, "root_cause": what}
        if self.session_id:
            payload["sessionId"] = self.session_id
        harness_events.emit("heal", payload)

    # ------------------------------------------------------------------ before a call

    def before(self, agent_name: str, tool: str, tool_input: Any) -> str | None:
        """A refusal for a call that would be the fifth identical one, else None."""
        with self._lock:
            agent = self._agent(agent_name)
            key = call_key(tool, tool_input)
            _digest_seen, count = agent.streaks.get(key, ("", 0))
            if count < BLOCK_AT:
                return None
            agent.blocks += 1
            agent.streaks[key] = (_digest_seen, NOTE_AT - 1)  # a nudge, not a lock: a deliberate retry may follow
            what = f"{tool} called {count} times with the same input and the same result"
            escalate = agent.blocks >= ESCALATE_AFTER_BLOCKS
            self._stall(agent_name, "block", what)
            if escalate and not agent.escalated:
                agent.escalated = True
                return (
                    f"{tool} was not run: this is the {agent.blocks}th time this turn you have repeated a step that was "
                    "not working. Stop. Tell the user what you tried, what you saw each time, and what you think is needed."
                )
            return (
                f"{tool} was not run: it has been called {count} times in a row with the same input and has returned "
                "the same result each time. Repeating it will not change anything. Change something first (the code, the "
                "input, the approach), read the output again for what it is actually saying, or — if you are waiting for "
                "something to finish — wait between checks instead of polling."
            )

    # ------------------------------------------------------------------ after a call

    def after(self, agent_name: str, tool: str, tool_input: Any, result_text: str) -> str | None:
        """A note to append to the result when the loop is forming, else None."""
        with self._lock:
            agent = self._agent(agent_name)
            key = call_key(tool, tool_input)
            digest = _digest(result_text)
            previous, count = agent.streaks.get(key, ("", 0))
            count = count + 1 if previous == digest else 1
            agent.streaks[key] = (digest, count)
            if len(agent.streaks) > MAX_KEYS:
                agent.streaks.pop(next(iter(agent.streaks)))
            agent.recent.append(key)
            note: str | None = None
            if count >= NOTE_AT and key not in agent.noted:
                agent.noted.add(key)
                agent.notes += 1
                what = f"{tool} returned the same result {count} times in a row"
                self._stall(agent_name, "note", what)
                note = (
                    f"[loop] You have now run this exact {tool} call {count} times and it has returned the same result each time. "
                    "Something has to change before you try it again: read the output for what it is actually saying, "
                    "look at a different file or the code that produces it, or ask the user."
                )
            elif count < NOTE_AT:
                agent.noted.discard(key)
            if note is None and not agent.alternation_noted and len(agent.recent) == ALTERNATING_WINDOW:
                distinct = set(agent.recent)
                window = list(agent.recent)
                strictly_alternating = all(window[i] != window[i + 1] for i in range(len(window) - 1))
                if len(distinct) == 2 and strictly_alternating and all(agent.streaks.get(k, ("", 0))[1] >= 2 for k in distinct):
                    agent.alternation_noted = True
                    agent.notes += 1
                    self._stall(agent_name, "note", f"{tool} alternating between two calls with no change in result")
                    note = (
                        "[loop] The last eight calls alternated between the same two steps and neither changed the outcome. "
                        "Step back: what is the failing check telling you, and what have you not tried?"
                    )
            return note

    def edited(self, agent_name: str, path: str, before: str | None, after: str | None) -> str | None:
        """A note when an edit puts a file back to a version it had earlier in this turn (``before`` and
        ``after`` are content hashes; None for a file that does not exist)."""
        with self._lock:
            agent = self._agent(agent_name)
            history = agent.versions.setdefault(path, [before])
            returned = after in history and history[-1] != after
            history.append(after)
            del history[:-12]
            if not returned:
                return None
            agent.cycles += 1
            self._stall(agent_name, "cycle", f"{path} was edited back to a version it had earlier in the turn")
            return (
                f"[loop] This edit returns {path} to a version it already had earlier in this turn — you are going in circles "
                "between the same states. Re-read the failing check, and choose a different change."
            )


class StallHooks(HookProvider):
    """The after-the-call half: annotate a result when the loop is forming, and watch edits for cycles. (The
    refusal half runs in the approval handler, where every other reason to refuse a call already lives.)"""

    def __init__(self, session_id: str | None, agent_name: str | None = None) -> None:
        self.session_id = session_id
        self.agent_name = agent_name
        self._before: dict[str, dict[str, str | None]] = {}

    def register_hooks(self, registry: HookRegistry) -> None:
        registry.add_callback(BeforeToolCallEvent, self._on_before)
        registry.add_callback(AfterToolCallEvent, self._on_after)

    def _name(self, agent: Any) -> str:
        return self.agent_name or str(getattr(agent, "name", "") or "agent")

    def _full(self, root: str, path: str) -> str:
        return path if os.path.isabs(path) else os.path.join(root, path)

    def _on_before(self, event: BeforeToolCallEvent) -> None:
        try:
            name = str(event.tool_use.get("name") or "")
            if name not in outcomes.EDIT_TOOLS:
                return
            recorder = outcomes.active(self.session_id)
            root = recorder.project if recorder is not None else os.getcwd()
            use_id = str(event.tool_use.get("toolUseId") or "")
            paths = outcome_hooks.edit_paths(name, event.tool_use.get("input"))[:8]
            self._before[use_id] = {p: outcomes.file_hash(self._full(root, p)) for p in paths}
            while len(self._before) > 64:
                self._before.pop(next(iter(self._before)))
        except Exception:
            logger.debug("stall: before-edit hash failed", exc_info=True)

    def _on_after(self, event: AfterToolCallEvent) -> None:
        try:
            name = str(event.tool_use.get("name") or "")
            tool_input = event.tool_use.get("input")
            guard = for_session(self.session_id)
            agent = self._name(event.agent)
            result = event.result if isinstance(event.result, dict) else {}
            text = "\n".join(c.get("text", "") for c in result.get("content") or [] if isinstance(c, dict))
            if outcome_hooks.was_denied(result):
                return  # a refused call is not progress or a loop; it never ran
            notes = [guard.after(agent, name, tool_input, text)]
            use_id = str(event.tool_use.get("toolUseId") or "")
            before = self._before.pop(use_id, None)
            if before and result.get("status") != "error":
                recorder = outcomes.active(self.session_id)
                root = recorder.project if recorder is not None else os.getcwd()
                for path, old in before.items():
                    notes.append(guard.edited(agent, path, old, outcomes.file_hash(self._full(root, path))))
            extra = [n for n in notes if n]
            if extra:
                content = list(result.get("content") or [])
                content.extend({"text": n} for n in extra)
                event.result = {**result, "content": content}
        except Exception:
            logger.debug("stall: after-call check failed", exc_info=True)


_GUARDS: dict[str, StallGuard] = {}
_GUARDS_LOCK = threading.Lock()


def for_session(session_id: str | None) -> StallGuard:
    key = session_id or ""
    with _GUARDS_LOCK:
        if key not in _GUARDS:
            if len(_GUARDS) > 64:
                _GUARDS.pop(next(iter(_GUARDS)))
            _GUARDS[key] = StallGuard(session_id)
        return _GUARDS[key]
