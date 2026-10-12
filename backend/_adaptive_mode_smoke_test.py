"""Adaptive mode and the plan card.

* **advisor (unit)** — what starts a turn in plan (an explicit "plan this", a design question, a large cross-cutting
  change, Laya's architect route), what does not (short messages, continuations, a plain question, slash commands and
  ``!shell``), and that advice only ever tightens.
* **edit mode** — file edits run without asking (and stay on the auto-approval trail), commands still ask; an approved
  plan can land there ("approve → edit").
* **ask_user** — a multiple-choice question reaches the person as a card whose options are the choices; the pick, the
  person's own words or "decide for me" come back as the tool's result; it works in plan mode and changes no mode.
* **through the real ACP server** — a build-mode session asked to "make a plan" starts the turn in plan mode and says
  why (``cordata/mode`` + ACP's own ``current_mode_update``); the agent's ``present_plan`` reaches the person as a card
  with four answers; *approve → build* switches the session and the tool tells the model; *approve → auto* switches
  to auto; *keep planning* leaves plan mode on, hands the person's note back to the agent and teaches the rejection
  memory nothing; a mode the person picked by hand holds adaptive mode off; ``modes.adaptive: false`` switches it
  off; and ``present_plan`` outside plan mode is refused, so it can never change a mode on the agent's say-so.

    backend/.venv/bin/python _adaptive_mode_smoke_test.py
"""

import asyncio
import json
import os
from pathlib import Path

os.environ["CORDATA_PRISM"] = "off"

import provider_settings  # noqa: F401  (before agent_common, which chdirs)

import acp  # noqa: E402

import mode_advisor as ma  # noqa: E402
import plan_mode  # noqa: E402
from _acp_harness_smoke_test import MODELS, ROOT, Conn, call, fresh_server, laya_answer, orchestrator_model, settle, text  # noqa: E402
from harness_testing import check  # noqa: E402

ROUTE = laya_answer("code", {"code": 0.97, "answer": 0.03})


def test_advisor() -> None:
    plan = lambda msg, **kw: (ma.advise(msg, mode=kw.pop("mode", "build"), **kw) or ma.Advice("none", "", 0)).mode  # noqa: E731
    check(plan("Make a plan for moving the settings module to pydantic") == "plan", "'make a plan' starts in plan")
    check(plan("don't change anything yet, just tell me how you'd approach the cache bug") == "plan", "'don't change anything yet' starts in plan")
    check(plan("how should we structure the plugin loader?") == "plan", "a design question starts in plan")
    check(plan("refactor every module across the repo to use the new logger") == "plan", "a large cross-cutting change starts in plan")
    check(plan("fix the typo in the readme") == "none", "a small edit does not")
    check(plan("rename foo to bar in utils.py") == "none", "a one-file rename does not")
    check(plan("yes") == "none" and plan("go ahead") == "none" and plan("continue.") == "none", "a continuation never changes the mode")
    check(plan("/plan something") == "none" and plan("!ls") == "none", "a slash command or a !shell line is left alone")
    check(plan("what does the router do?") == "none", "a plain question does not")
    check(plan("make a plan for X", mode="plan") == "none", "already in plan: nothing to do")
    check(plan("please update the docs and the examples to match the new behaviour, then go through the tests and tidy the fixtures, and also make sure the changelog and the release notes describe what changed and why it changed", route="architect") == "plan", "Laya's architect route plus a long message starts in plan")
    check(plan("fix it", route="architect") == "none", "…but a short message on that route does not")
    check(ma.should_apply(ma.advise("make a plan for X", mode="auto"), mode="auto", held=0), "auto is tightened to plan")
    check(not ma.should_apply(ma.advise("make a plan for X", mode="build"), mode="build", held=3), "a recent manual choice holds it off")
    check(all(ma.advise(m, mode="plan") is None for m in ("make a plan", "go ahead")), "nothing here can ever move toward auto: the only advice is 'plan'")


class AnsweringConn(Conn):
    """Answers the permission requests the way a person at the plan card would."""

    def __init__(self) -> None:
        super().__init__()
        self.answer = plan_mode.OPTION_BUILD
        self.note = ""
        self.asked: list[tuple[str, list[str], dict]] = []

    async def request_permission(self, session_id, tool_call, options, **kwargs):
        self.asked.append((tool_call.title, [o.option_id for o in options], getattr(tool_call, "raw_input", None) or {}))
        meta = {"cordata": {"note": self.note}} if self.note else None
        return acp.schema.RequestPermissionResponse(outcome=acp.schema.AllowedOutcome(outcome="selected", option_id=self.answer), field_meta=meta)


def user_settings(values: dict) -> None:
    home = Path(os.environ["CORDATA_HOME"])
    home.mkdir(parents=True, exist_ok=True)
    (home / "settings.json").write_text(json.dumps(values))


_N = 0


async def session(steps, *, mode="build"):
    global _N
    _N += 1
    cwd = ROOT / f"proj{_N}"
    cwd.mkdir()
    (cwd / "README.md").write_text("demo\n")
    orchestrator_model(steps)
    server, _ = fresh_server(ROUTE)
    conn = AnsweringConn()
    server.on_connect(conn)
    sid = (await server.new_session(cwd=str(cwd))).session_id
    if mode != "build":
        await server.set_session_mode(sid, mode)
        server._sessions[sid].adaptive_hold = 0  # the test set it, not a person
    return server, conn, sid


def mode_of(server, sid) -> str:
    return server._sessions[sid].handler.mode


def mode_events(conn) -> list[dict]:
    return [p for m, p in conn.ext if m == "cordata/mode"]


PLAN = "# Goal: move settings to pydantic\n1. add models.py\n2. update loader\nVerify: pytest"


async def test_plan_then_build() -> None:
    user_settings({})
    server, conn, sid = await session([call("present_plan", {"plan": PLAN}), text("on it")])
    await server.prompt(sid, [acp.text_block("Make a plan for moving the settings module to pydantic")])
    await settle()
    first = mode_events(conn)[0]
    check(first["kind"] == "adaptive" and first["mode"] == "plan" and first["from"] == "build" and "plan" in first["reason"], f"the turn started in plan, with the reason ({first})")
    check(any(getattr(u, "current_mode_id", None) == "plan" for u in conn.updates), "ACP's own current_mode_update said so too")
    title, option_ids, raw = conn.asked[0]
    check(title.startswith("Run present_plan") and option_ids == [plan_mode.OPTION_BUILD, plan_mode.OPTION_EDIT, plan_mode.OPTION_AUTO, plan_mode.OPTION_REVISE], f"the plan reaches the person as a card with four answers ({option_ids})")
    check(raw.get("plan") == PLAN, "…carrying the plan")
    check(mode_of(server, sid) == "build", "approve → build: the session left plan mode")
    approved = [e for e in mode_events(conn) if e["kind"] == "plan_approved"]
    check(len(approved) == 1 and approved[0]["mode"] == "build" and approved[0]["from"] == "plan", "the client was told")
    result = MODELS["orchestrator"].calls[1]["last_text"]
    check("Plan approved" in result and "build mode" in result, f"the agent was told ({result[:70]!r})")


async def test_approve_auto() -> None:
    user_settings({})
    server, conn, sid = await session([call("present_plan", {"plan": PLAN}), text("on it")])
    conn.answer = plan_mode.OPTION_AUTO
    await server.prompt(sid, [acp.text_block("Make a plan for moving the settings module to pydantic")])
    await settle()
    check(mode_of(server, sid) == "auto", "approve → auto: the person chose auto, so the session is in auto")
    check("auto mode" in MODELS["orchestrator"].calls[1]["last_text"], "…and the agent is told")


async def test_keep_planning() -> None:
    user_settings({})
    server, conn, sid = await session([call("present_plan", {"plan": PLAN}), text("revising")])
    conn.answer = plan_mode.OPTION_REVISE
    conn.note = "split step 2, and keep the old loader for now"
    await server.prompt(sid, [acp.text_block("Make a plan for moving the settings module to pydantic")])
    await settle()
    check(mode_of(server, sid) == "plan", "keep planning: still in plan mode")
    result = MODELS["orchestrator"].calls[1]["last_text"]
    check("not approved" in result and "split step 2" in result and "present_plan again" in result, f"the agent gets the person's note and is told to revise ({result[:90]!r})")
    check(not any(e["kind"] == "plan_approved" for e in mode_events(conn)), "no approval was announced")


async def test_manual_choice_holds_adaptive_off() -> None:
    user_settings({})
    server, conn, sid = await session([text("ok")])
    await server.set_session_mode(sid, "build")  # the person picks build by hand (as Tab does)
    await server.set_session_mode(sid, "auto")
    await server.prompt(sid, [acp.text_block("Make a plan for moving the settings module to pydantic")])
    await settle()
    check(mode_of(server, sid) == "auto" and not mode_events(conn), "a mode the person picked is not overridden for the next turns")


async def test_switched_off() -> None:
    user_settings({"modes.adaptive": False})
    server, conn, sid = await session([text("ok")])
    await server.prompt(sid, [acp.text_block("Make a plan for moving the settings module to pydantic")])
    await settle()
    check(mode_of(server, sid) == "build" and not mode_events(conn), "modes.adaptive=false leaves the mode alone")


async def test_not_in_plan_is_refused() -> None:
    user_settings({"modes.adaptive": False})
    server, conn, sid = await session([call("present_plan", {"plan": PLAN}), text("ok")])
    await server.prompt(sid, [acp.text_block("do the thing")])
    await settle()
    check(not conn.asked, "present_plan outside plan mode is not even put to the person")
    result = MODELS["orchestrator"].calls[1]["last_text"]
    check("for plan mode" in result and mode_of(server, sid) == "build", f"…it is refused and the mode cannot be changed by it ({result[:80]!r})")


async def test_auto_is_tightened() -> None:
    user_settings({})
    server, conn, sid = await session([text("ok")], mode="auto")
    await server.prompt(sid, [acp.text_block("Make a plan for moving the settings module to pydantic")])
    await settle()
    check(mode_of(server, sid) == "plan" and mode_events(conn)[0]["from"] == "auto", "an auto session is tightened to plan for a plan-worthy request")


async def test_approve_edit() -> None:
    user_settings({})
    server, conn, sid = await session([call("present_plan", {"plan": PLAN}), text("on it")])
    conn.answer = plan_mode.OPTION_EDIT
    await server.prompt(sid, [acp.text_block("Make a plan for moving the settings module to pydantic")])
    await settle()
    check(conn.asked[0][1] == [plan_mode.OPTION_BUILD, plan_mode.OPTION_EDIT, plan_mode.OPTION_AUTO, plan_mode.OPTION_REVISE], f"the plan card offers build, edit, auto and keep planning ({conn.asked[0][1]})")
    check(mode_of(server, sid) == "edit", "approve → edit: the session is in edit mode")
    check("edit mode" in MODELS["orchestrator"].calls[1]["last_text"] and "commands still ask" in MODELS["orchestrator"].calls[1]["last_text"], "…and the agent is told what that means")


async def test_edit_mode_runs_edits_and_asks_for_commands() -> None:
    user_settings({"modes.adaptive": False})
    server, conn, sid = await session(
        [call("editor", {"command": "create", "path": "notes.md", "file_text": "hello\n"}), call("shell", {"command": "echo hi"}), text("done")],
        mode="edit",
    )
    conn.answer = "allow"
    await server.prompt(sid, [acp.text_block("write notes.md then echo hi")])
    await settle()
    cwd = Path(server._sessions[sid].cwd)
    check((cwd / "notes.md").read_text() == "hello\n", "the edit ran")
    asked = [title for title, _, _ in conn.asked]
    check(asked == ["Run shell"], f"…without asking, while the command asked ({asked})")
    check(mode_of(server, sid) == "edit", "the mode is unchanged")


async def test_ask_user() -> None:
    user_settings({"modes.adaptive": False})
    q = {"question": "Which loader?", "options": ["pydantic-settings", "dynaconf", "keep ours"]}
    server, conn, sid = await session([call("ask_user", q), text("ok")], mode="plan")
    conn.answer = "opt2"
    await server.prompt(sid, [acp.text_block("pick a loader")])
    await settle()
    title, option_ids, raw = conn.asked[0]
    check(title.startswith("Run ask_user") and option_ids == ["opt1", "opt2", "opt3", "skip", "decline"], f"the question reaches the person as a card whose options are the choices ({option_ids})")
    check(raw.get("question") == "Which loader?", "…carrying the question")
    check("The person chose: dynaconf" in MODELS["orchestrator"].calls[1]["last_text"], "the pick comes back as the tool's result — in plan mode too")
    check(mode_of(server, sid) == "plan", "a question changes no mode")

    server, conn, sid = await session([call("ask_user", q), text("ok")], mode="build")
    conn.answer = "skip"
    conn.note = "use whatever the repo already has"
    await server.prompt(sid, [acp.text_block("pick a loader")])
    await settle()
    check("in their own words: use whatever the repo already has" in MODELS["orchestrator"].calls[1]["last_text"], "typed words come back as the answer")

    server, conn, sid = await session([call("ask_user", q), text("ok")], mode="build")
    conn.answer = "skip"
    await server.prompt(sid, [acp.text_block("pick a loader")])
    await settle()
    check("left it to you" in MODELS["orchestrator"].calls[1]["last_text"], "'decide for me' says so")

    server, conn, sid = await session([call("ask_user", {"question": "Which?", "options": []}), text("ok")], mode="build")
    await server.prompt(sid, [acp.text_block("pick")])
    await settle()
    check(not conn.asked and "two to six options" in MODELS["orchestrator"].calls[1]["last_text"], "a question with no options is refused, not asked")


def test_remote_doors_cannot_turn_a_click_into_an_answer() -> None:
    """The Discord bot and the GUI map Allow → the first allow-once option, Deny → reject-once, Always → allow-always."""
    first = lambda options, kind: next((o.option_id for o in options if o.kind == kind), None)  # noqa: E731
    plan = plan_mode.options()
    check(first(plan, "allow_once") == plan_mode.OPTION_BUILD and first(plan, "allow_always") is None, "on a plan card a remote Allow means build, and there is no 'always' that could mean auto")
    check(first(plan, "reject_once") == plan_mode.OPTION_REVISE, "…and Deny means keep planning")
    import ask_user as au

    asked = au.options({"options": ["a", "b", "c"]})
    check(first(asked, "allow_once") == "skip" and first(asked, "reject_once") == "decline" and first(asked, "allow_always") is None, "on a question a remote Allow means 'decide for me' and Deny skips it — never option 1")
    check([o.option_id for o in asked if o.kind == "reject_always"] == ["opt1", "opt2", "opt3"], "…the numbered choices are reachable by id only")


async def main() -> None:
    test_advisor()
    test_remote_doors_cannot_turn_a_click_into_an_answer()
    await test_plan_then_build()
    await test_approve_auto()
    await test_keep_planning()
    await test_manual_choice_holds_adaptive_off()
    await test_switched_off()
    await test_not_in_plan_is_refused()
    await test_auto_is_tightened()
    await test_approve_edit()
    await test_edit_mode_runs_edits_and_asks_for_commands()
    await test_ask_user()
    print("\nadaptive mode: all checks passed")


if __name__ == "__main__":
    asyncio.run(main())
