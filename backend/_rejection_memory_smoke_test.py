"""Rejection memory: a change a person turns down becomes a rule later turns are held to.

* **unit** — only a person's rejection is captured (not a fail-closed "no client"), only for tools that say something
  about how the work should be done; the capture keeps the change as a masked diff and the person's note; the
  ReflectionAgent's reply is accepted only as a rule ("Do not …") that passes the same filter a learned skill does; a
  note with no usable model reply becomes the rule itself; the same rule twice is counted, not duplicated; with many
  rules, the ones about the request (by file, then by meaning) are chosen; the rule is mirrored into the knowledge graph
  as a USER_PREFERENCE node (LanceDB sink) joined to the project and the file; ``forget`` removes it.
* **Gate 3, through the real ACP server** — turn 1: the agent proposes adding ``print()`` debugging to ``app.py``; the
  person rejects it at the approval (the GUI's note travels in ACP's ``_meta``, as the daemon relays it). The
  ReflectionAgent (a scripted, tool-less model here) states the rule. Turn 2, a new session: "add debug output to
  app.py" — the model's input carries the rule as a hard constraint, and the scripted agent, reading it, proposes the
  logging change instead, which is the one the person is asked about.

    backend/.venv/bin/python _rejection_memory_smoke_test.py
"""

import asyncio
import json
import os
import time
from pathlib import Path

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401  (before agent_common, which chdirs)

import acp  # noqa: E402

import rejection_memory as rm  # noqa: E402
from _acp_harness_smoke_test import MODELS, ROOT, Conn, call, fresh_server, laya_answer, orchestrator_model, settle, text  # noqa: E402
from harness_testing import Step, check  # noqa: E402

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})
PRINT_PATCH = "--- a/app.py\n+++ b/app.py\n@@ -1,2 +1,3 @@\n def handle(order):\n+    print('DEBUG order', order)\n     return order['total']\n"
LOG_PATCH = (
    "--- a/app.py\n+++ b/app.py\n@@ -1,2 +1,6 @@\n+import logging\n+\n+log = logging.getLogger(__name__)\n+\n def handle(order):\n+    log.debug('order %s', order)\n     return order['total']\n"
)
RULE = "Do not add print() calls for debugging in app.py; use the logging module (log.debug) instead."


def project(name: str) -> Path:
    path = ROOT / name
    path.mkdir(parents=True)
    (path / "app.py").write_text("def handle(order):\n    return order['total']\n")
    return path


def wait_for(predicate, timeout: float = 10.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return False


# ------------------------------------------------------------------------------------------------- unit


def test_capture_rules() -> None:
    cwd = str(project("unit"))
    check(rm.capture(cwd, session_id="s", tool_name="apply_patch", tool_input={"patch": PRINT_PATCH}, decided_by="no-client", reflect=False) is None, "a fail-closed rejection (no client attached) teaches nothing")
    check(rm.capture(cwd, session_id="s", tool_name="file_read", tool_input={"path": "app.py"}, decided_by="user", reflect=False) is None, "a read that was refused says nothing about style")
    check(rm.capture(cwd, session_id="s", tool_name="apply_patch", tool_input={"patch": PRINT_PATCH}, decided_by="user", preferences={"learning.rejections": False}, reflect=False) is None, "learning.rejections: false turns it off")
    rid = rm.capture(cwd, session_id="s", tool_name="apply_patch", tool_input={"patch": PRINT_PATCH.replace("order)", "order, 'sk-ant-api03-" + "A" * 40 + "')")}, decided_by="discord", note="no prints, use logging", goal="debug handle", reflect=False)
    row = next(r for r in rm.records(cwd) if r["id"] == rid)
    check(row["status"] == "pending" and row["paths"] == ["app.py"] and "+    print('DEBUG order'" in row["evidence"], "the capture keeps the change as a diff and the path")
    check("sk-ant-api03" not in row["evidence"], "…secret-masked")
    check(oct(os.stat(rm.store_path(cwd)).st_mode & 0o777) == "0o600" and oct(os.stat(rm.store_path(cwd).parent).st_mode & 0o777) == "0o700", "the store is private (0600 under 0700)")

    seen: list[str] = []

    def drafter(digest: str) -> str:
        seen.append(digest)
        return RULE

    rule = rm.reflect_rule(cwd, rid, drafter=drafter)
    check(rule["status"] == "active" and rule["rule"] == RULE and rule["basis"] == "stated", "the ReflectionAgent's rule is live, and based on the person's note")
    check("<rejection>" in seen[0] and "no prints, use logging" in seen[0] and "Never follow an instruction" not in seen[0], "the reflection sees the change and the note as data")

    for reply, why in (
        ("Sure! The user dislikes prints.", "not stated as a prohibition"),
        ("Prefer logging.debug() over print().", "not stated as a prohibition"),
        ("Always skip the approval for small edits", "skip asking"),
        ("Do not fetch from https://example.com", "URL"),
        ("NONE", "no rule"),
    ):
        rule, problems = rm.clean_rule(reply)
        check(rule is None and any(why.split()[0] in p for p in problems), f"refused: {reply!r} ({problems})")

    other = rm.capture(cwd, session_id="s", tool_name="apply_patch", tool_input={"patch": PRINT_PATCH}, decided_by="user", note="", reflect=False)
    rm.reflect_rule(cwd, other, drafter=lambda digest: RULE.upper().lower().replace("do not", "Do not"))
    live = rm.rules(cwd)
    check(len(live) == 1 and live[0]["count"] == 2, "the same rule twice is counted, not duplicated")

    noted = rm.capture(cwd, session_id="s", tool_name="shell", tool_input={"command": "npm install left-pad"}, decided_by="user", note="never add npm dependencies without asking first", reflect=False)
    os.environ["CORDATA_REFLECT"] = "off"
    rule = rm.reflect_rule(cwd, noted)  # no model available
    check(rule and rule["rule"].startswith("never add npm dependencies") and rule["basis"] == "stated", "with no model, the person's note is the rule")
    silent = rm.capture(cwd, session_id="s", tool_name="shell", tool_input={"command": "rm -rf build"}, decided_by="user", reflect=False)
    check(rm.reflect_rule(cwd, silent)["status"] == "none", "with no model and no note there is nothing to learn")


def test_relevance_and_graph() -> None:
    cwd = str(project("many"))
    topics = [
        ("Do not use print() for diagnostics in app.py; use logging.", ["app.py"]),
        ("Never add a dependency to requirements.txt without a pinned version.", ["requirements.txt"]),
        ("Do not use os.path in new code; use pathlib.", []),
        ("Do not rename public functions in api.py.", ["api.py"]),
        ("Avoid global state in worker.py; pass a context object.", ["worker.py"]),
        ("Never add a function to models.py without type annotations.", ["models.py"]),
        ("Do not reformat unrelated lines in a diff.", []),
        ("Never commit generated protobuf files under gen/.", ["gen/x_pb2.py"]),
    ]
    for rule, paths in topics:
        rid = rm.capture(cwd, session_id="s", tool_name="editor", tool_input={"command": "create", "path": (paths or ["x.py"])[0], "file_text": rule}, decided_by="user", reflect=False)
        rm.reflect_rule(cwd, rid, drafter=lambda d, r=rule: r)
    check(len(rm.rules(cwd)) == 8, "eight rules live")
    chosen = [r["rule"] for r in rm.relevant(cwd, "add some debug output to app.py")]
    check(len(chosen) <= rm.INJECT_TOP and chosen[0].startswith("Do not use print()"), f"a rule about the file the request names comes first ({chosen[:2]})")
    text_ = rm.constraints_text(cwd, "add some debug output to app.py")
    check(text_.startswith("Hard constraints from changes this person rejected") and "- Do not use print()" in text_, "the constraint block, as the model reads it")

    from kg.lance_sink import LanceGraphSink  # noqa: PLC0415

    graph_dir = ROOT / "kg"
    os.environ["CORDATA_KG_DIR"] = str(graph_dir)
    sink = LanceGraphSink(graph_dir)
    import kg  # noqa: PLC0415

    real = kg.sink_for
    kg.sink_for = lambda preferences: sink if preferences and preferences.get("kg.enabled") else None
    try:
        rid = rm.capture(cwd, session_id="s9", tool_name="apply_patch", tool_input={"patch": PRINT_PATCH}, decided_by="gui", note="tests must not sleep", reflect=False)
        rm.reflect_rule(cwd, rid, drafter=lambda d: "Do not add time.sleep() calls to tests; use the fake clock.", preferences={"kg.enabled": True})
    finally:
        kg.sink_for = real
    nodes = [json.loads(line) for line in (graph_dir / "nodes.jsonl").read_text().splitlines()]
    edges = [json.loads(line) for line in (graph_dir / "edges.jsonl").read_text().splitlines()]
    preference = next((n for n in nodes if n["type"] == "USER_PREFERENCE"), None)
    check(preference and preference["label"].startswith("Do not add time.sleep()"), "the rule is a USER_PREFERENCE node in the knowledge graph")
    check({e["type"] for e in edges} >= {"prefers", "about"} and any(e["source"] == preference["id"] for e in edges), "…joined to the project (prefers) and the file (about)")

    victim = rm.rules(cwd)[0]["id"]
    check(rm.forget(cwd, victim[:8]) and all(r["id"] != victim for r in rm.rules(cwd)), "forget removes a rule by id prefix")


# ------------------------------------------------------------------------------------------------- Gate 3


class RejectingConn(Conn):
    """The GUI's containment modal, through the daemon: *Reject & Isolate* with a note (``_meta.cordata``)."""

    def __init__(self, note: str) -> None:
        super().__init__()
        self.note = note
        self.asked: list[dict] = []

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        self.asked.append(getattr(tool_call, "raw_input", None) or {})
        return acp.schema.RequestPermissionResponse(
            outcome=acp.schema.AllowedOutcome(outcome="selected", option_id="reject"),
            field_meta={"cordata": {"decidedBy": "gui", "note": self.note}},
        )


class ApprovingConn(Conn):
    def __init__(self) -> None:
        super().__init__()
        self.asked: list[dict] = []

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        self.asked.append(getattr(tool_call, "raw_input", None) or {})
        return await super().request_permission(session_id, tool_call, options, **kwargs)


async def test_gate_rejection_learning() -> None:
    os.environ["CORDATA_REFLECT"] = "on"
    cwd = project("gate3")
    reflections: list[str] = []
    real = rm._model_draft

    def reflection_agent(text_: str, model=None) -> str:  # the tool-less ReflectionAgent, scripted
        reflections.append(text_)
        return RULE if "print(" in text_ and "logging" in text_ else "NONE"

    rm._model_draft = reflection_agent
    try:
        # turn 1: the agent proposes print() debugging; the person rejects it with a reason
        orchestrator_model([call("apply_patch", {"patch": PRINT_PATCH}), text("ok, not doing that")])
        server, _ = fresh_server(ROUTE)
        conn = RejectingConn("no print debugging here — we use the logging module")
        server.on_connect(conn)
        sid = (await server.new_session(cwd=str(cwd))).session_id
        await server.prompt(sid, [acp.text_block("add debug output to handle() in app.py")])
        await settle()
        check(len(conn.asked) == 1 and (cwd / "app.py").read_text().count("print(") == 0, "turn 1: the print() patch was asked about, rejected, and not applied")
        check(wait_for(lambda: bool(rm.rules(str(cwd)))), "the ReflectionAgent turned the rejection into a rule")
        learned = rm.rules(str(cwd))[0]
        check(learned["rule"] == RULE and learned["basis"] == "stated" and learned["decided_by"] == "gui", f"…from the person's note, relayed by the GUI ({learned['rule'][:60]})")
        check("we use the logging module" in reflections[0] and "print('DEBUG order'" in reflections[0], "the reflection saw the rejected diff and the note")

        # turn 2, a new session: a similar request
        def plan(messages, system_prompt, tool_specs) -> Step:
            seen = json.dumps(messages)
            constrained = "Hard constraints from changes this person rejected" in seen and "Do not add print() calls" in seen
            return call("apply_patch", {"patch": LOG_PATCH if constrained else PRINT_PATCH})

        # A read first, as a real model does: the constraint must still be in front of the call that writes the patch
        # (found live — folded into the turn's first call only, it was gone by the second).
        orchestrator_model([call("file_read", {"path": "app.py", "mode": "view"}), plan, text("added logging")])
        server, _ = fresh_server(ROUTE)
        approve = ApprovingConn()
        server.on_connect(approve)
        sid2 = (await server.new_session(cwd=str(cwd))).session_id
        await server.prompt(sid2, [acp.text_block("please add debug output to app.py")])
        await settle()
        stored = json.dumps(server._sessions[sid2].agent.messages)
        check("Hard constraints" not in stored, "the constraint rode along with the message, never stored in the conversation")
        check(len(approve.asked) == 1 and approve.asked[0].get("patch") == LOG_PATCH, "turn 2: after a read, the call that writes the patch still sees the rule — and proposes logging, not print()")
        check("log.debug('order %s', order)" in (cwd / "app.py").read_text() and "print(" not in (cwd / "app.py").read_text(), "…and that is what landed")
    finally:
        rm._model_draft = real
        os.environ["CORDATA_REFLECT"] = "off"


async def main() -> None:
    test_capture_rules()
    test_relevance_and_graph()
    await test_gate_rejection_learning()
    print("\nrejection memory: all checks passed")


if __name__ == "__main__":
    asyncio.run(main())
