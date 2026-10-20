"""Learned skills: the store, the filter, and the health record.

A *learned skill* is a lesson the harness drafted for itself after a turn went badly — a short
``SKILL.md`` (the same format the shipped skills use, loaded the same way by Strands'
``AgentSkills``: the model sees its one-line description and reads the body only when it applies).
Nothing a model writes ever goes straight into the model's own future instructions. A skill passes
through four places, and only a person's settings or a measured result moves it between them:

    staging/   drafted by ``reflect.py`` after a failed or reworked turn. Never loaded.
    live/      promoted — by the eval gate (``promote.py``) under ``learning.promotion: live``, or by a
               person. Loaded into new sessions *in the project it was learned in* (``global`` scope is
               a manual decision).
    retired/   was live, stopped helping (``record_use``), or was withdrawn. Kept, not loaded.
    rejected/  failed the filter or the gate, or a person said no. Kept so the same lesson is not drafted again.

Layout (``~/.cordata/learned/<project key>/<status>/<name>/{SKILL.md,meta.json}``). All of it is
local, private (0700) and auditable: every transition is a ``skill`` event in the ledger.

**The filter is not the defence.** A skill is prose a model will read and may follow, and the text it
was drafted from (a failing test's output) is untrusted. ``validate_draft`` refuses the obvious —
URLs, "ignore your instructions", "skip the approval", credential-shaped strings, oversize — but
a determined sentence can be phrased past any pattern list. What bounds the damage is structural:
a skill can only *advise* (it cannot approve a call, lower a Guard score or switch a mode — those
decisions live in code that never reads a skill), it is loaded only into the project it came from,
it is not loaded at all until it has beaten the baseline on the eval suite, and it is retired when
the turns that used it score worse than the turns that did not.

Stdlib plus ``state_store`` and ``secret_patterns``; no model, no Strands.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import re
import shutil
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
MIN_DESC, MAX_DESC = 20, 300
MAX_DRAFTS_PER_DAY = 3
MIN_USES_TO_JUDGE = 3
REWARD_WINDOW = 12
_LOCK = threading.RLock()

#: The banned-phrase filter lives in ``textfilter`` so the prompt-overlay store (``prompts.py``) enforces the
#: exact same rules. ``_BANNED`` stays exported for callers that read it.
_BANNED = textfilter.BANNED
_BANNED_RE = [(re.compile(p, re.IGNORECASE), why) for p, why in _BANNED]


# ------------------------------------------------------------------------------ paths


def home() -> Path:
    return Path(os.environ.get("CORDATA_HOME") or Path.home() / ".cordata") / "learned"


def project_key(project: str | os.PathLike | None) -> str:
    if project is None:
        return GLOBAL
    try:
        real = os.path.realpath(str(project))
    except OSError:
        real = str(project)
    return hashlib.sha1(real.encode()).hexdigest()[:16]


def scope_dir(project: str | os.PathLike | None) -> Path:
    return home() / project_key(project)


def _skill_dir(project: str | os.PathLike | None, status: str, name: str) -> Path:
    return scope_dir(project) / status / name


def shipped_names() -> set[str]:
    """The names of the skills that ship with the harness: a learned skill must not shadow one."""
    base = Path(__file__).resolve().parent / "skills"
    try:
        return {p.name for p in base.iterdir() if p.is_dir()}
    except OSError:
        return set()


# ------------------------------------------------------------------------------ drafts


@dataclass
class Draft:
    name: str = ""
    description: str = ""
    body: str = ""
    text: str = ""
    problems: list[str] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not self.problems


_FRONT = re.compile(r"\A\s*---\s*\n(?P<front>.*?)\n---\s*\n(?P<body>.*)\Z", re.DOTALL)


def parse(text: str) -> Draft:
    draft = Draft(text=text or "")
    found = _FRONT.match(draft.text)
    if not found:
        draft.problems.append("no '---' frontmatter with a name and description")
        return draft
    fields = {}
    for line in found.group("front").splitlines():
        key, sep, value = line.partition(":")
        if sep:
            fields[key.strip().lower()] = value.strip().strip("\"'")
    draft.name, draft.description, draft.body = fields.get("name", ""), fields.get("description", ""), found.group("body").strip()
    return draft


def validate_draft(text: str, *, taken: set[str] | None = None) -> Draft:
    """Parse and filter a drafted ``SKILL.md``. ``problems`` is empty when it may be staged."""
    draft = parse(text)
    problems = draft.problems
    if problems:
        return draft
    if not NAME_RE.match(draft.name):
        problems.append("name must be 3–48 lowercase letters, digits and dashes, starting with a letter")
    elif draft.name in shipped_names() or draft.name in (taken or set()):
        problems.append(f"name {draft.name!r} is already taken")
    if "\n" in draft.description or not (MIN_DESC <= len(draft.description) <= MAX_DESC):
        problems.append(f"description must be one line of {MIN_DESC}–{MAX_DESC} characters")
    if not re.match(r"(?i)^use when\b", draft.description):
        problems.append("description must start with 'Use when' — it is what tells the model when to read the skill")
    if not (MIN_BODY <= len(draft.body) <= MAX_BODY) or draft.body.count("\n") + 1 > MAX_BODY_LINES:
        problems.append(f"body must be {MIN_BODY}–{MAX_BODY} characters and at most {MAX_BODY_LINES} lines")
    whole = f"{draft.description}\n{draft.body}"
    problems.extend(textfilter.inspect(whole).problems)
    if textfilter.CONTROL_CHARS.search(draft.text) and not any("control" in p for p in problems):
        problems.append("it contains control or bidirectional-override characters")
    return draft


def _write_meta(path: Path, meta: dict[str, Any]) -> None:
    tmp = path / "meta.json.tmp"
    tmp.write_text(json.dumps(meta, indent=1, sort_keys=True))
    os.replace(tmp, path / "meta.json")


def _read_meta(path: Path) -> dict[str, Any]:
    try:
        data = json.loads((path / "meta.json").read_text())
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def _event(action: str, name: str, project: str | None, **extra: Any) -> None:
    try:
        state_store.append_event("skill", {"action": action, "skill": name, "project": project, **extra}, source="learned")
    except Exception:
        logger.debug("learned: could not write the ledger", exc_info=True)


def _private(path: Path) -> None:
    try:
        path.mkdir(parents=True, exist_ok=True)
        os.chmod(path, 0o700)
    except OSError:
        pass


def all_names(project: str | os.PathLike | None) -> set[str]:
    out: set[str] = set()
    for scope in {project_key(project), GLOBAL}:
        for status in STATUSES:
            base = home() / scope / status
            if base.is_dir():
                out.update(p.name for p in base.iterdir() if p.is_dir())
    return out


def drafted_today(project: str | os.PathLike | None, now: float | None = None) -> int:
    """Drafts made in the last day, whatever became of them (staged, promoted, retired or rejected)."""
    cutoff = (now or time.time()) - 86400
    key = project_key(project)
    return sum(1 for s in list_skills(project) if s.get("scope_key") == key and float(s.get("created", 0)) >= cutoff)


@dataclass
class Staged:
    ok: bool
    name: str = ""
    problems: list[str] = field(default_factory=list)
    path: Path | None = None


def stage(
    project: str | os.PathLike | None,
    text: str,
    *,
    signals: list[str] | None = None,
    turns: list[str] | None = None,
    family: str = "",
    key: str = "",
    session_id: str | None = None,
    scope: str = "project",
    source: str = "",
    attribution: dict[str, Any] | None = None,
) -> Staged:
    """Put a drafted skill in staging, or say why not. A draft that fails the filter is kept in ``rejected/``
    (with the reasons) so the same lesson is not drafted, and judged, again. ``source`` records what the draft was
    written *from* — a failure family, or the name of a live skill it is a variant of (``optimize.py``). ``attribution``
    is what ``reflect.py``'s drafter attributed (rows, evidence, confidence); its ``cue`` is the description's
    situation, the text ``skill_search`` embeds."""
    with _LOCK:
        target = None if scope == "global" else project
        draft = validate_draft(text, taken=all_names(project))
        meta = {
            "name": draft.name,
            "scope": scope,
            "project": os.path.realpath(str(project)) if project else None,
            "created": time.time(),
            "signals": (signals or [])[:8],
            "turns": (turns or [])[:8],
            "family": family,
            "key": key,
            "source": source,
            "description": draft.description,
            "uses": 0,
            "rewards": [],
            "evals": [],
        }
        if attribution is not None:
            cue = re.sub(r"^use when\s+", "", draft.description or "", flags=re.IGNORECASE).strip()
            meta["attribution"] = {
                "cue": cue,
                "rows": list(attribution.get("rows") or [])[:8],
                "evidence": str(attribution.get("evidence") or ""),
                "confidence": str(attribution.get("confidence") or ""),
            }
        if not draft.ok:
            if NAME_RE.match(draft.name or "") and draft.name not in all_names(project):
                path = _skill_dir(target, "rejected", draft.name)
                _private(path)
                (path / "SKILL.md").write_text(draft.text)
                _write_meta(path, {**meta, "status": "rejected", "reason": "; ".join(draft.problems)})
            _event("reject", draft.name, meta["project"], problems=draft.problems, stage="filter")
            return Staged(False, draft.name, list(draft.problems))
        path = _skill_dir(target, "staging", draft.name)
        _private(scope_dir(target))
        _private(path)
        (path / "SKILL.md").write_text(draft.text.strip() + "\n")
        _write_meta(path, {**meta, "status": "staging"})
        _event("draft", draft.name, meta["project"], signals=meta["signals"], turns=meta["turns"], family=family)
        return Staged(True, draft.name, [], path)


# ------------------------------------------------------------------------------ the store


def list_skills(project: str | os.PathLike | None = None, status: str | None = None) -> list[dict[str, Any]]:
    """Every learned skill visible from ``project`` (its own and the global ones), newest first."""
    rows: list[dict[str, Any]] = []
    scopes = [project_key(project)] if project is not None else [p.name for p in home().iterdir() if p.is_dir()] if home().is_dir() else []
    if project is not None and GLOBAL not in scopes:
        scopes.append(GLOBAL)
    for scope in scopes:
        for current in STATUSES:
            if status and current != status:
                continue
            base = home() / scope / current
            if not base.is_dir():
                continue
            for entry in sorted(base.iterdir()):
                if entry.is_dir() and (entry / "SKILL.md").exists():
                    meta = _read_meta(entry)
                    rows.append({**meta, "name": entry.name, "status": current, "scope_key": scope})
    return sorted(rows, key=lambda r: -float(r.get("created", 0)))


def find(name: str, project: str | os.PathLike | None, status: str | None = None) -> dict[str, Any] | None:
    return next((r for r in list_skills(project, status) if r["name"] == name), None)


def read_text(row: dict[str, Any]) -> str:
    try:
        return (home() / row["scope_key"] / row["status"] / row["name"] / "SKILL.md").read_text()
    except OSError:
        return ""


def _move(row: dict[str, Any], status: str, **meta_updates: Any) -> dict[str, Any]:
    source = home() / row["scope_key"] / row["status"] / row["name"]
    dest = home() / row["scope_key"] / status / row["name"]
    _private(dest.parent)
    if dest.exists():
        shutil.rmtree(dest)
    os.replace(source, dest)
    meta = {**_read_meta(dest), "status": status, f"{status}_at": time.time(), **meta_updates}
    _write_meta(dest, meta)
    return {**meta, "name": row["name"], "status": status, "scope_key": row["scope_key"]}


def promote(name: str, project: str | os.PathLike | None, *, evidence: dict[str, Any] | None = None, by: str = "gate") -> dict[str, Any]:
    """staging → live. Re-validates first: a file edited in place since it was staged gets no free pass."""
    with _LOCK:
        row = find(name, project, "staging")
        if row is None:
            return {"ok": False, "reason": f"no staged skill {name!r}"}
        draft = validate_draft(read_text(row), taken=all_names(project) - {name})
        if not draft.ok:
            _move(row, "rejected", reason="; ".join(draft.problems))
            _event("reject", name, row.get("project"), problems=draft.problems, stage="promote")
            return {"ok": False, "reason": "; ".join(draft.problems)}
        moved = _move(row, "live", promoted_by=by, evidence=evidence or {})
        _event("promote", name, row.get("project"), by=by, evidence=evidence or {})
        return {"ok": True, "skill": moved}


def reject(name: str, project: str | os.PathLike | None, reason: str = "rejected", by: str = "person") -> dict[str, Any]:
    with _LOCK:
        row = find(name, project, "staging") or find(name, project, "live")
        if row is None:
            return {"ok": False, "reason": f"no skill {name!r}"}
        moved = _move(row, "rejected", reason=reason)
        _event("reject", name, row.get("project"), by=by, reason=reason, stage=row["status"])
        return {"ok": True, "skill": moved}


def retire(name: str, project: str | os.PathLike | None, reason: str, by: str = "health") -> dict[str, Any]:
    with _LOCK:
        row = find(name, project, "live")
        if row is None:
            return {"ok": False, "reason": f"no live skill {name!r}"}
        moved = _move(row, "retired", reason=reason)
        _event("retire", name, row.get("project"), by=by, reason=reason)
        return {"ok": True, "skill": moved}


def record_eval(name: str, project: str | os.PathLike | None, verdict: dict[str, Any]) -> None:
    """Attach an evaluation to a staged skill (kept to the last five) — shadow mode's whole output."""
    with _LOCK:
        row = find(name, project, "staging")
        if row is None:
            return
        path = home() / row["scope_key"] / "staging" / name
        meta = _read_meta(path)
        meta["evals"] = [*(meta.get("evals") or []), {**verdict, "ts": time.time()}][-5:]
        _write_meta(path, meta)
        _event("eval", name, row.get("project"), verdict=verdict)


def live_dirs(project: str | os.PathLike | None) -> list[str]:
    """The directories to hand ``AgentSkills`` for a session in ``project``: its live skills, then the global ones.
    One entry when ``project`` is None (``project_key(None)`` *is* ``GLOBAL``)."""
    out = []
    for scope in dict.fromkeys([project_key(project), GLOBAL]):
        base = home() / scope / "live"
        if base.is_dir() and any((p / "SKILL.md").exists() for p in base.iterdir() if p.is_dir()):
            out.append(str(base))
    return out


# ------------------------------------------------------------------------------ health


def record_use(project: str | os.PathLike | None, names: list[str], reward: float, turn: str, baseline: float | None = None) -> list[str]:
    """A turn that read these skills scored ``reward``. Returns the names retired as a result.

    Judged on the turns that used the skill, against the turns that did not (``baseline``, the project's
    recent mean): retired when it has been used at least three times and its last five average more than
    0.15 below the baseline, or its last three were all clearly negative. A skill that merely does not
    help is left alone; one that coincides with worse turns is not."""
    retired: list[str] = []
    with _LOCK:
        for name in dict.fromkeys(names):
            row = find(name, project, "live")
            if row is None:
                continue
            path = home() / row["scope_key"] / "live" / name
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
        return f"turns that used it averaged {sum(last5) / len(last5):+.2f}, against {baseline:+.2f} for the project's other turns"
    return None
