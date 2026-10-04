"""Regression check for the observer loop (daemon/observer.py + observer_runner.py).

Two things this pins down, both of which are the sort of property that quietly
stops being true:

1. **The observer cannot write.** Its toolset is asserted to be exactly
   `OBSERVER_TOOLS`, disjoint from `sub_agent_common`'s denylists; the
   allowlist hook is shown cancelling a call outside it; and a real cycle over a
   real (temporary) git repository is shown to leave that repository
   byte-identical — no file created, changed or removed, `.git/index` included.
2. **Preemption kills.** A user attaching mid-cycle abandons it: no briefing is
   written, and the child process that was doing the observation is *dead*, not
   politely asked to stop.

Hermetic: the observer's agent is replaced by a stub script (a real model call
would need a provider), the ledger is a temp `SIERRALAB_STATE_DB`, and the
briefing lands in a temp file. Nothing here touches the developer's own
`~/.sierralab/`.

    backend/.venv/bin/python _observer_smoke_test.py
"""

import asyncio
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace

_TMP = tempfile.TemporaryDirectory()
_TMPDIR = Path(_TMP.name)
os.environ["SIERRALAB_STATE_DB"] = str(_TMPDIR / "state.db")
os.environ["SIERRALAB_OBSERVER_BRIEFING"] = str(_TMPDIR / "nightly_briefing.md")
os.environ["SIERRALAB_OBSERVER_QUIET"] = ""
# Must be set before observer_runner imports sub_agent_common (no chdir either
# way, but keeping the child's imports deterministic matters for a smoke test).
sys.path.insert(0, str(Path(__file__).resolve().parent))

import daemon.observer as observer_mod  # noqa: E402
from daemon import observer_runner  # noqa: E402
import state_store  # noqa: E402
import sub_agent_common as sac  # noqa: E402


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


class Prefs:
    """`settings_store.Settings` is a validated dict; this is the dict part."""

    def __init__(self, **values) -> None:
        self.values = values

    def get(self, key, default=None):
        return self.values.get(key, default)


ON = Prefs(**{"observer.enabled": True, "observer.intervalMinutes": 30.0})
OFF = Prefs()


def make_repo(root: Path) -> Path:
    """A tiny real git repository, committed, with one uncommitted change."""
    root.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"}
    for argv in (
        ["git", "init", "-q"],
        ["git", "add", "-A"],
        ["git", "commit", "-q", "-m", "first", "--allow-empty"],
    ):
        subprocess.run(argv, cwd=root, env=env, check=True, capture_output=True)
    (root / "README.md").write_text("# project\n", encoding="utf-8")
    (root / "work.py").write_text("print('hello')\n", encoding="utf-8")
    subprocess.run(["git", "add", "README.md", "work.py"], cwd=root, env=env, check=True, capture_output=True)
    subprocess.run(["git", "commit", "-q", "-m", "add files"], cwd=root, env=env, check=True, capture_output=True)
    (root / "work.py").write_text("print('hello there')\n", encoding="utf-8")  # uncommitted
    return root


def tree_digest(root: Path) -> dict[str, str]:
    """Every file under `root` and its content hash — including `.git`."""
    out: dict[str, str] = {}
    for path in sorted(root.rglob("*")):
        if path.is_file():
            out[str(path.relative_to(root))] = hashlib.sha256(path.read_bytes()).hexdigest()
    return out


STUB = '''\
import json, os, sys, time
request = json.loads(sys.stdin.readline() or "{}")
if os.environ.get("OBSERVER_STUB_PIDFILE"):
    open(os.environ["OBSERVER_STUB_PIDFILE"], "w").write(str(os.getpid()))
    time.sleep(float(os.environ.get("OBSERVER_STUB_SLEEP", "30")))
print(json.dumps({
    "summary": "work.py has an uncommitted edit; the tree is otherwise clean.",
    "proposals": [{"title": "Finish the work.py edit", "why": "it is uncommitted", "next": "run it"}],
    "project": request.get("project"),
}))
'''


def write_stub(name: str = "stub.py") -> Path:
    path = _TMPDIR / name
    path.write_text(STUB, encoding="utf-8")
    return path


def observer_for(
    repo: Path,
    stub: Path,
    *,
    settings=ON,
    busy=lambda: False,
    logs=(),
    briefing: Path | None = None,
) -> observer_mod.Observer:
    return observer_mod.Observer(
        projects=lambda: [str(repo)],
        busy=busy,
        settings=lambda: settings,
        briefing_path=briefing or (_TMPDIR / "nightly_briefing.md"),
        runner=lambda: [sys.executable, str(stub)],
        logs=lambda: list(logs),
    )


# ------------------------------------------------------------------ toolset


def test_the_observer_cannot_write() -> None:
    names = set(observer_runner.OBSERVER_TOOLS)
    check(names == {"file_read", "think"}, f"the allowlist is exactly {sorted(names)}")
    check(
        not names & (sac.DENIED_TOOLS | sac.ORCHESTRATOR_ONLY_TOOLS),
        "it is disjoint from sub_agent_common's denied and orchestrator-only toolsets",
    )
    for forbidden in ("shell", "python_repl", "file_write", "editor", "apply_patch", "http_request"):
        check(forbidden not in names, f"the observer does not hold {forbidden}")

    # The real agent, built with a model that is never called: what matters is
    # which tools reached its registry, not what it says.
    from strands.models.model import Model

    class NoModel(Model):
        def update_config(self, **kw):
            pass

        def get_config(self):
            return {}

        async def structured_output(self, *a, **k):
            raise NotImplementedError
            yield  # pragma: no cover

        async def stream(self, *a, **k):
            raise NotImplementedError
            yield  # pragma: no cover

    agent = observer_runner.build_observer_agent(model=NoModel())
    registry = set(getattr(agent.tool_registry, "registry", {}) or {})
    check(registry == names, f"the built agent holds exactly the allowlist (got {sorted(registry)})")

    # Belt and braces: a call outside the allowlist is cancelled even if a tool
    # reached the registry some other way.
    hook = observer_runner._AllowlistOnly(names)
    blocked = SimpleNamespace(tool_use={"name": "shell", "input": {"command": "ls"}})
    hook._check(blocked)
    check(bool(getattr(blocked, "cancel_tool", None)), "a shell call is cancelled by the allowlist hook")
    allowed = SimpleNamespace(tool_use={"name": "file_read", "input": {"path": "README.md"}})
    hook._check(allowed)
    check(getattr(allowed, "cancel_tool", None) is None, "a file_read call is not")


def test_parse_answer() -> None:
    answer = observer_runner.parse_answer('Sure:\n{"summary": "s", "proposals": [{"title": "t", "why": "w", "next": "n"}]}\n')
    check(answer["summary"] == "s" and answer["proposals"][0]["next"] == "n", "JSON in surrounding prose is parsed")
    prose = observer_runner.parse_answer("no json at all")
    check(prose["summary"] == "no json at all" and prose["proposals"] == [], "prose still yields a summary")
    partial = observer_runner.parse_answer('{"summary": "s", "proposals": ["nope", {"why": "no title"}, {"title": "ok"}]}')
    check([p["title"] for p in partial["proposals"]] == ["ok"], "proposals without a title are dropped")


# ------------------------------------------------------------------ evidence


def test_evidence_is_read_only() -> None:
    repo = make_repo(_TMPDIR / "repo")
    before = tree_digest(repo)
    evidence = observer_mod.git_evidence(str(repo))
    check("work.py" in evidence["status"], "git status sees the uncommitted edit")
    check("add files" in evidence["log"], "the log lists the commits")
    check("work.py" in evidence["diff-stat"], "the diff stat names the changed file")
    check(tree_digest(repo) == before, "reading the repository changed nothing — .git/index included")

    empty = _TMPDIR / "not-a-repo"
    empty.mkdir()
    (empty / "x.txt").write_text("x", encoding="utf-8")
    missing = observer_mod.git_evidence(str(empty))
    check("not a git repository" in missing["status"] or "exit 128" in missing["status"], "a non-repository is recorded, not raised")
    check(tree_digest(empty) == {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in [empty / "x.txt"]}, "and nothing was created in it")


def test_logs_and_briefing_render() -> None:
    log = _TMPDIR / "app.log"
    log.write_text("\n".join(f"line {i}" for i in range(100)) + "\n", encoding="utf-8")
    tails = observer_mod.log_evidence([log, _TMPDIR / "absent.log"])
    check("line 99" in tails[str(log)] and "line 0" not in tails[str(log)], "only the tail of a log is read")
    check("unreadable" in tails[str(_TMPDIR / "absent.log")], "a missing log is recorded as unreadable")

    empty = observer_mod.render_briefing([], generated_at=time.time())
    check("Nothing to report" in empty, "an empty cycle renders a briefing that says so")
    full = observer_mod.render_briefing(
        [
            {
                "project": "/tmp/project",
                "summary": "a summary",
                "proposals": [{"title": "do a thing", "why": "because", "next": "start"}],
                "evidence": {"git": {"status": " M work.py", "log": "abc first", "diff": "diff --git a/work.py", "diff-stat": "work.py | 2 +-"}},
            }
        ],
        generated_at=time.time(),
    )
    check("do a thing" in full and "a summary" in full and "/tmp/project" in full, "the briefing carries projects, summaries and proposals")
    check("read-only tools" in full, "the briefing says who wrote it and what it could not do")


def test_projects_come_from_the_session_index() -> None:
    index = _TMPDIR / "index.json"
    index.write_text(
        json.dumps(
            {
                "a": {"cwd": str(_TMPDIR / "repo"), "updatedAt": "2026-01-01T00:00:00Z"},
                "b": {"cwd": str(_TMPDIR / "gone"), "updatedAt": "2026-06-01T00:00:00Z"},
                "c": {"cwd": str(_TMPDIR / "repo"), "updatedAt": "2026-07-01T00:00:00Z"},
            }
        ),
        encoding="utf-8",
    )
    projects = observer_mod.known_projects(index)
    check(projects == [str(_TMPDIR / "repo")], f"live, de-duplicated project directories (got {projects})")


def test_quiet_hours() -> None:
    window = observer_mod.parse_quiet("22:00-07:00")
    check(window == (1320, 420), "a wrapping window parses")
    check(observer_mod.parse_quiet("") is None, "no window means no quiet hours")
    check(observer_mod.parse_quiet("whenever") is None, "an unparseable window is None, not a wrong window")
    midnight = time.mktime((2026, 1, 2, 23, 30, 0, 0, 0, -1))
    midday = time.mktime((2026, 1, 2, 12, 0, 0, 0, 0, -1))
    check(observer_mod.in_quiet_hours(midnight, window) is True, "23:30 is inside 22:00-07:00")
    check(observer_mod.in_quiet_hours(midday, window) is False, "noon is not")


# ------------------------------------------------------------------- the loop


def test_a_cycle_writes_the_briefing_and_stages_proposals() -> None:
    repo = make_repo(_TMPDIR / "repo2")
    stub = write_stub()
    briefing = _TMPDIR / "nightly_briefing.md"
    briefing.unlink(missing_ok=True)
    observer = observer_for(repo, stub, briefing=briefing)

    result = asyncio.run(observer.cycle())

    check(result is not None and result["observed"] == 1, "the cycle observed one project")
    text = briefing.read_text(encoding="utf-8")
    check("uncommitted edit" in text, "the model's summary reached the briefing")
    check("Finish the work.py edit" in text, "its proposal did too")
    check(str(repo) in text, "and the project it belongs to")

    rows = state_store.connect().execute("SELECT kind, status, session_id, note FROM interventions").fetchall()
    check(len(rows) == 1 and rows[0]["kind"] == "observation", "the proposal is staged as an observation")
    check(rows[0]["status"] == "proposed", "with status 'proposed', not 'staged'")
    check(rows[0]["session_id"] == observer_mod.OBSERVER_SESSION, "under the observer's ledger id")
    check(state_store.pending_interventions("observer") == [], "it is NOT a pending approval")
    check(state_store.pending_interventions() == [], "and does not show up in the daemon-wide one either")
    events = state_store.recent_events(kind="observation")
    check(len(events) == 1, "an observation event was appended")


def test_a_busy_daemon_is_not_observed() -> None:
    repo = make_repo(_TMPDIR / "repo3")
    briefing = _TMPDIR / "busy_briefing.md"
    briefing.unlink(missing_ok=True)
    observer = observer_mod.Observer(
        projects=lambda: [str(repo)],
        busy=lambda: True,
        settings=lambda: ON,
        briefing_path=briefing,
        runner=lambda: [sys.executable, str(write_stub("never.py"))],
        logs=lambda: [],
    )
    check(asyncio.run(observer.cycle()) is None, "a cycle with a client attached does nothing")
    check(not briefing.exists(), "and writes no briefing")


def test_off_is_off() -> None:
    repo = make_repo(_TMPDIR / "repo4")
    briefing = _TMPDIR / "off_briefing.md"
    briefing.unlink(missing_ok=True)
    observer = observer_for(repo, write_stub("off.py"), settings=OFF, briefing=briefing)
    check(observer.enabled() is False, "observer.enabled default is off")
    check(asyncio.run(observer.cycle()) is None, "an observer that is off never cycles")
    check(not briefing.exists(), "and writes nothing")


def test_preemption_kills_the_child() -> None:
    """The load-bearing one: a cycle in flight while a user attaches is
    abandoned — and the process running the observation is dead."""
    repo = make_repo(_TMPDIR / "repo5")
    stub = write_stub("slow.py")
    pidfile = _TMPDIR / "child.pid"
    pidfile.unlink(missing_ok=True)
    briefing = _TMPDIR / "preempt_briefing.md"
    briefing.unlink(missing_ok=True)
    os.environ["OBSERVER_STUB_PIDFILE"] = str(pidfile)
    os.environ["OBSERVER_STUB_SLEEP"] = "30"
    observer = observer_mod.Observer(
        projects=lambda: [str(repo)],
        busy=lambda: False,
        settings=lambda: ON,
        briefing_path=briefing,
        runner=lambda: [sys.executable, str(stub)],
        logs=lambda: [],
    )

    async def run() -> dict | None:
        cycle = asyncio.ensure_future(observer.cycle())
        for _ in range(100):  # wait for the child to be up
            if pidfile.exists():
                break
            await asyncio.sleep(0.05)
        observer.wake("a client attached")
        return await asyncio.wait_for(cycle, timeout=15)

    result = asyncio.run(run())
    check(result is None, "the preempted cycle returns nothing")
    check("preempted" in (observer._last.get("note") or ""), f"and says why ({observer._last.get('note')!r})")
    check(not briefing.exists(), "no briefing was written")

    pid = int(pidfile.read_text().strip())
    time.sleep(0.5)
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        check(True, "the observation child was killed, not left running")
    else:
        os.kill(pid, 9)
        check(False, f"the observation child (pid {pid}) survived preemption")


def test_a_preempted_cycle_does_not_lose_the_queue() -> None:
    """`/observe` answers "queued" — so a cycle that is abandoned after it took
    the queue has to hand the observations back, not drop them."""
    repo = make_repo(_TMPDIR / "repo7")
    stub = write_stub("requeue.py")
    pidfile = _TMPDIR / "requeue.pid"
    pidfile.unlink(missing_ok=True)
    os.environ["OBSERVER_STUB_PIDFILE"] = str(pidfile)
    os.environ["OBSERVER_STUB_SLEEP"] = "30"
    observer = observer_mod.Observer(
        projects=lambda: [str(repo)],
        busy=lambda: False,
        settings=lambda: ON,
        briefing_path=_TMPDIR / "requeue_briefing.md",
        runner=lambda: [sys.executable, str(stub)],
        logs=lambda: [],
    )
    observer.observe("the deploy failed", source="ci")

    async def run() -> None:
        cycle = asyncio.ensure_future(observer.cycle())
        for _ in range(100):
            if pidfile.exists():
                break
            await asyncio.sleep(0.05)
        observer.observe("arrived while the model was thinking", source="ci")
        observer.wake("a client attached")
        await asyncio.wait_for(cycle, timeout=15)

    asyncio.run(run())
    held = [item["text"] for item in observer._inbox]
    check(
        held == ["the deploy failed", "arrived while the model was thinking"],
        f"an abandoned cycle returns what it took, ahead of what arrived since (got {held})",
    )


def test_evidence_does_not_run_the_repos_own_config() -> None:
    """git executes programs a repository's config names. The observer runs git
    unattended, so `core.fsmonitor` and `diff.external` must not fire."""
    repo = make_repo(_TMPDIR / "repo8")
    marker = _TMPDIR / "ran-from-git-config"
    marker.unlink(missing_ok=True)
    hook = _TMPDIR / "hook.sh"
    hook.write_text(f"#!/bin/sh\ntouch {marker}\n", encoding="utf-8")
    hook.chmod(0o755)
    for key in ("core.fsmonitor", "diff.external"):
        subprocess.run(["git", "config", key, str(hook)], cwd=repo, check=True, capture_output=True)
    (repo / "work.py").write_text("print('changed again')\n", encoding="utf-8")
    evidence = observer_mod.git_evidence(str(repo))
    check(bool(evidence.get("diff")), "the evidence is still collected")
    check(not marker.exists(), "a program named in the repository's git config was not executed")


def test_briefing_is_private_and_fences_hold() -> None:
    repo = make_repo(_TMPDIR / "repo9")
    briefing = _TMPDIR / "private_briefing.md"
    briefing.unlink(missing_ok=True)
    briefing.with_suffix(".tmp").unlink(missing_ok=True)
    observer = observer_for(repo, write_stub("private.py"), briefing=briefing)
    asyncio.run(observer.cycle())
    mode = briefing.stat().st_mode & 0o777
    check(mode == 0o600, f"the briefing is 0600 (got {oct(mode)})")

    # A diff of a markdown file carries triple backticks of its own.
    text = observer_mod.render_briefing(
        [
            {
                "project": "p",
                "summary": "s",
                "proposals": [],
                "evidence": {"git": {"diff": "+```python\n+print(1)\n+```", "status": " M README.md"}},
            }
        ],
        generated_at=time.time(),
    )
    fence = next(line for line in text.splitlines() if line.startswith("`") and line.endswith("text"))
    check(len(fence) - len("text") >= 4, f"the fence is longer than any backtick run in the evidence (got {fence!r})")
    check(text.count(fence[: -len("text")]) == 2, "and it opens and closes exactly once, so the diff cannot escape it")


def test_webhook_inbox_is_a_source() -> None:
    repo = make_repo(_TMPDIR / "repo6")
    briefing = _TMPDIR / "inbox_briefing.md"
    briefing.unlink(missing_ok=True)
    observer = observer_for(repo, write_stub("inbox.py"), briefing=briefing)
    depth = observer.observe("the nightly deploy failed", source="ci")
    check(depth == 1, "the queue reports its depth")
    asyncio.run(observer.cycle())
    text = briefing.read_text(encoding="utf-8")
    check("the nightly deploy failed" in text, "a queued observation is evidence for the next cycle")
    check(observer.observe("x") == 1, "the queue is drained by the cycle that read it")


def main() -> None:
    test_the_observer_cannot_write()
    test_parse_answer()
    test_evidence_is_read_only()
    test_logs_and_briefing_render()
    test_projects_come_from_the_session_index()
    test_quiet_hours()
    test_a_cycle_writes_the_briefing_and_stages_proposals()
    test_a_busy_daemon_is_not_observed()
    test_off_is_off()
    test_preemption_kills_the_child()
    test_a_preempted_cycle_does_not_lose_the_queue()
    test_evidence_does_not_run_the_repos_own_config()
    test_briefing_is_private_and_fences_hold()
    test_webhook_inbox_is_a_source()
    print("\nobserver: all checks passed")


if __name__ == "__main__":
    main()
