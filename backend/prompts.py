"""Prompt overlays: a paragraph the harness wrote for itself, appended to a session's system prompt.

A learned skill (``learned.py``) is read *on demand* — the model sees one line and opens the body only when it
applies. A **prompt overlay** is the other half of the same idea: prose that is in front of the model on **every**
turn of a session in the project it was learned in. That makes it the more powerful lever and the more dangerous
one, so it is bounded the same way a skill is, and a little tighter:

    staging/   drafted by ``optimize.py``. Never read by a session.
    live/      passed the promotion gate (``promote.py``) under ``learning.promotion: live``, or a person said yes.
    retired/   was live and stopped helping, or was withdrawn. Kept, not read.
    rejected/  failed the filter or the gate. Kept so the same draft is not proposed again.

Layout (``~/.cordata/prompts/<project key>/<status>/<name>.md``). The file is the overlay — no frontmatter; the
name is the file name. Appended as its own block at the end of the system prompt, after the project prompt, the
workspace context and the person's instructions, wrapped so the boundary is unambiguous.

**The same structural bound as a skill.** An overlay is text a model reads and may follow, and it may have been
drafted from untrusted text. ``validate_draft`` applies ``textfilter`` (the exact rules skills get) and a size
rule — but a determined sentence can be phrased past any pattern list. What bounds the damage is that an overlay
can only *advise*: it is appended to the prompt string and nothing else. It cannot approve a call, lower a Guard
score, change a mode or touch the vault, because every one of those decisions is made in code that never reads a
prompt overlay; it is read only in the project it was learned in; it is not read at all until it has beaten the
baseline on the eval suite (``promote.evaluate`` with ``kind="prompt"``); and it is retired when the turns that
had it in front of them score worse than the turns that did not.

Stdlib plus ``state_store``, ``secret_patterns`` and ``textfilter``; no model, no Strands.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import re
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import state_store
import textfilter

logger = logging.getLogger(__name__)

STATUSES = ("staging", "live", "retired", "rejected")
GLOBAL = "_global"
NAME_RE = re.compile(r"^[a-z][a-z0-9-]{2,47}$")
MIN_BODY, MAX_BODY, MAX_BODY_LINES = 40, 3000, 60
MAX_DRAFTS_PER_DAY = 2
MIN_USES_TO_JUDGE = 3
REWARD_WINDOW = 12
_LOCK = threading.RLock()

#: The environment a promotion arm sets to put a candidate overlay in front of a session without staging it live.
OVERLAY_ENV = "CORDATA_PROMPT_OVERLAY"
#: Named so a person reading the prompt can tell harness-written text from their own instructions.
HEADING = "How this project is worked on"


# ------------------------------------------------------------------------------ paths


def home() -> Path:
    return Path(os.environ.get("CORDATA_HOME") or Path.home() / ".cordata") / "prompts"


def project_key(project: str | os.PathLike | None) -> str:
    """Same key as ``learned`` — the two stores are read side by side and must sort the same folder."""
    if project is None:
        return GLOBAL
    try:
        real = os.path.realpath(str(project))
    except OSError:
        real = str(project)
    return hashlib.sha1(real.encode()).hexdigest()[:16]


def scope_dir(project: str | os.PathLike | None) -> Path:
    return home() / project_key(project)


def _path(project: str | os.PathLike | None, status: str, name: str) -> Path:
    return scope_dir(project) / status / f"{name}.md"


def _private(path: Path) -> None:
    try:
        path.mkdir(parents=True, exist_ok=True)
        os.chmod(path, 0o700)
    except OSError:
        pass


# ------------------------------------------------------------------------------ drafts


@dataclass
class Draft:
    name: str = ""
    body: str = ""
    text: str = ""
    problems: list[str] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not self.problems


def validate_draft(name: str, text: str) -> Draft:
    """Filter a drafted overlay. ``problems`` empty means it may be staged."""
    draft = Draft(name=name, body=(text or "").strip(), text=text or "")
    problems = draft.problems
    if not NAME_RE.match(name or ""):
        problems.append("name must be 3–48 lowercase letters, digits and dashes, starting with a letter")
    if not (MIN_BODY <= len(draft.body) <= MAX_BODY) or draft.body.count("\n") + 1 > MAX_BODY_LINES:
        problems.append(f"it must be {MIN_BODY}–{MAX_BODY} characters and at most {MAX_BODY_LINES} lines")
    problems.extend(textfilter.inspect(draft.body).problems)
    return draft


def _write_meta(path: Path, meta: dict[str, Any]) -> None:
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(meta, indent=1, sort_keys=True))
    os.replace(tmp, path.with_suffix(".json"))


def _read_meta(path: Path) -> dict[str, Any]:
    try:
        data = json.loads(path.with_suffix(".json").read_text())
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def _event(action: str, name: str, project: str | None, **extra: Any) -> None:
    try:
        state_store.append_event("prompt", {"action": action, "prompt": name, "project": project, **extra}, source="prompts")
    except Exception:
        logger.debug("prompts: could not write the ledger", exc_info=True)


def all_names(project: str | os.PathLike | None) -> set[str]:
    out: set[str] = set()
    for scope in {project_key(project), GLOBAL}:
        for status in STATUSES:
            base = home() / scope / status
            if base.is_dir():
                out.update(p.stem for p in base.glob("*.md"))
    return out


def drafted_today(project: str | os.PathLike | None, now: float | None = None) -> int:
    cutoff = (now or time.time()) - 86400
    return sum(1 for s in list_prompts(project, "staging") if float(s.get("created", 0)) >= cutoff) + sum(
        1 for s in list_prompts(project, "rejected") if float(s.get("created", 0)) >= cutoff
    )


@dataclass
class Staged:
    ok: bool
    name: str = ""
    problems: list[str] = field(default_factory=list)
    path: Path | None = None


def stage(
    project: str | os.PathLike | None,
    name: str,
    text: str,
    *,
    signals: list[str] | None = None,
    turns: list[str] | None = None,
    source: str = "",
    session_id: str | None = None,
    scope: str = "project",
) -> Staged:
    """Put a drafted overlay in staging, or say why not. What fails the filter is kept in ``rejected/`` with the
    reasons, so the same text is not drafted, and judged, again."""
    with _LOCK:
        target = None if scope == "global" else project
        draft = validate_draft(name, text)
        meta = {
            "name": name,
            "scope": scope,
            "project": os.path.realpath(str(project)) if project else None,
            "created": time.time(),
            "signals": (signals or [])[:8],
            "turns": (turns or [])[:8],
            "source": source,
            "uses": 0,
            "rewards": [],
            "evals": [],
        }
        if not draft.ok:
            if NAME_RE.match(name) and name not in all_names(project):
                path = _path(target, "rejected", name)
                _private(path.parent)
                path.write_text(draft.text)
                _write_meta(path, {**meta, "status": "rejected", "reason": "; ".join(draft.problems)})
            _event("reject", name, meta["project"], problems=draft.problems, stage="filter")
            return Staged(False, name, list(draft.problems))
        path = _path(target, "staging", name)
        _private(scope_dir(target))
        _private(path.parent)
        path.write_text(draft.body + "\n")
        _write_meta(path, {**meta, "status": "staging"})
        _event("draft", name, meta["project"], signals=meta["signals"], turns=meta["turns"], source=source)
        return Staged(True, name, [], path)


# ------------------------------------------------------------------------------ the store


def list_prompts(project: str | os.PathLike | None = None, status: str | None = None) -> list[dict[str, Any]]:
    """Every overlay visible from ``project`` (its own and the global ones), newest first."""
    rows: list[dict[str, Any]] = []
    if project is not None:
        scopes = [project_key(project), GLOBAL]
    else:
        scopes = [p.name for p in home().iterdir() if p.is_dir()] if home().is_dir() else []
    for scope in dict.fromkeys(scopes):
        for current in STATUSES:
            if status and current != status:
                continue
            base = home() / scope / current
            if not base.is_dir():
                continue
            for entry in sorted(base.glob("*.md")):
                rows.append({**_read_meta(entry), "name": entry.stem, "status": current, "scope_key": scope})
    return sorted(rows, key=lambda r: -float(r.get("created", 0)))


def find(name: str, project: str | os.PathLike | None, status: str | None = None) -> dict[str, Any] | None:
    return next((r for r in list_prompts(project, status) if r["name"] == name), None)


def read_text(row: dict[str, Any]) -> str:
    try:
        return (home() / row["scope_key"] / row["status"] / f"{row['name']}.md").read_text()
    except OSError:
        return ""


def _move(row: dict[str, Any], status: str, **meta_updates: Any) -> dict[str, Any]:
    source = home() / row["scope_key"] / row["status"] / f"{row['name']}.md"
    meta_src = source.with_suffix(".json")
    dest_dir = home() / row["scope_key"] / status
    _private(dest_dir)
    dest = dest_dir / source.name
    if dest.exists():
        dest.unlink()
    os.replace(source, dest)
    if meta_src.exists():
        meta_dest = dest.with_suffix(".json")
        if meta_dest.exists():
            meta_dest.unlink()
        os.replace(meta_src, meta_dest)
    meta = {**_read_meta(dest), "status": status, f"{status}_at": time.time(), **meta_updates}
    _write_meta(dest, meta)
    return {**meta, "name": row["name"], "status": status, "scope_key": row["scope_key"]}


def promote(name: str, project: str | os.PathLike | None, *, evidence: dict[str, Any] | None = None, by: str = "gate") -> dict[str, Any]:
    """staging → live. Re-validates first: a file edited in place since it was staged gets no free pass."""
    with _LOCK:
        row = find(name, project, "staging")
        if row is None:
            return {"ok": False, "reason": f"no staged prompt {name!r}"}
        draft = validate_draft(name, read_text(row))
        if not draft.ok:
            _move(row, "rejected", reason="; ".join(draft.problems))
            _event("reject", name, row.get("project"), problems=draft.problems, stage="promote")
            return {"ok": False, "reason": "; ".join(draft.problems)}
        moved = _move(row, "live", promoted_by=by, evidence=evidence or {})
        _event("promote", name, row.get("project"), by=by, evidence=evidence or {})
        return {"ok": True, "prompt": moved}


def reject(name: str, project: str | os.PathLike | None, reason: str = "rejected", by: str = "person") -> dict[str, Any]:
    with _LOCK:
        row = find(name, project, "staging") or find(name, project, "live")
        if row is None:
            return {"ok": False, "reason": f"no prompt {name!r}"}
        moved = _move(row, "rejected", reason=reason)
        _event("reject", name, row.get("project"), by=by, reason=reason, stage=row["status"])
        return {"ok": True, "prompt": moved}


def retire(name: str, project: str | os.PathLike | None, reason: str, by: str = "health") -> dict[str, Any]:
    with _LOCK:
        row = find(name, project, "live")
        if row is None:
            return {"ok": False, "reason": f"no live prompt {name!r}"}
        moved = _move(row, "retired", reason=reason)
        _event("retire", name, row.get("project"), by=by, reason=reason)
        return {"ok": True, "prompt": moved}


def record_eval(name: str, project: str | os.PathLike | None, verdict: dict[str, Any]) -> None:
    """Attach an evaluation to a staged overlay (kept to the last five)."""
    with _LOCK:
        row = find(name, project, "staging")
        if row is None:
            return
        path = home() / row["scope_key"] / "staging" / f"{name}.md"
        meta = _read_meta(path)
        meta["evals"] = [*(meta.get("evals") or []), {**verdict, "ts": time.time()}][-5:]
        _write_meta(path, meta)
        _event("eval", name, row.get("project"), verdict=verdict)


# ------------------------------------------------------------------------------ reading one into a prompt


def _overlay_from_env() -> str | None:
    """The promotion arm's candidate: a file named by ``CORDATA_PROMPT_OVERLAY``, if it is set and readable."""
    path = os.environ.get(OVERLAY_ENV, "")
    if not path:
        return None
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def _scopes(project: str | os.PathLike | None) -> list[str]:
    """The stores a session in ``project`` reads: its own, then the global ones. One entry when project is None
    (``project_key(None)`` *is* ``GLOBAL``), so a global overlay is never appended twice."""
    return list(dict.fromkeys([project_key(project), GLOBAL]))


def live_text(project: str | os.PathLike | None) -> str:
    """The overlay block for a session in ``project``: the arm's candidate when one is set, else the live
    overlays of the project (its own, then the global ones). ``""`` when there is nothing to add."""
    arm = _overlay_from_env()
    parts: list[str] = []
    if arm:
        parts.append(arm)
    else:
        for scope in _scopes(project):
            base = home() / scope / "live"
            if not base.is_dir():
                continue
            for entry in sorted(base.glob("*.md")):
                try:
                    parts.append(entry.read_text().strip())
                except OSError:
                    continue
    parts = [p for p in parts if p]
    if not parts:
        return ""
    # Its own block, named, at the end of the system prompt: the boundary between the harness's own framing and
    # prose a model wrote for itself has to be unambiguous to a person reading the prompt.
    return "\n\n# " + HEADING + "\n\n" + "\n\n".join(parts) + "\n"


def live_names(project: str | os.PathLike | None) -> list[str]:
    out: list[str] = []
    for scope in _scopes(project):
        base = home() / scope / "live"
        if base.is_dir():
            out.extend(p.stem for p in sorted(base.glob("*.md")))
    return out


# ------------------------------------------------------------------------------ health


def record_use(project: str | os.PathLike | None, names: list[str], reward: float, turn: str, baseline: float | None = None) -> list[str]:
    """A turn that had these overlays in front of it scored ``reward``. Returns the names retired as a result.

    Same rule as a skill's (``learned.record_use``): judged on the turns that had it against the project's other
    turns, retired only when it has been used at least three times and its recent turns are clearly worse."""
    retired: list[str] = []
    with _LOCK:
        for name in dict.fromkeys(names):
            row = find(name, project, "live")
            if row is None:
                continue
            path = home() / row["scope_key"] / "live" / f"{name}.md"
            meta = _read_meta(path)
            meta["uses"] = int(meta.get("uses", 0)) + 1
            meta["last_used"] = time.time()
            meta["rewards"] = [*(meta.get("rewards") or []), [turn, round(float(reward), 4)]][-REWARD_WINDOW:]
            _write_meta(path, meta)
            _event("use", name, row.get("project"), turn=turn, reward=round(float(reward), 4))
            verdict = unhealthy(meta, baseline)
            if verdict:
                retire(name, project, verdict)
                retired.append(name)
    return retired


def unhealthy(meta: dict[str, Any], baseline: float | None) -> str | None:
    values = [float(r[1]) for r in meta.get("rewards") or [] if isinstance(r, (list, tuple)) and len(r) == 2]
    if int(meta.get("uses", 0)) < MIN_USES_TO_JUDGE or len(values) < MIN_USES_TO_JUDGE:
        return None
    last3, last5 = values[-3:], values[-5:]
    if all(v < -0.1 for v in last3):
        return f"its last three turns all scored badly ({', '.join(f'{v:+.2f}' for v in last3)})"
    if baseline is not None and sum(last5) / len(last5) < baseline - 0.15:
        return f"turns that had it in front of them averaged {sum(last5) / len(last5):+.2f}, against {baseline:+.2f} for the project's other turns"
    return None
