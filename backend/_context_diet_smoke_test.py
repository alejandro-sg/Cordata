"""The token diet (context_diet.py): fewer tokens per model call, nothing taken away.

* **unit** — `shorten` keeps head and tail and says what is gone, once; `cap_result` leaves small results alone and
  shortens only the blocks that are over; `age_messages` spares the task and the newest messages, shortens old tool
  output and old huge tool inputs, keeps every tool call paired with its result, and is idempotent; `cold_for`
  hides what the message does not mention and spares what it does, what was used lately, and `http_request` on a
  web route.
* **through a real Strands loop** — a scripted agent: a plain turn does not send the cold tools but lists them and
  offers `enable_tools`; asking for one loads it for the next step; a message that mentions a subject loads that
  tool from the start; a tool the agent used stays loaded for the next turns; the registry and the prompt are
  restored after the turn; a huge tool result is capped in what the model reads next; and a long turn's early
  output is shortened in what the model sees later while the first message is untouched.
* **off switches** — `context.coldTools: false`, and a limit of 0 for aging.

Nothing here talks to a model: `harness_testing.ScriptedModel` plays back fixed steps and records what each call saw.

    backend/.venv/bin/python _context_diet_smoke_test.py
"""

import os

os.environ.setdefault("SIERRALAB_REFLECT", "off")

from strands import Agent, tool

import context_diet as cd
from harness_testing import ScriptedModel, check, text
from harness_testing import tool as call


class Prefs(dict):
    """The settings view the diet reads: `.get(key, default)`."""


# ------------------------------------------------------------------------------------------ unit


def test_shorten_and_cap() -> None:
    small = "hello"
    check(cd.shorten(small, 100) is small, "text within the limit is returned as it is")
    big = "HEAD" + "x" * 50_000 + "TAIL: exit code 1"
    cut = cd.shorten(big, 1000)
    check(len(cut) < 1300 and cut.startswith("HEAD") and cut.endswith("TAIL: exit code 1"), "a long text keeps its head and its tail — an error or an exit code is at one end")
    check("characters omitted to save tokens" in cut and "50,021 in all" in cut and "read a narrower slice" in cut, "and says how much is gone and how to ask for the rest")
    check(cd.shorten(cut, 1000) is cut, "shortening twice changes nothing: the marker stops it")

    result = {"toolUseId": "t", "status": "success", "content": [{"text": "small"}, {"text": big}, {"json": {"a": "b" * 30_000}}, {"image": {"format": "png", "source": {"bytes": b"x"}}}]}
    capped, saved = cd.cap_result(result, 1000)
    check(saved > 70_000 and capped["content"][0] == {"text": "small"}, "only the blocks over the limit are shortened; a small one is untouched")
    check(len(capped["content"]) == 4 and "image" in capped["content"][3], "every block is kept, in order — an image is not text and is left alone")
    check("text" in capped["content"][2] and len(capped["content"][2]["text"]) < 1300, "a huge json block becomes a short text block")
    check(capped["toolUseId"] == "t" and capped["status"] == "success", "the result keeps its id and status")
    same, none = cd.cap_result({"toolUseId": "t", "status": "success", "content": [{"text": "ok"}]}, 1000)
    check(none == 0 and same["content"] == [{"text": "ok"}], "a small result is returned unchanged")
    check(cd.cap_result("not a result", 1000) == ("not a result", 0) and cd.cap_result(result, 0)[1] == 0, "a non-result, or a limit of 0, is left alone")


def history(n_exchanges: int, size: int = 6000) -> list:
    messages: list = [{"role": "user", "content": [{"text": "THE TASK: " + "t" * 50}]}]
    for i in range(n_exchanges):
        messages.append({"role": "assistant", "content": [{"text": f"step {i}"}, {"toolUse": {"toolUseId": f"u{i}", "name": "file_read", "input": {"path": f"f{i}.py", "note": "n" * 5000}}}]})
        messages.append({"role": "user", "content": [{"toolResult": {"toolUseId": f"u{i}", "status": "success", "content": [{"text": f"RESULT-{i} " + "r" * size + f" END-{i}"}]}}]})
    messages.append({"role": "assistant", "content": [{"text": "done"}]})
    return messages


def test_aging() -> None:
    messages = history(10)
    before = sum(len(str(m)) for m in messages)
    saved = cd.age_messages(messages, keep=8, result_limit=1000, input_limit=1000)
    after = sum(len(str(m)) for m in messages)
    check(saved > 30_000 and before - after > 30_000, f"old tool output and old huge inputs are shortened ({saved:,} characters)")
    check(messages[0]["content"][0]["text"].startswith("THE TASK: ") and len(messages[0]["content"][0]["text"]) == 60, "the first message — the task as the person worded it — is never touched")
    old_result = messages[2]["content"][0]["toolResult"]["content"][0]["text"]
    check(old_result.startswith("RESULT-0") and old_result.endswith("END-0") and "shortened to save tokens" in old_result, "an old result keeps its head and tail and says it was shortened")
    check(len(messages[1]["content"][1]["toolUse"]["input"]["note"]) < 1200 and messages[1]["content"][1]["toolUse"]["input"]["path"] == "f0.py", "an old huge tool input is shortened, a short field beside it is not")
    fresh = messages[-2]["content"][0]["toolResult"]["content"][0]["text"]
    check(len(fresh) > 5000 and "shortened" not in fresh, "the newest messages are left in full — the next decision is made from them")
    uses = {b["toolUse"]["toolUseId"] for m in messages for b in m["content"] if "toolUse" in b}
    results = {b["toolResult"]["toolUseId"] for m in messages for b in m["content"] if "toolResult" in b}
    check(uses == results and len(uses) == 10, "every tool call still has its result: a provider never sees an orphan")
    check(cd.age_messages(messages, keep=8, result_limit=1000, input_limit=1000) == 0, "ageing is idempotent")
    check(cd.age_messages(history(2), keep=8, result_limit=1000) == 0 and cd.age_messages(history(10), result_limit=0) == 0, "a short conversation, or a limit of 0, is left alone")


def test_cold_for() -> None:
    loaded = {name: object() for name in ("context_usage", "http_request", "set_temperature", "claude_code_engineer", "list_models")}
    hidden = cd.cold_for("fix the failing test in parser.py", cold=loaded, recent=set())
    check(set(hidden) == set(loaded), "a plain coding turn needs none of them")
    hidden = cd.cold_for("how much of the context window is left?", cold=loaded, recent=set())
    check("context_usage" not in hidden and "http_request" in hidden, "a question about context usage loads that tool")
    hidden = cd.cold_for("fetch https://example.com/docs and summarise", cold=loaded, recent=set())
    check("http_request" not in hidden, "a URL loads http_request")
    hidden = cd.cold_for("do the next one", cold=loaded, recent={"context_usage"})
    check("context_usage" not in hidden, "a tool used in the last few turns stays loaded for a follow-up")
    hidden = cd.cold_for("look at the code", cold=loaded, recent=set(), route="web")
    check("http_request" not in hidden, "a web route from Laya loads http_request")
    hidden = cd.cold_for("refactor the session layer across the repo", cold=loaded, recent=set())
    check("claude_code_engineer" not in hidden, "a big refactor request loads the Claude Code tool (and a small edit never does)")
    check("claude_code_engineer" in cd.cold_for("rename this variable", cold=loaded, recent=set()), "…so an ordinary edit cannot spend Claude Code usage")
    managers = {"omlx_resource_manager": object(), "ollama_resource_manager": object()}
    check(not cd.cold_for("load the planner model, then implement the feature", cold=managers, recent=set()), "'load the planner model' loads both resource managers — a phrasing the first pattern missed")
    check(set(cd.cold_for("fix the failing test", cold=managers, recent=set())) == set(managers), "…and a coding request loads neither")


# ------------------------------------------------------------------------------- through a real loop


@tool
def context_usage(question: str) -> str:
    """Report how much of the context window is used."""
    return "42% used"


@tool
def http_request(url: str) -> str:
    """Fetch a URL."""
    return f"fetched {url}"


@tool
def big_dump(label: str) -> str:
    """Return a huge output."""
    return f"START-{label} " + "line of output\n" * 4000 + f" END-{label}"


@tool
def small_tool(n: int) -> str:
    """Return a small output."""
    return f"small {n}"


def agent_with(steps, prefs=None, scope=True) -> tuple[Agent, ScriptedModel, cd.ContextDiet]:
    model = ScriptedModel(steps)
    diet = cd.ContextDiet(lambda: Prefs(prefs or {}), scope_tools=scope)
    agent = Agent(model=model, tools=[context_usage, http_request, big_dump, small_tool, cd.enable_tools], hooks=[diet], system_prompt="base prompt", callback_handler=None)
    return agent, model, diet


def test_cold_tools_through_the_loop() -> None:
    agent, model, diet = agent_with([text("ok")])
    agent("list the files in the repo")
    seen = model.calls[0]
    check("context_usage" not in seen["tools"] and "http_request" not in seen["tools"], "a plain turn does not send the cold tools")
    check({"big_dump", "small_tool", "enable_tools"} <= set(seen["tools"]), "…and still sends the rest, with the loader")
    prompt = str(seen["system_prompt"])
    check("Tools not loaded this turn" in prompt and "context_usage (report context-window usage" in prompt and "enable_tools" in prompt, "the prompt lists what is not loaded, what each is for, and how to get it")
    check("context_usage" in agent.tool_registry.registry and "http_request" in agent.tool_registry.registry, "after the turn the registry is whole again")
    check("Tools not loaded" not in str(agent.system_prompt), "…and the prompt is clean")
    check(diet.saved["hidden_tools"] == 2, "the diet counted what it hid")

    agent, model, _ = agent_with([call("enable_tools", {"names": ["context_usage"]}), call("context_usage", {"question": "how much?"}), text("42")])
    agent("list the files in the repo")
    first, second, third = model.calls[0], model.calls[1], model.calls[2]
    check("context_usage" not in first["tools"] and "context_usage" in second["tools"] and "context_usage" in third["tools"], "enable_tools loads a tool for the very next step")
    check("42" in model.calls[2]["last_text"], "…and the agent used it")

    agent, model, _ = agent_with([call("enable_tools", {"names": ["nope"]}), text("x")])
    agent("hello")
    check("Not a hidden tool: nope" in model.calls[1]["last_text"], "asking for a tool that is not hidden is told so, not a crash")

    agent, model, _ = agent_with([text("ok")])
    agent("how much of the context window is left?")
    check("context_usage" in model.calls[0]["tools"] and "http_request" not in model.calls[0]["tools"], "a message that mentions the subject loads that tool from the start")

    agent, model, _ = agent_with([call("enable_tools", {"names": ["http_request"]}), text("done"), text("again"), text("and again"), text("and once more")])
    agent("hello")
    agent("go on")
    agent("go on again")
    agent("one more")
    check("http_request" in model.calls[2]["tools"] and "http_request" in model.calls[3]["tools"], "a tool the agent used stays loaded for the turns after")
    agent("one more")
    agent("and another")
    check("http_request" not in model.calls[-1]["tools"], "…and is hidden again once it has not been needed for a while")


def test_switches() -> None:
    agent, model, diet = agent_with([text("ok")], prefs={"context.coldTools": False})
    agent("list the files")
    check("context_usage" in model.calls[0]["tools"] and "Tools not loaded" not in str(model.calls[0]["system_prompt"]), "context.coldTools=false sends every tool, as before")
    agent, model, diet = agent_with([text("ok")], scope=False)
    agent("list the files")
    check("context_usage" in model.calls[0]["tools"], "a fixed-kit agent (scope_tools=False) never has tools hidden")


def test_result_cap_through_the_loop() -> None:
    agent, model, diet = agent_with([call("big_dump", {"label": "A"}), text("ok")], prefs={"context.toolOutputChars": 3000})
    agent("dump it")
    seen = model.calls[1]["last_text"]
    check(len(seen) < 3500 and seen.startswith("START-A") and seen.endswith("END-A"), f"a huge tool result is capped in what the model reads next ({len(seen):,} characters)")
    check("characters omitted to save tokens" in seen, "and says so")
    check(diet.saved["results"] > 50_000, "the diet counted the saving")

    agent, model, diet = agent_with([call("big_dump", {"label": "A"}), text("ok")])
    agent("dump it")
    seen = model.calls[1]["last_text"]
    check(len(seen) < 13_000 and "omitted" in seen, f"the default cap is {cd.DEFAULT_TOOL_OUTPUT_CHARS:,} characters")


def test_aging_through_the_loop() -> None:
    steps = [call("big_dump", {"label": str(i)}) for i in range(7)] + [text("done")]
    agent, model, diet = agent_with(steps, prefs={"context.toolOutputChars": 100_000, "context.agedOutputChars": 800})
    agent("do the whole thing")
    last = model.calls[-1]
    check(last["last_text"].startswith("START-6") and len(last["last_text"]) > 40_000, "the newest tool result is seen in full")
    check(diet.saved["aged"] > 100_000, f"old tool output was shortened in what the model saw later ({diet.saved['aged']:,} characters)")
    first_user = agent.messages[0]["content"][0]["text"]
    check(first_user == "do the whole thing", "the task is exactly as the person wrote it")
    old = agent.messages[2]["content"][0]["toolResult"]["content"][0]["text"]
    check(old.startswith("START-0") and "shortened to save tokens" in old and len(old) < 1200, "the earliest result is a short stub with its head and tail")

    steps = [call("big_dump", {"label": str(i)}) for i in range(7)] + [text("done")]
    agent, model, diet = agent_with(steps, prefs={"context.toolOutputChars": 100_000, "context.agedOutputChars": 0})
    agent("do the whole thing")
    check(diet.saved["aged"] == 0 and agent.messages[2]["content"][0]["toolResult"]["content"][0]["text"].startswith("START-0 line of output"), "an ageing limit of 0 turns it off")


def main() -> None:
    test_shorten_and_cap()
    test_aging()
    test_cold_for()
    test_cold_tools_through_the_loop()
    test_switches()
    test_result_cap_through_the_loop()
    test_aging_through_the_loop()
    print("\nPASS: cold tools are loaded on demand, one result cannot swamp the context, old output ages — and nothing the agent could do is gone")


if __name__ == "__main__":
    main()
