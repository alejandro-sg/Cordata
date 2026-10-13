"""Run tasks through the real harness and score them.

A run drives the real ACP server (the same object a client talks to): a session in a throwaway git
repo built from the task, the task's prompt, a permission policy standing in for the person, and a
*driver* standing in for the model:

* ``oracle``  — the task's own solution as a scripted model. This validates the **harness** (tools,
  gates, hooks, recording) and the **task** (solvable, checkable). Free, instant, deterministic.
* ``replay`` / ``record`` — a cassette of a live model's answers (``cassette.py``): the live run is
  paid for once, then replayed identically.
* ``live``    — the configured provider, as a person would use it. Costs money and varies run to run.

Each task ends with the verifier (the repo's own checks), the expectations (files that must not
change, approvals that must have been put to a person, text the answer must contain, secrets that
must not have reached the model) and the turn's outcome record, so a result carries the reward
proxy alongside pass/fail. Aggregates split **dev** from **held-out**: tune on one, report the other.

The agent runs in a sandbox — a temporary ledger, home and session directory — so evals never write
into (or learn from) the user's real history.
"""

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from collections.abc import Iterator
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

from .task import Task, check_shape, render_command, steps_for

PYTHON = sys.executable
VERIFY_TIMEOUT = 60.0


# ------------------------------------------------------------------------------ the repo


def materialize(task: Task, root: Path) -> Path:
    """The task's files in a fresh git repository (committed, so diffs and rollbacks work)."""
    repo = root / task.id
    if repo.exists():
        shutil.rmtree(repo)
    for rel, content in task.files.items():
        path = repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
    env = {**os.environ, "GIT_AUTHOR_NAME": "evals", "GIT_AUTHOR_EMAIL": "evals@example.invalid", "GIT_COMMITTER_NAME": "evals", "GIT_COMMITTER_EMAIL": "evals@example.invalid"}
    for argv in (["git", "init", "-q", "-b", "main"], ["git", "add", "-A"], ["git", "commit", "-q", "-m", "initial", "--no-gpg-sign"]):
        subprocess.run(argv, cwd=repo, env=env, capture_output=True, timeout=30)
    return repo


def _purge_bytecode(repo: Path) -> None:
    """Drop compiled Python from ``repo`` before a verifier run. A task's buggy and fixed files are often the
    *same byte length*, and the two runs land in the same wall-clock second — so the ``(mtime, size)`` stamp in
    a stale ``.pyc`` still matches, and the second run would import the *first* run's module and grade the
    candidate against the code it just replaced (``bug_clamp``, ``multi_unpack_order``)."""
    for path in repo.rglob("__pycache__"):
        shutil.rmtree(path, ignore_errors=True)
    for path in repo.rglob("*.pyc"):
        path.unlink(missing_ok=True)


def run_commands(commands: tuple[str, ...], repo: Path, *, python: str = PYTHON, timeout: float = VERIFY_TIMEOUT) -> tuple[bool, list[str]]:
    """Run the verifier; ``(all passed, one line per failing command)``."""
    _purge_bytecode(repo)
    failures = []
    for command in commands:
        try:
            run = subprocess.run(render_command(command, python), shell=True, cwd=repo, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            failures.append(f"timed out: {command[:80]}")
            continue
        if run.returncode != 0:
            tail = (run.stderr or run.stdout).strip().splitlines()[-1:] or [""]
            failures.append(f"exit {run.returncode}: {command[:70]} — {tail[0][:100]}")
    return not failures, failures


def tree_hashes(repo: Path) -> dict[str, str]:
    """``{path: content hash}`` for the repo's working files (not .git)."""
    out = {}
    for path in sorted(repo.rglob("*")):
        if path.is_file() and ".git" not in path.relative_to(repo).parts and "__pycache__" not in path.parts:
            out[str(path.relative_to(repo))] = hashlib.sha256(path.read_bytes()).hexdigest()[:16]
    return out


def validate_task(task: Task, *, python: str = PYTHON) -> list[str]:
    """Is this task a fair question? Its definition is well-formed, its verifier **fails** on the untouched
    repo, and it **passes** once the oracle's files are in place. Returns the problems (empty = valid).
    ``script`` tasks (answers, attacks) have no file oracle; their expectations are checked by running them."""
    problems = check_shape(task)
    if problems:
        return problems
    if not task.verify or task.fixed is None:
        return []
    with tempfile.TemporaryDirectory() as tmp:
        repo = materialize(task, Path(tmp))
        passed, _ = run_commands(task.verify, repo, python=python)
        if passed:
            problems.append(f"{task.id}: the verifier already passes on the untouched repo — it proves nothing")
        for rel, content in task.fixed.items():
            path = repo / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        passed, failures = run_commands(task.verify, repo, python=python)
        if not passed:
            problems.append(f"{task.id}: the oracle's solution does not pass its own verifier: {failures}")
    return problems


# ------------------------------------------------------------------------------ results


@dataclass
class TaskResult:
    id: str
    category: str
    split: str
    passed: bool
    reasons: list[str] = field(default_factory=list)
    seconds: float = 0.0
    calls: int = 0
    asked: int = 0
    tokens: int = 0
    reward: float | None = None
    reward_parts: dict[str, float] = field(default_factory=dict)
    response: str = ""
    driver: str = ""
    #: Which arm of an A/B produced this result (``baseline`` / ``candidate``), so a saved run says what it measured.
    arm: str = ""

    def to_json(self) -> dict[str, Any]:
        return asdict(self)


@dataclass
class RunResult:
    run_id: str
    started: float
    driver: str
    config: dict[str, Any]
    results: list[TaskResult]
    aggregate: dict[str, Any]

    def to_json(self) -> dict[str, Any]:
        return {"run_id": self.run_id, "started": self.started, "driver": self.driver, "config": self.config, "aggregate": self.aggregate, "results": [r.to_json() for r in self.results]}


def aggregate(results: list[TaskResult]) -> dict[str, Any]:
    def block(rows: list[TaskResult]) -> dict[str, Any]:
        if not rows:
            return {"tasks": 0}
        rewards = [r.reward for r in rows if r.reward is not None]
        return {
            "tasks": len(rows),
            "passed": sum(1 for r in rows if r.passed),
            "pass_rate": round(sum(1 for r in rows if r.passed) / len(rows), 4),
            "mean_reward": round(sum(rewards) / len(rewards), 4) if rewards else None,
            "mean_calls": round(sum(r.calls for r in rows) / len(rows), 2),
            "mean_seconds": round(sum(r.seconds for r in rows) / len(rows), 2),
            "tokens": sum(r.tokens for r in rows),
        }

    out = {"all": block(results), "dev": block([r for r in results if r.split == "dev"]), "heldout": block([r for r in results if r.split == "heldout"])}
    out["by_category"] = {c: block([r for r in results if r.category == c]) for c in sorted({r.category for r in results})}
    out["failed"] = [r.id for r in results if not r.passed]
    return out


# ------------------------------------------------------------------------------ the sandbox


@contextlib.contextmanager
def sandbox(root: Path, *, live: bool = False, env: dict[str, str] | None = None) -> Iterator[None]:
    """A private ledger, home and session directory, and the harness's network-touching bits quieted, so a
    run never reads or writes the user's history. ``env`` is the arm the run is measuring — a candidate skill
    (``CORDATA_EXTRA_SKILLS``) or prompt overlay (``CORDATA_PROMPT_OVERLAY``) put in force for the run and
    removed on exit. Restores everything on exit."""
    saved_env = {k: os.environ.get(k) for k in ("CORDATA_HOME", "CORDATA_STATE_DB", "BYPASS_TOOL_CONSENT", "LAYA_TRIAGE_MODE", "LAYA_TRIAGE_LOG", "CORDATA_PRISM", "CORDATA_REFLECT", *(env or {}))}
    os.environ.update(
        CORDATA_HOME=str(root / "home"),
        CORDATA_STATE_DB=str(root / "state.db"),
        BYPASS_TOOL_CONSENT="true",
        LAYA_TRIAGE_MODE="off",
        LAYA_TRIAGE_LOG=str(root / "laya.jsonl"),
        CORDATA_PRISM="off",
        CORDATA_REFLECT="off",  # an eval turn is not a lesson
        **(env or {}),
    )
    import acp_server
    import agent_common
    import plugin_host
    import prompt_security
    import specialists

    sessions = root / "sessions"
    sessions.mkdir(parents=True, exist_ok=True)
    saved = {
        "sessions_dir": agent_common.SESSIONS_DIR,
        "acp_sessions": acp_server.SESSIONS_DIR,
        "build_model": agent_common.build_model,
        "ensure": agent_common.ensure_provider_ready,
        "fetch": acp_server._fetch_available_models,
        "laya_ready": prompt_security.laya_ready,
        "mcp": plugin_host.mcp_config_paths,
        "spec_model": specialists._model,
        "cwd": os.getcwd(),
    }
    agent_common.SESSIONS_DIR = sessions
    acp_server.SESSIONS_DIR = sessions
    acp_server._fetch_available_models = lambda: []
    prompt_security.laya_ready = lambda: False  # no forward pass: the guard runs on its rules, deterministically
    real_paths = plugin_host.mcp_config_paths
    plugin_host.mcp_config_paths = lambda cwd: [p for p in real_paths(cwd) if p[0] != "backend"]
    if not live:
        agent_common.ensure_provider_ready = lambda: None
    try:
        yield
    finally:
        agent_common.SESSIONS_DIR = saved["sessions_dir"]
        acp_server.SESSIONS_DIR = saved["acp_sessions"]
        agent_common.build_model = saved["build_model"]
        agent_common.ensure_provider_ready = saved["ensure"]
        acp_server._fetch_available_models = saved["fetch"]
        prompt_security.laya_ready = saved["laya_ready"]
        plugin_host.mcp_config_paths = saved["mcp"]
        specialists._model = saved["spec_model"]
        with contextlib.suppress(OSError):
            os.chdir(saved["cwd"])
        for key, value in saved_env.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


class EvalConn:
    """The client: collects what the agent says, and answers permission requests by policy."""

    def __init__(self, approvals: str) -> None:
        self.approvals = approvals
        self.text: list[str] = []
        self.asked = 0

    async def session_update(self, session_id, update, **kwargs):
        if type(update).__name__ == "AgentMessageChunk":
            self.text.append(getattr(getattr(update, "content", None), "text", "") or "")

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        import acp

        self.asked += 1
        choice = "reject" if self.approvals == "deny" else "allow"
        return acp.schema.RequestPermissionResponse(outcome=acp.schema.AllowedOutcome(outcome="selected", option_id=choice))

    async def ext_notification(self, method, params):
        return None


# ------------------------------------------------------------------------------ drivers


class Driver:
    """Supplies the model for a task. ``seen`` is what the model was shown (for the 'secret never reached it' check)."""

    name = "driver"

    def model_for(self, task: Task, repo: Path) -> Any:  # pragma: no cover - interface
        raise NotImplementedError

    def seen(self) -> str:
        return ""


class OracleDriver(Driver):
    name = "oracle"

    def __init__(self) -> None:
        self.model: Any = None

    def model_for(self, task: Task, repo: Path) -> Any:
        from harness_testing import ScriptedModel, text, tool

        steps = []
        for step in steps_for(task, PYTHON):
            steps.append(tool(step["tool"], step.get("input")) if "tool" in step else text(step["text"]))
        self.model = ScriptedModel(steps, model_id="oracle")
        return self.model

    def seen(self) -> str:
        return "\n".join(str(call.get("last_text") or "") for call in (self.model.calls if self.model else []))


class CassetteDriver(Driver):
    def __init__(self, directory: str | Path, *, record: bool = False) -> None:
        self.directory = Path(directory)
        self.record = record
        self.name = "record" if record else "replay"
        self.model: Any = None

    def model_for(self, task: Task, repo: Path) -> Any:
        from .cassette import CassetteModel

        path = self.directory / f"{task.id}.jsonl"
        scrub = {str(repo): "<WORK>", str(repo.resolve()): "<WORK>"}
        if self.record:
            import agent_common

            path.unlink(missing_ok=True)
            self.model = CassetteModel(path, mode="record", inner=agent_common.build_model(), scrub=scrub)
        else:
            self.model = CassetteModel(path, mode="replay", scrub=scrub)
        return self.model


class LiveDriver(Driver):
    name = "live"

    def model_for(self, task: Task, repo: Path) -> Any:
        return None  # leave agent_common.build_model alone: the configured provider


# ------------------------------------------------------------------------------ one task


def _expectation_failures(task: Task, repo: Path, before: dict[str, str], conn: EvalConn, response: str, driver: Driver, calls: int) -> list[str]:
    expect = task.expect
    after = tree_hashes(repo)
    reasons = []
    for rel in expect.get("unchanged", []):
        if before.get(rel) != after.get(rel):
            reasons.append(f"{rel} changed but must not have")
    for rel in expect.get("absent", []):
        if rel in after:
            reasons.append(f"{rel} exists but must not")
    if expect.get("no_changes") and before != after:
        changed = sorted({*before, *after} - {k for k in before if before.get(k) == after.get(k)})
        reasons.append(f"files changed but nothing should have: {changed[:5]}")
    if "response" in expect and not re.search(expect["response"], response, re.IGNORECASE | re.DOTALL):
        reasons.append(f"the answer does not match /{expect['response']}/")
    if conn.asked < int(expect.get("asked_min", 0)):
        reasons.append(f"the harness put {conn.asked} approvals to a person; at least {expect['asked_min']} were required")
    if "max_calls" in expect and calls > int(expect["max_calls"]):
        reasons.append(f"{calls} tool calls; at most {expect['max_calls']} allowed")
    secret = expect.get("transcript_must_not_contain")
    if secret and (secret in response or secret in driver.seen()):
        reasons.append("a secret reached the model's context or the answer")
    return reasons


async def run_task(task: Task, driver: Driver, *, workdir: Path, timeout: float = 180.0, config: dict[str, Any] | None = None) -> TaskResult:
    """Run one task in the current (already sandboxed) process."""
    import acp
    import acp_server
    import agent_common
    import outcomes
    import specialists
    import state_store

    started = time.time()
    repo = materialize(task, workdir)
    before = tree_hashes(repo)
    model = driver.model_for(task, repo)
    if model is not None:
        agent_common.build_model = lambda model_id=None, _m=model: _m
        from harness_testing import ScriptedModel, text

        specialists._model = lambda tier, role=None: ScriptedModel([text("(specialist idle)")])
    conn = EvalConn(task.approvals)
    server = acp_server.CordataAcpAgent()
    server.on_connect(conn)
    reasons: list[str] = []
    session_id = ""
    stop = "error"
    try:
        created = await server.new_session(cwd=str(repo))
        session_id = created.session_id
        await server.set_session_mode(session_id, task.mode)
        try:
            response = await asyncio.wait_for(server.prompt(session_id, [acp.text_block(task.prompt)]), timeout)
            stop = str(getattr(response, "stop_reason", "end_turn"))
        except asyncio.TimeoutError:
            await server.cancel(session_id)
            reasons.append(f"timed out after {timeout:.0f}s")
            stop = "timeout"
    except Exception as error:  # a harness failure is a result, not a crash of the whole run
        reasons.append(f"{type(error).__name__}: {str(error)[:200]}")
    for _ in range(5):
        await asyncio.sleep(0.02)  # let notifications flush
    response_text = "".join(conn.text)

    if task.verify:
        passed, failures = run_commands(task.verify, repo)
        if not passed:
            reasons.extend(failures)
    record = None
    if session_id:
        rows = state_store.recent_events(kind=outcomes.KIND, session_id=session_id, limit=1)
        record = rows[0]["payload"] if rows else None
    calls = int(record["calls"]) if record else 0
    reasons.extend(_expectation_failures(task, repo, before, conn, response_text, driver, calls))
    reward = outcomes.score(record) if record else None
    return TaskResult(
        id=task.id,
        category=task.category,
        split=task.split,
        passed=not reasons,
        reasons=reasons,
        seconds=round(time.time() - started, 2),
        calls=calls,
        asked=conn.asked,
        tokens=int((record or {}).get("usage", {}).get("total", 0)),
        reward=reward.value if reward else None,
        reward_parts=reward.parts if reward else {},
        response=response_text[:600],
        driver=driver.name,
        arm=str((config or {}).get("arm") or ""),
    )


# ------------------------------------------------------------------------------ a suite


def select(tasks: tuple[Task, ...], *, suite: str = "all", category: str | None = None, ids: list[str] | None = None) -> list[Task]:
    chosen = [t for t in tasks if suite == "all" or t.split == suite]
    if category:
        chosen = [t for t in chosen if t.category == category]
    if ids:
        chosen = [t for t in chosen if t.id in ids]
    return chosen


def _arm_env(config: dict[str, Any] | None) -> dict[str, str]:
    """The environment a run's arm needs, from its ``config``: the candidate skill directory and/or prompt
    overlay path. This is what puts a candidate *in force* for the run — the point of the A/B."""
    env: dict[str, str] = {}
    if (skill_dir := (config or {}).get("skill_dir")):
        env["CORDATA_EXTRA_SKILLS"] = str(skill_dir)
    if (overlay := (config or {}).get("prompt_overlay")):
        env["CORDATA_PROMPT_OVERLAY"] = str(overlay)
    return env


def run_suite(tasks: list[Task], driver: Driver, *, config: dict[str, Any] | None = None, out_dir: Path | None = None, timeout: float = 180.0, on_result=None) -> RunResult:
    """Run ``tasks`` one after another in a sandbox and return (and, with ``out_dir``, save) the result."""
    run_id = time.strftime("%Y%m%d-%H%M%S") + "-" + uuid.uuid4().hex[:6]
    started = time.time()
    results: list[TaskResult] = []
    with tempfile.TemporaryDirectory(prefix="evals-") as tmp:
        root = Path(tmp)
        with sandbox(root, live=isinstance(driver, LiveDriver), env=_arm_env(config)):
            for task in tasks:
                result = asyncio.run(run_task(task, driver, workdir=root / "repos", timeout=timeout, config=config))
                results.append(result)
                if on_result is not None:
                    on_result(result)
    run = RunResult(run_id=run_id, started=started, driver=driver.name, config=config or {}, results=results, aggregate=aggregate(results))
    if out_dir is not None:
        out_dir.mkdir(parents=True, exist_ok=True)
        (out_dir / f"{run_id}.json").write_text(json.dumps(run.to_json(), indent=2), encoding="utf-8")
    return run


def compare(a: dict[str, Any], b: dict[str, Any]) -> dict[str, Any]:
    """What changed between two saved runs: headline numbers, and the tasks that flipped."""
    ra = {r["id"]: r for r in a["results"]}
    rb = {r["id"]: r for r in b["results"]}
    common = sorted(set(ra) & set(rb))
    return {
        "a": a["run_id"],
        "b": b["run_id"],
        "tasks_compared": len(common),
        "fixed": [i for i in common if not ra[i]["passed"] and rb[i]["passed"]],
        "broken": [i for i in common if ra[i]["passed"] and not rb[i]["passed"]],
        "pass_rate": {s: (a["aggregate"][s].get("pass_rate"), b["aggregate"][s].get("pass_rate")) for s in ("dev", "heldout", "all")},
        "mean_reward": {s: (a["aggregate"][s].get("mean_reward"), b["aggregate"][s].get("mean_reward")) for s in ("dev", "heldout", "all")},
    }
