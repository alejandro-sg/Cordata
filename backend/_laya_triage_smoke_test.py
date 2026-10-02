"""Regression check for laya_triage.py (Laya as the orchestrator's triage:
route mode hints + model tiers, shadow mode logging) and _laya_triage_report.py.
No Laya weights, no LLM: the classifier is injected, and a real Strands Agent
with a scripted model exercises the actual hook lifecycle, including exactly
what the model is handed.

    backend/.venv/bin/python _laya_triage_smoke_test.py
"""

import asyncio
import json
import tempfile
import time
from pathlib import Path

from strands import Agent, ToolContext, tool
from strands.models.model import Model

import _laya_triage_export as export
import _laya_triage_report as report
import laya_triage as lt
import secret_guard as sg
import settings

BASE_PROMPT = "You are the test orchestrator."


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


# ------------------------------------------------------------------ helpers


def test_pure_helpers() -> None:
    check(lt.actual_route([]) == "chat", "no tools -> chat")
    check(lt.actual_route(["think"]) == "chat", "`think` alone is not route evidence")
    check(lt.actual_route(["shell"]) == "files_and_code", "shell -> files_and_code")
    check(lt.actual_route(["http_request", "shell"]) == "web_research", "web research outranks code")
    check(lt.actual_route(["shell", "local_swarm"]) == "team", "a team call outranks everything")
    check(lt.actual_route(["agent_memory"]) == "memory", "agent_memory -> memory")
    check(lt.actual_route(["calculator"]) == "other_tools", "an unlisted tool -> other_tools")
    check(lt.actual_route(["think", "calculator"]) == "other_tools", "think is ignored next to a real tool")

    msg = lambda text, role="user": {"role": role, "content": [{"text": text}]}  # noqa: E731
    check(lt.user_text([msg("hello")]) == "hello", "user_text reads a user message")
    check(lt.user_text("plain string") == "plain string", "user_text accepts a bare string")
    check(lt.user_text([msg("[Style preference: brief, short replies]\nwhat is 2+2?")]) == "what is 2+2?", "the AG-UI style prefix is stripped")
    check(lt.user_text([msg("a"), msg("reply", "assistant"), msg("latest")]) == "latest", "the last user message wins")
    check(lt.user_text([{"interruptResponse": {"interruptId": "x", "response": True}}]) is None, "an approval-resume payload carries no new user turn")
    check(lt.user_text(None) is None and lt.user_text([]) is None, "no input -> None")
    check(set(lt.TRIAGE_QUESTIONS) == {"route", "needs_team", "complexity"}, "triage asks route / needs_team / complexity")
    check(set(lt.TRIAGE_QUESTIONS["route"]["criteria"]) == set(lt.ROUTES), "route choice keys match ROUTES")
    check(set(lt.TRIAGE_QUESTIONS["needs_team"]["criteria"]) == {"true", "false"}, "noul criteria use the true/false keys Laya requires")
    check(not any(k in ("true", "false", "yes", "no") for k in lt.TRIAGE_QUESTIONS["route"]["criteria"]), "no boolean-word labels in the choice question")


def decision(route="chat", conf=0.95, team=0.1, cx=1.0, cx_conf=0.9) -> lt.Decision:
    return lt.Decision({"route": route, "route_confidence": conf, "needs_team": team, "complexity": cx, "complexity_confidence": cx_conf})


def test_decision_policy() -> None:
    check("http_request" in decision("web_research", 0.95).hint(), "a confident web_research route hints at http_request")
    check("file and code tools" in decision("files_and_code", 0.9).hint(), "files_and_code hints at the file/code tools")
    check("agent_memory" in decision("memory", 0.9).hint(), "memory hints at agent_memory")
    check(decision("chat", 0.99).hint() is None, "a chat route never hints (don't discourage tools)")
    check(decision("other_tools", 0.99).hint() is None, "other_tools never hints")
    check(decision("web_research", 0.60).hint() is None, "below the confidence gate: no hint")
    check(decision("chat", 0.99, team=0.9).hint() is None, "REGRESSION (live run): a high needs_team on a trivial chat prompt must NOT hint a team by default")
    old = settings.LAYA_TRIAGE_TEAM_THRESHOLD
    settings.LAYA_TRIAGE_TEAM_THRESHOLD = 0.8
    try:
        check("local_graph" in decision("chat", 0.99, team=0.9).hint(), "with LAYA_TRIAGE_TEAM_THRESHOLD set, a high P(needs team) points at the team tools")
        check(decision("chat", 0.99, team=0.5).hint() is None, "…and a low one does not")
    finally:
        settings.LAYA_TRIAGE_TEAM_THRESHOLD = old
    check("local_graph" in decision("team", 0.9).hint(), "a confident team route points at the team tools")
    check("0.95" in decision("web_research", 0.95).hint() and "advisory" in decision("web_research", 0.95).hint(), "the hint states the confidence and that it is advisory")
    hint = decision("web_research", 0.95).hint()
    check("{" not in hint and "<" not in hint, "the hint is fixed text with no interpolated user content")

    check(decision(cx=0.3).tier() == "fast", "low complexity -> fast tier")
    check(decision(cx=1.0).tier() == "standard", "middling complexity -> standard tier")
    check(decision(cx=1.8).tier() == "strong", "high complexity -> strong tier")
    check(decision(cx=1.8, cx_conf=0.4).tier() is None, "an unsure complexity estimate suggests no tier")


# --------------------------------------------------------------- real agent


class Scripted(Model):
    """Per user turn: optionally call one tool, then answer 'done'. Records the
    system prompt and the messages of every model call."""

    def __init__(self, tool_per_turn: list[str | None]) -> None:
        self.plan = tool_per_turn
        self.turn = -1
        self.system_prompts: list[str | None] = []
        self.message_dumps: list[str] = []

    def update_config(self, **kw):
        pass

    def get_config(self):
        return {}

    async def structured_output(self, *a, **k):
        raise NotImplementedError
        yield  # pragma: no cover

    async def stream(self, messages, tool_specs=None, system_prompt=None, **kw):
        self.system_prompts.append(system_prompt)
        self.message_dumps.append(json.dumps(messages, default=str))
        last = messages[-1]
        fresh_user_turn = last["role"] == "user" and not any("toolResult" in b for b in last["content"])
        if fresh_user_turn:
            self.turn += 1
        tool_name = self.plan[self.turn] if fresh_user_turn else None
        yield {"messageStart": {"role": "assistant"}}
        if tool_name:
            yield {"contentBlockStart": {"start": {"toolUse": {"toolUseId": f"t{self.turn}", "name": tool_name}}}}
            yield {"contentBlockDelta": {"delta": {"toolUse": {"input": "{}"}}}}
            yield {"contentBlockStop": {}}
            yield {"messageStop": {"stopReason": "tool_use"}}
        else:
            yield {"contentBlockStart": {"start": {}}}
            yield {"contentBlockDelta": {"delta": {"text": "done"}}}
            yield {"contentBlockStop": {}}
            yield {"messageStop": {"stopReason": "end_turn"}}
        yield {"metadata": {"usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}, "metrics": {"latencyMs": 1}}}


@tool
def calculator() -> str:
    """Pretend calculator."""
    return "4"


@tool
def http_request() -> str:
    """Pretend fetch."""
    return "page"


tier_seen: list[str | None] = []


@tool(context=True)
def spawn_team(tool_context: ToolContext) -> str:
    """Stands in for local_swarm: asks what tier Laya suggests, as sub_agent_common does."""
    tier_seen.append(lt.suggested_tier(tool_context.agent))
    lt.record_team_tier(tool_context.agent, "researcher", tier_seen[-1])
    return "team done"


def wait_for_lines(path: Path, count: int, timeout: float = 10.0) -> list[dict]:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists():
            lines = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
            if len(lines) >= count:
                return lines
        time.sleep(0.05)
    raise AssertionError(f"FAIL: expected {count} log lines within {timeout}s, got {path.read_text() if path.exists() else '<no file>'}")


def predictor(**fixed):
    """A fake classifier: returns `fixed` fields over a neutral decision; `sleep`
    delays it, `boom` raises, and 'research' in the text makes it web_research."""

    def predict(text: str) -> dict:
        if fixed.get("sleep"):
            time.sleep(fixed["sleep"])
        if "boom" in text:
            raise RuntimeError("classifier exploded")
        data = {"route": "chat", "route_confidence": 0.9, "route_probs": {"chat": 0.9}, "needs_team": 0.1,
                "complexity": 1.0, "complexity_confidence": 0.9, "latency_ms": 5.0, "model": "fake"}
        if "research" in text:
            data.update(route="web_research", route_confidence=0.95)
        data.update({k: v for k, v in fixed.items() if k != "sleep"})
        return data

    return predict


def run_agent(mode: str, predict, prompts: list[str], plan: list[str | None], tools=None):
    tmp = tempfile.TemporaryDirectory()
    log = Path(tmp.name) / "triage.jsonl"
    model = Scripted(plan)
    agent = Agent(
        model=model,
        system_prompt=BASE_PROMPT,
        tools=tools or [calculator, http_request],
        hooks=[lt.LayaTriageHooks(mode=mode, log_path=log, predict=predict)],
        callback_handler=None,
    )

    async def go() -> None:
        for prompt in prompts:
            await agent.invoke_async(prompt)

    asyncio.run(go())
    rows = wait_for_lines(log, len(prompts))
    rows.sort(key=lambda r: r["ts"])
    return agent, model, rows, tmp


def test_shadow_mode() -> None:
    agent, model, rows, tmp = run_agent(
        "shadow", predictor(), ["[Style preference: brief]\nhello there", "please research strands", "what is 2+2 with the calculator", "boom: make the classifier raise"],
        [None, "http_request", "calculator", None],
    )
    with tmp:
        check([r["actual"]["route"] for r in rows] == ["chat", "web_research", "other_tools", "chat"], "actual routes come from the tools really called")
        check(rows[0]["text"] == "hello there", "the logged text is the user's words (style prefix removed)")
        check(rows[1]["laya"]["route"] == "web_research" and rows[0]["laya"]["route"] == "chat", "laya's would-be route is logged beside it")
        check(rows[1]["actual"]["tools"] == ["http_request"] and rows[1]["actual"]["stop_reason"] == "end_turn", "tools and stop_reason are recorded")
        check(all(r["actual"]["turn_ms"] is not None and not r["actual"]["incomplete"] for r in rows), "turn timing recorded; none incomplete")
        check("error" in rows[3]["laya"] and "classifier exploded" in rows[3]["laya"]["error"], "a classifier failure is logged, not raised into the turn")
        check(rows[3]["actual"]["route"] == "chat", "…and the turn itself still completed normally")
        check(len(rows) == 4 and all(r["mode"] == "shadow" for r in rows), "exactly one record per user turn, tagged with the mode")
        check(all(p == BASE_PROMPT for p in model.system_prompts), "shadow mode never touches the system prompt")
        check(all(r["hint"] is None for r in rows), "shadow mode applies no hint")


def test_route_mode_hint() -> None:
    agent, model, rows, tmp = run_agent("route", predictor(), ["please research strands"], ["http_request"])
    with tmp:
        check(len(model.system_prompts) == 2, "the model was called twice (tool call, then answer)")
        check(all("Routing hint" in p and "http_request" in p and p.startswith(BASE_PROMPT) for p in model.system_prompts), "every model call in the turn carried the base prompt plus the hint")
        check(agent.system_prompt == BASE_PROMPT, "the system prompt is restored after the turn")
        check(all("Routing hint" not in dump for dump in model.message_dumps), "the hint is never in the conversation messages (not persisted)")
        check("Routing hint" not in json.dumps(agent.messages, default=str), "…nor in the stored history afterwards")
        check(rows[0]["hint"] is not None and rows[0]["mode"] == "route" and rows[0]["waited_ms"] is not None, "the applied hint and wait are logged")

    # next turn without a confident route is clean again
    agent, model, rows, tmp = run_agent("route", predictor(), ["research it", "just chat"], ["http_request", None])
    with tmp:
        check("Routing hint" in model.system_prompts[0], "turn 1 (confident) was hinted")
        check(model.system_prompts[-1] == BASE_PROMPT, "turn 2 (chat route) was not — hints do not leak across turns")


def test_route_mode_fallbacks() -> None:
    agent, model, rows, tmp = run_agent("route", predictor(route_confidence=0.5, route="web_research"), ["research low confidence"], [None])
    with tmp:
        check(all(p == BASE_PROMPT for p in model.system_prompts) and rows[0]["hint"] is None, "below the confidence gate: the LLM decides alone")

    old_timeout = settings.LAYA_TRIAGE_TIMEOUT
    settings.LAYA_TRIAGE_TIMEOUT = 0.05
    try:
        started = time.time()
        agent, model, rows, tmp = run_agent("route", predictor(sleep=0.6), ["research but slow classifier"], [None])
        elapsed = time.time() - started
    finally:
        settings.LAYA_TRIAGE_TIMEOUT = old_timeout
    with tmp:
        check(all(p == BASE_PROMPT for p in model.system_prompts) and rows[0]["hint"] is None, "a classifier slower than the timeout: no hint, the turn proceeds")
        check(rows[0]["laya"]["route"] == "web_research", "…but its late answer is still logged")
        check(rows[0]["waited_ms"] < 400, "…and the turn waited only the timeout, not the classifier")

    agent, model, rows, tmp = run_agent("route", predictor(), ["boom: raise please"], [None])
    with tmp:
        check(all(p == BASE_PROMPT for p in model.system_prompts) and "error" in rows[0]["laya"], "a classifier error: no hint, turn completes, error logged")

    agent, model, rows, tmp = run_agent("route", predictor(needs_team=0.92), ["pong"], [None])
    with tmp:
        check(model.system_prompts[0] == BASE_PROMPT, "by default a high needs_team alone adds no hint (the observed pong case)")
    settings.LAYA_TRIAGE_TEAM_THRESHOLD = 0.8
    try:
        agent, model, rows, tmp = run_agent("route", predictor(needs_team=0.92), ["just chat but a big job"], [None])
        with tmp:
            check("local_graph" in model.system_prompts[0], "with the threshold enabled, a high P(needs team) hints at the team tools")
    finally:
        settings.LAYA_TRIAGE_TEAM_THRESHOLD = None


def test_tier_suggestion() -> None:
    tier_seen.clear()
    agent, model, rows, tmp = run_agent("route", predictor(complexity=1.8, complexity_confidence=0.85), ["hard job"], ["spawn_team"], tools=[spawn_team])
    with tmp:
        check(tier_seen == ["strong"], "a tool running in the turn sees Laya's suggested tier")
        check(rows[0]["actual"]["tiers"] == {"researcher": "strong"}, "the tier each spawned sub-agent got is logged")
        check(lt.suggested_tier(agent) is None, "outside a turn there is no suggestion")

    tier_seen.clear()
    agent, model, rows, tmp = run_agent("route", predictor(complexity=1.8, complexity_confidence=0.3), ["hard job"], ["spawn_team"], tools=[spawn_team])
    with tmp:
        check(tier_seen == [None], "an unsure complexity estimate suggests nothing")

    tier_seen.clear()
    agent, model, rows, tmp = run_agent("shadow", predictor(complexity=1.8), ["hard job"], ["spawn_team"], tools=[spawn_team])
    with tmp:
        check(tier_seen == [None], "shadow mode never drives tiers")


def test_secrets_and_modes() -> None:
    secret = "triage-secret-0123456789-abcdef"
    sg.register_secret(secret)
    seen: list[str] = []
    agent, model, rows, tmp = run_agent("route", lambda text: seen.append(text) or predictor()(text), [f"my key is {secret}"], [None])
    with tmp:
        check(secret not in json.dumps(rows) and sg.REDACTED in rows[0]["text"], "a pasted secret is masked in the log")
        check(all(secret not in text for text in seen), "…and is never handed to the classifier")

    real_warm = lt.warm_up_laya
    lt.warm_up_laya = lambda: None  # don't load real weights in a unit test
    old = settings.LAYA_TRIAGE_MODE
    try:
        for mode, expected in (("route", 1), ("shadow", 1), ("off", 0), ("nonsense", 0)):
            settings.LAYA_TRIAGE_MODE = mode
            hooks = lt.triage_hooks()
            check(len(hooks) == expected and (not hooks or hooks[0].mode == mode), f"triage_hooks() for mode {mode!r} returns {expected} hook(s)")
    finally:
        settings.LAYA_TRIAGE_MODE = old
        lt.warm_up_laya = real_warm
    check(old == "route" or "LAYA_TRIAGE_MODE" in __import__("os").environ, "the default mode is route unless the environment says otherwise")


def test_report() -> None:
    def rec(actual: str, laya: str, conf: float, team_p: float = 0.1, incomplete: bool = False) -> dict:
        return {"actual": {"route": actual, "turn_ms": 1000, "incomplete": incomplete},
                "laya": {"route": laya, "route_confidence": conf, "needs_team": team_p, "latency_ms": 30}}

    records = (
        [rec("chat", "chat", 0.95)] * 6
        + [rec("chat", "memory", 0.45)] * 2
        + [rec("web_research", "web_research", 0.85)] * 3
        + [rec("web_research", "files_and_code", 0.55)]
        + [rec("team", "team", 0.9, team_p=0.9)] * 2
        + [rec("team", "chat", 0.3, team_p=0.4, incomplete=True)]
        + [{"actual": {"route": "chat"}, "laya": {"error": "unavailable"}}]
    )
    text = report.build_report(records, (0.5, 0.9))
    check("usable: 14" in text and "laya errors: 1" in text and "incomplete turns: 1" in text, "report counts usable / errored / incomplete turns")
    check("11/14 = 78.6%" in text, "agreement is computed over usable turns")
    check("majority-class baseline    : 8/14 = 57.1%   (always 'chat')" in text, "the majority-class baseline is printed")
    check("only 14 usable turns" in text, "a small sample is flagged as anecdotal")
    check("chat -> memory: 2" in text, "top disagreements are listed")
    gate90 = next(l for l in text.splitlines() if l.strip().startswith("0.90"))
    check(gate90.split()[-1] == "0" and "100%" in gate90, "at threshold 0.90 every covered turn agrees (0 misrouted)")
    check(report.percentile([10, 20, 30, 40, 50, 60, 70, 80, 90, 100], 95) == 100, "percentile uses nearest rank")
    check("nothing to compare" in report.build_report([{"actual": {"route": "chat"}, "laya": {"error": "x"}}]), "an all-error log degrades gracefully")


def test_export() -> None:
    def rec(text, route, stop="end_turn", incomplete=False):
        return {"text": text, "actual": {"route": route, "stop_reason": stop, "incomplete": incomplete}}

    row = export.to_row(rec("research strands", "web_research"))
    gold = json.loads(row["gold"])
    check(json.loads(row["state"]) == "research strands", "an exported row's state is the JSON-encoded request")
    check(gold["route"]["probabilities"]["web_research"] == 1.0 and sum(gold["route"]["probabilities"].values()) == 1.0, "the route label is one-hot over every route key")
    check(gold["needs_team"]["probabilities"] == {"true": 0.0, "false": 1.0}, "a non-team turn is labelled needs_team=false")
    check(json.loads(export.to_row(rec("big job", "team"))["gold"])["needs_team"]["probabilities"]["true"] == 1.0, "a team turn is labelled needs_team=true")
    check("complexity" not in gold and "complexity" not in json.loads(row["questions"]), "complexity (no observed label) is left out")
    check(export.to_row(rec("x", "chat", incomplete=True)) is None and export.to_row(rec("x", "chat", stop="max_tokens")) is None, "cut-off and non-end_turn turns are not exported")
    check(export.to_row(rec("", "chat")) is None and export.to_row(rec("x", "bogus_route")) is None, "empty text and unknown routes are skipped")

    with tempfile.TemporaryDirectory() as tmp:
        log = Path(tmp) / "log.jsonl"
        log.write_text("\n".join(json.dumps(r) for r in [rec("a", "chat"), rec("a", "chat"), rec("b", "memory"), rec("c", "chat", incomplete=True)]) + "\nnot json\n")
        pairs, skipped = export.load_rows([log])
        check([t for t, _ in pairs] == ["a", "b"] and skipped == 3, "duplicates, incomplete turns and malformed lines are dropped")
        check(export._split_of("same text", 0.5) == export._split_of("same text", 0.5), "the train/eval split is deterministic per text")
        out = Path(tmp) / "out"
        check(export.main([str(log), "--out", str(out)]) == 0 and (out / "train.jsonl").exists() and (out / "eval.jsonl").exists(), "the exporter writes train.jsonl and eval.jsonl")
        check(sum(1 for _ in (out / "train.jsonl").open()) + sum(1 for _ in (out / "eval.jsonl").open()) == 2, "every kept row lands in exactly one split")

    # Format check against Laya's own item builder (needs the checkpoint; skipped, not failed, without it).
    problems = export.validate_rows([export.to_row(rec("list the python files", "files_and_code"))])
    if problems and "could not load Laya" in problems[0]:
        print("skip Laya build_sequence validation (checkpoint unavailable)")
    else:
        check(problems == [], "exported rows are accepted by Laya's own build_sequence (the training script's builder)")


def main() -> None:
    test_pure_helpers()
    test_decision_policy()
    test_shadow_mode()
    test_route_mode_hint()
    test_route_mode_fallbacks()
    test_tier_suggestion()
    test_secrets_and_modes()
    test_report()
    test_export()
    print("\nPASS: laya triage hints, tiers and logs per turn — gated, bounded, never persisted, never blocking")


if __name__ == "__main__":
    main()
