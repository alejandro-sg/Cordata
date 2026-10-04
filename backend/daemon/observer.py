"""The proactive loop: look at the projects while nobody is around, and leave a
briefing behind.

Off by default (`observer.enabled`). When on, the daemon wakes every
`observer.intervalMinutes` and does one *cycle*: collect evidence, ask the
observer agent (a child process — `observer_runner.py`) what it makes of it, and
write `nightly_briefing.md`.

**Zero-write is structural.** The agent this drives holds `file_read` and
`think` and nothing else (`OBSERVER_TOOLS`); it cannot run a command, fetch
anything, or write a file, and the allowlist is asserted disjoint from
`sub_agent_common`'s denylists at import. The briefing is written *here*, by
daemon code, from text the child prints — a model cannot reach that file except
by producing text a human will read. The evidence it is shown is collected with
a fixed `git` argv list, so the model never chooses a command either.

**It never runs alongside a turn.** `wake()` is called by the daemon whenever a
client attaches, and `busy()` is checked before a cycle starts and again before
anything is written; if either fires mid-cycle the child is killed and the cycle
is abandoned. Preemption is therefore not "the agent was asked to stop" — it is
a process-group signal to a process that has no way to resist.

**Its output is a reading, not a change.** Proposals are staged in the ledger as
`interventions` rows with `kind='observation'` and status `proposed` — never
`staged`, which the ledger reserves for an approval someone is actually waiting
on (`state_store.pending_interventions`, and the `--resume` notice built on it).
Turning one into a real change is a separate, human decision that goes through
`proposer_tool.py`'s own static/test/critic gates, entered by the orchestrator —
the observer has no path to those either.

Residual, stated plainly: the observer's agent runs on the same provider as
everything else, so its prompts (evidence from your working tree) go wherever
your model provider is. And a project directory is *observed*, not isolated —
the evidence is git output and log tails, which is exactly as sensitive as those
are.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import re
import subprocess
import sys
import threading
import time
from collections import deque
from pathlib import Path
from typing import Any, Callable, Iterable

logger = logging.getLogger("sierralab.observer")

BACKEND_DIR = Path(__file__).resolve().parent.parent

#: The ledger's session id for anything the observer writes. Not a real session:
#: it exists because `interventions.session_id` is NOT NULL and an observation
#: has to live somewhere that a query can single out.
OBSERVER_SESSION = "observer"

#: How many queued webhook observations to keep. A source that keeps pushing
#: must not be able to grow the daemon's memory, and one cycle will not read
#: more than a handful anyway.
INBOX_LIMIT = 50

#: Per-command and per-source caps. The point of the observer is a short
#: briefing; a 40k-line diff would cost more than it tells anyone.
MAX_EVIDENCE_CHARS = 6000
MAX_LOG_CHARS = 2000
LOG_TAIL_LINES = 40

COMMAND_TIMEOUT = 15.0

#: The longest one observation may take. A model call that never returns would
#: otherwise park the only unattended loop here forever — a client attaching is
#: the sole other way out, and the point of the loop is that nobody is there.
CHILD_TIMEOUT = 600.0

#: The commands the observer is allowed to gather evidence with. Fixed argv,
#: fixed order, no shell — this is the "allowlisted read-only command set", read
#: as strictly as it can be read: the *daemon* runs them and the model is handed
#: the output. `git diff HEAD` is included but truncated; see MAX_EVIDENCE_CHARS.
#:
#: Reading is not the same as harmless: git runs programs named in the
#: repository's own config. `core.fsmonitor` is executed by `status` and `diff`,
#: and `diff.external` / `diff.<driver>.textconv` by `diff`, so a checkout whose
#: `.git/config` was written by something else would run its command from a
#: daemon nobody is watching. Those are switched off per invocation.
GIT_COMMANDS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("status", ("git", "-c", "core.fsmonitor=false", "status", "--short", "--branch")),
    ("diff-stat", ("git", "-c", "core.fsmonitor=false", "diff", "--no-ext-diff", "--no-textconv", "--stat", "HEAD")),
    ("log", ("git", "log", "--oneline", "-n", "20")),
    ("diff", ("git", "-c", "core.fsmonitor=false", "diff", "--no-ext-diff", "--no-textconv", "HEAD")),
)

#: `GIT_OPTIONAL_LOCKS=0` is the load-bearing one: a plain `git status` refreshes
#: and *writes* the index as a side effect, which would make the observer's
#: "writes nothing" a claim about files the user chose rather than a property the
#: process has. The rest keep it non-interactive and unpaged.
GIT_ENV = {
    "GIT_OPTIONAL_LOCKS": "0",
    "GIT_PAGER": "cat",
    "GIT_TERMINAL_PROMPT": "0",
    "LC_ALL": "C",
}

_NUDGE_SECONDS = 30.0


def _truncate(text: str, limit: int = MAX_EVIDENCE_CHARS) -> str:
    text = text.rstrip()
    if len(text) <= limit:
        return text
    return text[:limit] + f"\n… [{len(text) - limit} more characters]"


def _fence(body: str, info: str = "text") -> list[str]:
    """A fenced block whose fence is longer than any backtick run inside it.

    The evidence is a `git diff`, and a diff of a markdown file is full of
    triple backticks: with a fixed fence the first one closes the block early and
    the rest of the diff — plus whatever follows it — renders as the briefing's
    own prose, which is exactly the line this file draws between evidence and
    reading."""
    longest = max((len(run) for run in re.findall(r"`+", body)), default=0)
    ticks = "`" * max(3, longest + 1)
    return [f"{ticks}{info}", body, ticks]


def user_briefing_path() -> Path:
    """Where the briefing lands. `SIERRALAB_OBSERVER_BRIEFING` overrides it, which
    is what the smoke test uses — the default is the user's private harness
    directory, beside `state.db`."""
    override = os.environ.get("SIERRALAB_OBSERVER_BRIEFING")
    if override:
        return Path(override).expanduser()
    here = os.environ.get("SIERRALAB_HOME")
    base = Path(here).expanduser() if here else Path.home() / ".sierralab"
    return base / "nightly_briefing.md"


def runner_command() -> list[str]:
    override = os.environ.get("SIERRALAB_OBSERVER_RUNNER")
    if override:
        return override.split()
    return [sys.executable, str(BACKEND_DIR / "daemon" / "observer_runner.py")]


def parse_quiet(text: str | None) -> tuple[int, int] | None:
    """`"22:00-07:00"` as (start, end) minutes-of-day, or None.

    Wrapping is allowed and is the common case: a quiet period that ends before
    it starts is the overnight one, and refusing to parse it would silently drop
    the setting. Anything unparseable is None — "no quiet hours" — reported in the
    log, so a typo does not look like a working schedule.
    """
    if not text or "-" not in text:
        return None
    start_text, _, end_text = text.partition("-")

    def minutes(value: str) -> int | None:
        parts = value.strip().split(":")
        if len(parts) != 2 or not all(part.isdigit() for part in parts):
            return None
        hour, minute = int(parts[0]), int(parts[1])
        if not (0 <= hour < 24 and 0 <= minute < 60):
            return None
        return hour * 60 + minute

    start, end = minutes(start_text), minutes(end_text)
    if start is None or end is None:
        logger.warning("observer: cannot read SIERRALAB_OBSERVER_QUIET=%r; running all hours", text)
        return None
    return start, end


def in_quiet_hours(now: float, window: tuple[int, int] | None) -> bool:
    if window is None:
        return False
    start, end = window
    local = time.localtime(now)
    minutes = local.tm_hour * 60 + local.tm_min
    if start <= end:
        return start <= minutes < end
    return minutes >= start or minutes < end


def known_projects(index_path: Path | None = None, *, limit: int = 5) -> list[str]:
    """The project directories the harness knows about, newest session first.

    `sessions/index.json` is what the ACP server writes for `session/list` — it
    is the only record of "projects this person actually works in", which is a
    better source than scanning the filesystem for git repositories.
    """
    path = index_path or (BACKEND_DIR / "sessions" / "index.json")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    if not isinstance(data, dict):
        return []
    rows = [row for row in data.values() if isinstance(row, dict)]
    rows.sort(key=lambda row: row.get("updatedAt") or "", reverse=True)
    seen: list[str] = []
    for row in rows:
        cwd = row.get("cwd")
        if not isinstance(cwd, str) or not cwd or cwd in seen:
            continue
        if not Path(cwd).is_dir():
            continue
        seen.append(cwd)
        if len(seen) >= limit:
            break
    return seen


def git_evidence(project: str, *, timeout: float = COMMAND_TIMEOUT) -> dict[str, str]:
    """Read-only git output for one project.

    Every command is run with `cwd=project` and a fixed argv; nothing about the
    invocation depends on anything a model produced. A directory that is not a
    repository (or a git that fails for any reason) is recorded as such rather
    than raised — a project without git is still worth observing.
    """
    env = {**os.environ, **GIT_ENV}
    out: dict[str, str] = {}
    for label, argv in GIT_COMMANDS:
        try:
            done = subprocess.run(
                list(argv),
                cwd=project,
                capture_output=True,
                timeout=timeout,
                env=env,
                stdin=subprocess.DEVNULL,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            out[label] = f"[{type(error).__name__}: {error}]"
            continue
        text = done.stdout.decode("utf-8", "replace")
        if done.returncode != 0:
            detail = done.stderr.decode("utf-8", "replace").strip().splitlines()
            text += f"\n[exit {done.returncode}: {detail[-1] if detail else 'no output'}]"
        out[label] = _truncate(text)
    return out


def log_evidence(paths: Iterable[Path]) -> dict[str, str]:
    """The tail of each configured log file. `SIERRALAB_OBSERVER_LOGS` names them,
    separator-separated (it is an env var rather than a settings key on purpose:
    a path list in a settings file the agent can read is a path list an injected
    agent can point somewhere else)."""
    out: dict[str, str] = {}
    for path in paths:
        try:
            text = Path(path).expanduser().read_text(encoding="utf-8", errors="replace")
        except OSError as error:
            out[str(path)] = f"[unreadable: {error.strerror or error}]"
            continue
        tail = "\n".join(text.splitlines()[-LOG_TAIL_LINES:])
        out[str(path)] = _truncate(tail, MAX_LOG_CHARS)
    return out


class Observer:
    """One per daemon. Owns the schedule, the child processes, and the inbox."""

    def __init__(
        self,
        *,
        projects: Callable[[], list[str]] = known_projects,
        busy: Callable[[], bool] = lambda: False,
        settings: Callable[[], Any] | None = None,
        briefing_path: Path | None = None,
        runner: Callable[[], list[str]] = runner_command,
        logs: Callable[[], list[Path]] | None = None,
        tier: str | None = None,
    ) -> None:
        self._projects = projects
        self._busy = busy
        #: `observer.*` is daemon-wide, so it is read once from the user scope —
        #: settings_store.current(HOME) merges the user file and, normally, no
        #: project file at all.
        self._settings = settings or self._default_settings
        self._briefing = briefing_path or user_briefing_path()
        self._runner = runner
        self._logs = logs or self._default_logs
        self._tier = tier
        self._preempt = threading.Event()
        self._stop = threading.Event()
        self._nudge = threading.Event()
        self._inbox: deque[dict[str, Any]] = deque(maxlen=INBOX_LIMIT)
        self._lock = threading.Lock()
        self._task: asyncio.Task | None = None
        self._cycles = 0
        self._last: dict[str, Any] = {}

    # ------------------------------------------------------------- wiring

    @staticmethod
    def _default_settings() -> Any:
        import settings_store  # noqa: PLC0415 — same lazy-import rule as server.py

        return settings_store.current(str(Path.home()))

    @staticmethod
    def _default_logs() -> list[Path]:
        raw = os.environ.get("SIERRALAB_OBSERVER_LOGS") or ""
        return [Path(item) for item in raw.split(os.pathsep) if item.strip()]

    def enabled(self) -> bool:
        return self._settings().get("observer.enabled", False) is True

    def interval_minutes(self) -> float:
        try:
            value = float(self._settings().get("observer.intervalMinutes", 30.0))
        except (TypeError, ValueError):
            return 30.0
        return min(max(value, 1.0), 1440.0)

    def quiet(self):
        return parse_quiet(os.environ.get("SIERRALAB_OBSERVER_QUIET"))

    # ------------------------------------------------------------- sources

    def observe(self, text: str, *, project: str | None = None, source: str = "webhook") -> int:
        """Queue an external observation (the daemon's webhook route calls this).

        Queued rather than acted on: a webhook is a *source*, and the thing that
        decides what any of this means is the cycle, which will not run while
        somebody is working. The return value is how many are waiting, so the
        caller can tell "taken" from "dropped".
        """
        with self._lock:
            self._inbox.append({"text": str(text)[:4000], "project": project, "source": source, "ts": time.time()})
            depth = len(self._inbox)
        self._nudge.set()
        return depth

    def _take_inbox(self) -> list[dict[str, Any]]:
        with self._lock:
            items = list(self._inbox)
            self._inbox.clear()
        return items

    def _requeue(self, items: list[dict[str, Any]]) -> None:
        """Put taken observations back, ahead of anything that arrived since.

        A cycle that is abandoned never wrote them anywhere. Without this a
        webhook that said "deploy failed" is lost to whoever happened to attach a
        client while the model was thinking — and `/observe` has already told the
        sender it was queued."""
        if not items:
            return
        with self._lock:
            newer = list(self._inbox)
            self._inbox.clear()
            self._inbox.extend([*items, *newer])

    # ------------------------------------------------------------- control

    def wake(self, reason: str = "") -> None:
        """Somebody is here — abandon any cycle in flight.

        Called on client attach and on a prompt. Setting the event is the whole
        protocol: the cycle loop checks it before each source, before the child
        runs, and before it writes anything, and the child is killed rather than
        asked to stop.
        """
        if not self._preempt.is_set() and reason:
            logger.info("observer: preempted (%s)", reason)
        self._preempt.set()

    def stop(self) -> None:
        self._stop.set()
        self.wake("daemon shutting down")

    def status(self) -> dict[str, Any]:
        with self._lock:
            queued = len(self._inbox)
        return {
            "enabled": self.enabled(),
            "running": self._task is not None and not self._task.done(),
            "cycles": self._cycles,
            "queued": queued,
            "intervalMinutes": self.interval_minutes(),
            "briefing": str(self._briefing),
            **self._last,
        }

    # ------------------------------------------------------------- the loop

    def start(self) -> asyncio.Task:
        """Start the background loop. Returns the task so the daemon can await it
        on shutdown; a no-op task if the observer is switched off."""
        self._task = asyncio.create_task(self.run(), name="observer")
        return self._task

    async def run(self) -> None:
        if not self.enabled():
            logger.info("observer: off (observer.enabled is false)")
            return
        logger.info("observer: on every %s min, briefing at %s", self.interval_minutes(), self._briefing)
        while not self._stop.is_set():
            await self._sleep_until_due()
            if self._stop.is_set():
                break
            try:
                await self.cycle()
            except asyncio.CancelledError:
                raise
            except Exception:
                # One bad cycle must not end the loop: the observer is the only
                # thing here that runs unattended, so a crash would be silent.
                logger.exception("observer: cycle failed")

    async def _sleep_until_due(self) -> None:
        loop = asyncio.get_running_loop()
        deadline = loop.time() + self.interval_minutes() * 60.0
        while not self._stop.is_set():
            if self._nudge.is_set():
                self._nudge.clear()
                return
            remaining = deadline - loop.time()
            if remaining <= 0:
                return
            await asyncio.sleep(min(remaining, _NUDGE_SECONDS))

    # ------------------------------------------------------------- a cycle

    async def cycle(self) -> dict[str, Any] | None:
        """One pass. None when nothing was done, and the reason is logged."""
        self._preempt.clear()
        if not self.enabled():
            return None
        window = self.quiet()
        if in_quiet_hours(time.time(), window):
            logger.debug("observer: quiet hours")
            return None
        if self._busy():
            logger.debug("observer: a client is attached; not observing")
            return None

        projects = self._projects()
        inbox = self._take_inbox()
        if not projects and not inbox:
            self._last = {"lastCycle": time.time(), "observed": 0, "note": "no known projects"}
            return None

        observations: list[dict[str, Any]] = []
        # Read the logs once per cycle, not once per project: they are the
        # configured files, not anything project-specific.
        logs = await asyncio.to_thread(log_evidence, self._logs()) if self._logs() else {}
        for project in projects:
            if self._preempt.is_set():
                return self._abandon("preempted during evidence collection", inbox)
            evidence = {
                "collectedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "project": project,
                "git": await asyncio.to_thread(git_evidence, project),
                "logs": logs,
            }
            for item in inbox:
                if item.get("project") in (None, project):
                    evidence.setdefault("noted", []).append({"source": item["source"], "text": item["text"]})
            if self._preempt.is_set():
                return self._abandon("preempted before the observer agent ran", inbox)
            answer = await self._run_child(project, evidence)
            if answer is None:
                return self._abandon("preempted while the observer agent was running", inbox)
            answer["evidence"] = evidence
            observations.append(answer)

        # A webhook naming a project nobody has a session for still gets read.
        orphan = [item for item in inbox if item.get("project") and item["project"] not in projects]
        if orphan:
            observations.append(
                {
                    "project": orphan[0]["project"],
                    "summary": "; ".join(item["text"] for item in orphan)[:2000],
                    "proposals": [],
                    "evidence": {"noted": orphan},
                }
            )

        if self._preempt.is_set() or self._busy():
            # The last look before anything is written: a user who attached while
            # the model was thinking must not get a briefing written over their
            # session's project.
            return self._abandon("preempted before writing the briefing", inbox)

        text = render_briefing(observations, generated_at=time.time())
        await asyncio.to_thread(self._write_briefing, text)
        staged = await asyncio.to_thread(self._stage, observations)
        self._cycles += 1
        self._last = {
            "lastCycle": time.time(),
            "observed": len(observations),
            "proposals": staged,
            "briefing": str(self._briefing),
        }
        logger.info("observer: cycle %s — %s project(s), %s proposal(s)", self._cycles, len(observations), staged)
        return self._last

    def _abandon(self, why: str, inbox: list[dict[str, Any]] | None = None) -> None:
        logger.info("observer: %s", why)
        self._requeue(inbox or [])
        self._last = {"lastCycle": time.time(), "observed": 0, "note": why}

    async def _run_child(self, project: str, evidence: dict[str, Any]) -> dict[str, Any] | None:
        """One observation, in a process of its own. None if it was preempted.

        The child is spawned in the project directory — that is what makes
        `file_read`'s relative paths mean anything — and killed as a group on
        preemption, so a model in the middle of a tool call cannot finish it.
        """
        request = {"project": project, "evidence": evidence}
        if self._tier:
            request["tier"] = self._tier
        payload = (json.dumps(request) + "\n").encode("utf-8")
        try:
            process = await asyncio.create_subprocess_exec(
                *self._runner(),
                cwd=project,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=None,  # the child's logs go to ours
                start_new_session=True,
                env={**os.environ, "PYTHONUNBUFFERED": "1"},
            )
        except OSError as error:
            logger.warning("observer: could not start the runner in %s: %s", project, error)
            return None

        talking = asyncio.ensure_future(process.communicate(payload))
        watching = asyncio.ensure_future(self._wait_preempt())
        try:
            done, _ = await asyncio.wait({talking, watching}, timeout=CHILD_TIMEOUT, return_when=asyncio.FIRST_COMPLETED)
            if talking not in done:
                talking.cancel()
                await self._kill(process)
                if watching not in done:
                    logger.warning("observer: the runner in %s took longer than %.0fs; killed", project, CHILD_TIMEOUT)
                return None
            stdout, _ = talking.result()
        except asyncio.CancelledError:
            # Shutdown, not preemption: the cycle task was cancelled while the
            # model was thinking. The child has to be killed here, or it outlives
            # the daemon with a model call in flight.
            talking.cancel()
            await self._kill(process)
            raise
        finally:
            watching.cancel()

        line = stdout.decode("utf-8", "replace").strip().splitlines()
        if not line:
            logger.warning("observer: runner in %s said nothing (rc=%s)", project, process.returncode)
            return None
        try:
            answer = json.loads(line[-1])
        except ValueError:
            logger.warning("observer: runner in %s printed no JSON", project)
            return None
        if answer.get("error"):
            logger.warning("observer: %s: %s", project, answer["error"])
            return None
        return answer

    async def _wait_preempt(self) -> None:
        """Completes when `wake()` fires.

        Polled rather than handed to a thread: `threading.Event.wait` in
        `asyncio.to_thread` cannot be cancelled, so each cycle would leave a
        thread parked on an event that is *cleared* at the start of the next one
        — a leak that grows one thread per cycle, forever.
        """
        while not self._preempt.is_set():
            await asyncio.sleep(0.2)

    @staticmethod
    async def _kill(process) -> None:
        """The supervisor's own termination, reused: SIGTERM the *group* (a model
        call may have children), then SIGKILL. Same reasoning as
        `Supervisor._terminate` — a killed process whose children survive is
        still running."""
        from .supervisor import Supervisor  # noqa: PLC0415

        await Supervisor._terminate(process, grace=3.0)

    # ------------------------------------------------------------- output

    def _write_briefing(self, text: str) -> None:
        try:
            self._briefing.parent.mkdir(parents=True, exist_ok=True)
            tmp = self._briefing.with_suffix(".tmp")
            # 0600 from creation: the briefing quotes a full `git diff` of the
            # user's working tree, which is not for other local accounts.
            fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            os.fchmod(fd, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(text)
            os.replace(tmp, self._briefing)
        except OSError as error:
            logger.warning("observer: could not write %s: %s", self._briefing, error)

    def _stage(self, observations: list[dict[str, Any]]) -> int:
        """Stage each proposal in the ledger, or write nothing if the ledger is
        unwritable — a briefing that was written is still worth having."""
        try:
            import state_store  # noqa: PLC0415
        except ImportError:  # pragma: no cover — the daemon always runs beside it
            return 0
        staged = 0
        for observation in observations:
            for proposal in observation.get("proposals") or []:
                row = {
                    "project": observation.get("project"),
                    "title": proposal.get("title"),
                    "why": proposal.get("why"),
                    "next": proposal.get("next"),
                }
                ident = state_store.stage_intervention(
                    session_id=OBSERVER_SESSION,
                    kind="observation",
                    tool=None,
                    tool_input=row,
                    note=str(proposal.get("title") or "")[:200],
                )
                # `proposed`, not `staged`: `pending_interventions` and the
                # `--resume` notice built on it mean "an approval someone is
                # waiting on", and nothing is waiting on an observation.
                state_store.resolve_intervention(ident, "proposed", decided_by="observer", note=row["why"])
                state_store.append_event("observation", {**row, "intervention_id": ident}, session_id=OBSERVER_SESSION, source="observer")
                staged += 1
        return staged


def render_briefing(observations: list[dict[str, Any]], *, generated_at: float) -> str:
    """The file, as markdown.

    Written by this function and nothing else. The header says who wrote it and
    what it could not have done, because the reader's first question about a file
    that appeared on its own is exactly that.
    """
    when = time.strftime("%Y-%m-%d %H:%M", time.localtime(generated_at))
    parts = [
        "# Nightly briefing",
        "",
        f"_{when} — written by the observer daemon while nothing was attached. "
        "The observer holds read-only tools (`file_read`, `think`): it ran no commands, "
        "fetched nothing, and changed nothing. The evidence below is what this daemon "
        "collected with a fixed `git` command list; anything else is the model's reading of it._",
        "",
    ]
    if not observations:
        parts += ["Nothing to report this cycle.", ""]
        return "\n".join(parts)

    for observation in observations:
        project = observation.get("project") or "(unknown project)"
        git = (observation.get("evidence") or {}).get("git") or {}
        changed = [line for line in (git.get("status") or "").splitlines() if line.strip()]
        parts += [f"## {project}", ""]
        if git.get("diff-stat"):
            parts += [*_fence(git["diff-stat"].strip(), ""), ""]
        parts += [str(observation.get("summary") or "").strip(), ""]
        proposals = observation.get("proposals") or []
        if proposals:
            parts.append("### Proposed")
            parts.append("")
            for proposal in proposals:
                line = f"- **{proposal.get('title')}**"
                if proposal.get("why"):
                    line += f" — {proposal['why']}"
                if proposal.get("next"):
                    line += f" _(next: {proposal['next']})_"
                parts.append(line)
            parts.append("")
        evidence_block = "\n".join(
            [
                f"working tree ({len(changed)} line(s) of status):",
                (git.get("status") or "(none)").strip(),
                "",
                "recent commits:",
                (git.get("log") or "(none)").strip(),
                "",
                "full diff (truncated):",
                (git.get("diff") or "(none)").strip(),
            ]
        )
        parts += ["<details><summary>Evidence</summary>", "", *_fence(evidence_block), ""]
        noted = (observation.get("evidence") or {}).get("noted") or []
        if noted:
            # Externally supplied text, so it goes in the evidence block — raw and
            # attributed, never promoted into the model's prose. A webhook that
            # said "deploy failed" must show up even if the model did not repeat
            # it, or the operator would never learn it arrived.
            queued = "\n".join(
                [
                    "queued observations (from outside this daemon; not written by a model):",
                    *(_truncate(f"[{item.get('source')}] {item.get('text')}", MAX_EVIDENCE_CHARS) for item in noted),
                ]
            )
            parts += [*_fence(queued), ""]
        logs = (observation.get("evidence") or {}).get("logs") or {}
        if logs:
            parts += [*_fence("\n".join(f"{path}:\n{text}" for path, text in logs.items())), ""]
        parts += ["</details>", ""]
    return "\n".join(parts)
