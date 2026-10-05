"""Regression check for the Discord front door (daemon/discord_bot.py).

Four properties, each of which is the sort of thing that quietly stops being true:

1. **Fail closed.** No token, no `ALLOWED_USER_ID`, an id that is not an id, or an explicit
   `SIERRALAB_DISCORD=off` all leave the bot inert — and every one of them says why. A bot that
   starts with no allowlist looks like a front door and is an open one.
2. **Zero trust on the caller.** A click from any user other than the configured operator is an
   ephemeral refusal with *no* `answer_permission` call behind it, and a message from anyone else
   is dropped in silence. Asserted against a stub supervisor that records every relay, so "it
   refused" means the daemon never saw it.
3. **The relay is the GUI's relay.** An authorized click goes through
   `Supervisor.answer_permission` with `decided_by="discord"`, and an already-answered prompt
   (`LookupError`) is a message, not a second answer — the two front doors race by design and the
   loser has to be harmless.
4. **Only what the directive asked for goes out.** Destructive pendings are posted with the pair of
   buttons; sub-threshold ones are not. Workflow tasks open exactly one thread per session and land
   in it; chat tasks do not. Steering takes the Lab's guidance path, capped and acknowledged by
   length — never echoed back.

Hermetic: `fake_discord` is installed into `sys.modules` in place of the library, the supervisor is a
stub, `SIERRALAB_HOME` and the state db point at a temp directory, and nothing touches the network.

    backend/.venv/bin/python _discord_remote_smoke_test.py
"""

import asyncio
import json
import os
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

_TMP = tempfile.TemporaryDirectory()
_TMPDIR = Path(_TMP.name)
os.environ["SIERRALAB_HOME"] = str(_TMPDIR / "home")
os.environ["SIERRALAB_STATE_DB"] = str(_TMPDIR / "state.db")
sys.path.insert(0, str(Path(__file__).resolve().parent))

import fake_discord  # noqa: E402
import secret_guard  # noqa: E402

sys.modules["discord"] = fake_discord  # before discord_bot's lazy `import discord`

import daemon.discord_bot as bot  # noqa: E402

OPERATOR = 424242
OTHER = 999999
CHANNEL_ID = 555001
TOKEN = "MTIzNDU2Nzg5.fake-token-abcdefghijklmnop"
ENV = {"DISCORD_BOT_TOKEN": TOKEN, "ALLOWED_USER_ID": str(OPERATOR), "TELEMETRY_CHANNEL_ID": str(CHANNEL_ID)}


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


# --------------------------------------------------------------------------- stubs


class StubSupervisor:
    """The supervisor surface `DiscordRemote` uses, with every relay recorded and one
    scriptable refusal (`fail`) so the racing-front-doors path can be exercised."""

    def __init__(self) -> None:
        self.listeners: dict[str, list] = {}
        self.answers: list[dict] = []
        self.calls: list[dict] = []
        self.sessions_map: dict[str, SimpleNamespace] = {"sess-1": SimpleNamespace(cwd=str(_TMPDIR / "proj"), alive=True)}
        self.fail: str = ""

    def listen(self, topic: str, callback) -> object:
        self.listeners.setdefault(topic, []).append(callback)

        def stop() -> None:
            if callback in self.listeners.get(topic, []):
                self.listeners[topic].remove(callback)

        return stop

    def notify(self, topic: str, payload: dict) -> None:
        for callback in list(self.listeners.get(topic, [])):
            callback(payload)

    def sessions(self) -> list[dict]:
        return [{"sessionId": sid, "cwd": s.cwd, "alive": s.alive} for sid, s in self.sessions_map.items()]

    def session(self, session_id: str):
        return self.sessions_map.get(session_id)

    async def answer_permission(self, session_id: str, key: str, approve: bool, *, decided_by: str = "gui"):
        if self.fail == "lookup":
            raise LookupError(f"no pending permission {key!r}")
        if self.fail == "value":
            raise ValueError("the session offered no allow_once option")
        self.answers.append({"session_id": session_id, "key": key, "approve": approve, "decided_by": decided_by})
        return SimpleNamespace(session_id=session_id, key=key)

    async def call(self, session_id: str, method: str, params: dict | None = None, *, timeout: float = 5.0) -> dict:
        self.calls.append({"session_id": session_id, "method": method, "params": params or {}})
        return {"result": {"queued": 2}}


def pending(score: float = 0.9, *, kind: str = "shell", tool: str = "shell", body: str = "rm -rf build/", session_id: str = "sess-1") -> SimpleNamespace:
    meta = {
        "interrupt_id": "7",
        "pod": "debugger",
        "tool": tool,
        "kind": kind,
        "command": body if kind == "shell" else "",
        "diff": body if kind == "diff" else "",
        "input_json": json.dumps({"command": body}),
        "risk_score": score,
        "risk_band": "destructive" if score >= 0.8 else "elevated",
        "armed": True,
        "reason": "removes files",
        "redactions": 0,
    }
    return SimpleNamespace(session_id=session_id, key="7", meta=meta, title=f"Run {tool}", raw_input={"command": body})


class Rig:
    """One configured, connected bot with the fake client's channel wired up."""

    def __init__(self, supervisor: StubSupervisor, remote: "bot.DiscordRemote", channel: fake_discord.Channel) -> None:
        self.supervisor = supervisor
        self.remote = remote
        self.channel = channel

    @property
    def client(self) -> fake_discord.Client:
        return self.remote._client


async def boot(*, environ: dict | None = None, token: str | None = TOKEN, channel_id: int = CHANNEL_ID) -> Rig:
    supervisor = StubSupervisor()
    remote = bot.DiscordRemote(supervisor, environ=ENV if environ is None else environ, token=token)
    channel = fake_discord.Channel(channel_id)
    assert remote.start() is not None, "the test rig's environment is a complete one"
    # The task `start()` scheduled has not run yet (no await since), so the channel is in place
    # before `on_ready` looks for it.
    remote._client.channels[channel_id] = channel
    await settle()
    return Rig(supervisor, remote, channel)


async def settle(times: int = 4) -> None:
    """Let the scheduled listener tasks run. The listeners themselves must not await (a raising
    callback is swallowed by `Supervisor._notify`), so everything they do is one hop away."""
    for _ in range(times):
        await asyncio.sleep(0)


# --------------------------------------------------------------------------- config


def test_config_fails_closed() -> None:
    no_token = bot.RemoteConfig.from_env({}, token="")
    check(not no_token.enabled and "DISCORD_BOT_TOKEN" in no_token.reason, "no token: the bot does not start, and says which name is missing")

    no_operator = bot.RemoteConfig.from_env({"TELEMETRY_CHANNEL_ID": "1", "DISCORD_BOT_TOKEN": "x" * 20})
    check(not no_operator.enabled and "ALLOWED_USER_ID" in no_operator.reason, "no allowlist: an open front door is refused outright")

    bad_id = bot.RemoteConfig.from_env({"DISCORD_BOT_TOKEN": "x" * 20, "ALLOWED_USER_ID": "me", "TELEMETRY_CHANNEL_ID": "1"})
    check(not bad_id.enabled and "not an integer" in bad_id.reason, "an unparsable id is refused, never guessed at")

    no_channel = bot.RemoteConfig.from_env({"DISCORD_BOT_TOKEN": "x" * 20, "ALLOWED_USER_ID": "1"})
    check(not no_channel.enabled and "TELEMETRY_CHANNEL_ID" in no_channel.reason, "no channel: nowhere to post is a reason not to start")

    killed = bot.RemoteConfig.from_env({**ENV, "SIERRALAB_DISCORD": "off"})
    check(not killed.enabled, "SIERRALAB_DISCORD=off stops the bot without deleting the token")

    ok = bot.RemoteConfig.from_env(ENV)
    check(ok.enabled and ok.allowed_user_id == OPERATOR and ok.channel_id == CHANNEL_ID, "a complete environment enables it, with both ids as ints")
    check(secret_guard.redact(f"token is {TOKEN}") == f"token is {secret_guard.REDACTED}", "the token is registered for masking the moment it is read")


async def test_start_is_a_no_op_without_a_token() -> None:
    supervisor = StubSupervisor()
    remote = bot.DiscordRemote(supervisor, environ={}, token="")
    check(remote.start() is None, "an unconfigured bot returns no task at all")
    check(not remote.status()["enabled"] and remote.status()["allowedUser"] is None, "and `/health` says so rather than claiming a connection")
    check(not remote._unsubscribes, "and it never subscribed to the supervisor, so nothing is posted")


async def test_startup_failure_does_not_raise() -> None:
    fake_discord.FAIL_NEXT_START = fake_discord.PrivilegedIntentsRequired("message content intent is not enabled")
    supervisor = StubSupervisor()
    remote = bot.DiscordRemote(supervisor, environ=ENV, token=TOKEN)
    remote.start()
    await settle()
    check(not remote.status()["connected"], "a privileged-intent refusal leaves the bot disconnected, not the daemon down")
    await remote.stop()


# --------------------------------------------------------------------------- HITL


async def test_destructive_pending_posts_an_embed_and_two_buttons() -> None:
    rig = await boot()
    check(rig.remote.status()["connected"], "the fake client's on_ready marks the bot connected")
    check(any("Daemon Connected" in text for text in rig.channel.contents()), "connecting announces itself in the telemetry channel")

    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.93)})
    await settle()
    posted = [entry for entry in rig.channel.sent if entry["view"] is not None]
    check(len(posted) == 1, "a pending at or above the destructive line is posted exactly once")
    embed, view = posted[0]["embed"], posted[0]["view"]
    check("shell" in embed.title and "0.93" in embed.title, "the title carries the agent's tool and Laya's score")
    check("```sh" in embed.text and "rm -rf build/" in embed.text, "the command is in a shell-fenced block")
    check("debugger" in embed.text, "the pod and session are named, so a person knows which agent paused")
    check(view.timeout is None, "the buttons do not expire: a late decision is still the right one")

    buttons = {name: getattr(type(view), name).button for name in ("approve", "reject")}
    check(buttons["approve"]["label"] == bot.APPROVE_LABEL and buttons["approve"]["style"] == fake_discord.ButtonStyle.success, "the approve button is the success pair the directive asked for")
    check(buttons["reject"]["label"] == bot.REJECT_LABEL and buttons["reject"]["style"] == fake_discord.ButtonStyle.danger, "the reject button is the danger pair")
    await rig.remote.stop()


async def test_quiet_pendings_are_not_posted() -> None:
    rig = await boot()
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.3)})
    await settle()
    check(not [entry for entry in rig.channel.sent if entry["view"] is not None], "a sub-threshold ask stays in the GUI's tray, out of the channel")

    unreadable = pending(0.9)
    unreadable.meta["risk_score"] = None
    rig.supervisor.notify("intervention", {"event": "requested", "pending": unreadable})
    await settle()
    check(not [entry for entry in rig.channel.sent if entry["view"] is not None], "a pending with no score is not guessed at")

    rig.supervisor.notify("intervention", {"event": "resolved", "pending": pending(0.9), "outcome": "approved"})
    await settle()
    check(not [entry for entry in rig.channel.sent if entry["view"] is not None], "a resolution is not a new prompt")
    await rig.remote.stop()


async def test_threshold_follows_the_project() -> None:
    """`laya.riskDestroy` is per project (settings_store), so the line the bot applies is the line
    the gate applied for that session's folder — not a constant in this module."""
    home = Path(os.environ["SIERRALAB_HOME"])
    home.mkdir(parents=True, exist_ok=True)
    (home / "settings.json").write_text(json.dumps({"laya.riskDestroy": 0.5}), encoding="utf-8")
    try:
        check(bot.destructive_line(str(_TMPDIR / "proj")) == 0.5, "the project's own destructive line is read, not the default")
        rig = await boot()
        rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.6)})
        await settle()
        check(len([entry for entry in rig.channel.sent if entry["view"] is not None]) == 1, "a 0.60 call is posted in a project whose line is 0.50")
        await rig.remote.stop()
    finally:
        (home / "settings.json").unlink()


async def test_a_stranger_click_is_not_a_decision() -> None:
    rig = await boot()
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.95)})
    await settle()
    view = [entry for entry in rig.channel.sent if entry["view"] is not None][0]["view"]

    stranger = fake_discord.Interaction(OTHER)
    await view.approve(stranger, None)
    check(stranger.response.messages and stranger.response.messages[0]["ephemeral"], "an unauthorized click gets an ephemeral refusal")
    check("only accepts decisions" in stranger.response.messages[0]["content"], "and is told why, not left guessing")
    check(not rig.supervisor.answers, "and the daemon never hears about it: a stranger's click is not a decision")
    check(not stranger.response.edits, "and the prompt is left standing for the person it is waiting on")
    await rig.remote.stop()


async def test_the_operator_unblocks_the_agent() -> None:
    rig = await boot()
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.95)})
    await settle()
    view = [entry for entry in rig.channel.sent if entry["view"] is not None][0]["view"]

    operator = fake_discord.Interaction(OPERATOR)
    await view.approve(operator, None)
    check(len(rig.supervisor.answers) == 1 and rig.supervisor.answers[0]["approve"], "an authorized approve relays into the paused request")
    check(rig.supervisor.answers[0]["decided_by"] == "discord", "and the decision is attributed to the front door that made it")
    check(operator.response.edits and operator.response.edits[0]["view"] is None, "the message is closed out, so nobody answers it twice")

    reject = fake_discord.Interaction(OPERATOR)
    await view.reject(reject, None)
    check(len(rig.supervisor.answers) == 2 and rig.supervisor.answers[1]["approve"] is False, "reject relays a refusal, not an approval")


async def test_a_prompt_answered_elsewhere_is_not_answered_twice() -> None:
    rig = await boot()
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.95)})
    await settle()
    view = [entry for entry in rig.channel.sent if entry["view"] is not None][0]["view"]

    rig.supervisor.fail = "lookup"  # the GUI answered first: `_settle` no longer knows this key
    late = fake_discord.Interaction(OPERATOR)
    await view.approve(late, None)
    check(late.response.messages and "Already decided" in late.response.messages[0]["content"], "a late click is told it is moot")
    check(not rig.supervisor.answers, "and sends nothing to the child that already moved on")

    rig.supervisor.fail = "value"
    broken = fake_discord.Interaction(OPERATOR)
    await view.approve(broken, None)
    check(broken.response.messages and "did not offer" in broken.response.messages[0]["content"], "a session that offered no matching option is an error message, not a crash")
    rig.supervisor.fail = ""
    await rig.remote.stop()


async def test_secrets_in_the_view_are_masked() -> None:
    secret_guard.register_secret("correct-horse-battery-staple")
    rig = await boot()
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.9, body="export KEY=correct-horse-battery-staple; rm -rf /")})
    await settle()
    embed = [entry for entry in rig.channel.sent if entry["view"] is not None][0]["embed"]
    check("correct-horse-battery-staple" not in embed.text, "an unregistered credential-shaped value never reaches the channel")
    check(secret_guard.REDACTED in embed.text, "the fenced command is masked on the way out")
    await rig.remote.stop()


async def test_a_long_diff_is_truncated_visibly() -> None:
    rig = await boot()
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.9, kind="diff", tool="editor", body="+" + "x" * 4000)})
    await settle()
    embed = [entry for entry in rig.channel.sent if entry["view"] is not None][0]["embed"]
    check("```diff" in embed.text, "a diff gets the diff fence")
    check("(truncated" in embed.text, "a clipped diff says it was clipped, rather than looking complete")
    check(len(embed.field("To run · diff")) < 1024, "and the field stays inside Discord's own limit")
    await rig.remote.stop()


# --------------------------------------------------------------------------- threads and steering


async def test_workflow_tasks_open_one_thread_each() -> None:
    rig = await boot()
    rig.supervisor.notify("task", {"session_id": "sess-1", "source": "team", "state": "running", "title": "team: architect → debugger"})
    await settle()
    check(len(rig.channel.threads) == 1, "a multi-step workflow opens exactly one thread")
    thread = rig.channel.threads[0]
    check(thread.name.startswith("🧵 Task:") and "architect" in thread.name, "the thread is named after the task")
    check(any("Tracking `sess-1`" in text for text in thread.contents()), "and says which session it is tracking")
    check(thread.sent[-1]["content"].startswith("•"), "the step that opened it is the first thing in it")

    rig.supervisor.notify("task", {"session_id": "sess-1", "source": "memory", "state": "running", "title": "ollama pull qwen"})
    await settle()
    check(len(rig.channel.threads) == 1, "further steps reuse the session's thread instead of opening another")
    check(len(thread.sent) == 2, "later steps land in the thread")

    rig.supervisor.notify("laya", {"session_id": "sess-1", "route": "chat", "risk": {"band": "destructive"}})
    rig.supervisor.notify("laya", {"session_id": "sess-1", "route": "chat", "risk": {"band": "safe"}})
    await settle()
    check(len(thread.sent) == 3 and "◆ laya" in thread.sent[-1]["content"], "only a destructive Laya reading is worth a line in the thread")

    rig.supervisor.notify("task", {"session_id": "sess-1", "source": "", "state": "running", "title": "chat turn"})
    await settle()
    check(len(thread.sent) == 3, "a chat turn's task events stay out of Discord entirely")

    rig.supervisor.notify("task", {"session_id": "sess-2", "source": "heal", "state": "running", "title": "self-heal: attempt 1"})
    await settle()
    check(len(rig.channel.threads) == 2, "a second session gets its own thread")
    await rig.remote.stop()


async def test_task_storms_are_coalesced() -> None:
    rig = await boot()
    rig.supervisor.notify("task", {"session_id": "sess-1", "source": "team", "state": "running", "title": "step"})
    await settle()
    thread = rig.channel.threads[0]
    for _ in range(20):
        rig.supervisor.notify("task", {"session_id": "sess-1", "source": "team", "state": "running", "title": "step", "message": f"{_}"})
    await settle(times=30)
    check(len(thread.sent) < 20, "a 20-update storm does not become 20 messages")
    rig.supervisor.notify("task", {"session_id": "sess-1", "source": "team", "state": "completed", "title": "step"})
    await settle()
    check(thread.sent[-1]["content"].startswith("✔"), "a terminal state always goes through, and says what was folded in")
    await rig.remote.stop()


async def test_steering_takes_the_lab_path() -> None:
    rig = await boot()
    rig.supervisor.notify("task", {"session_id": "sess-1", "source": "team", "state": "running", "title": "team"})
    await settle()
    thread = rig.channel.threads[0]

    await rig.client.dispatch("on_message", fake_discord.Message("> steer: revert that last loop", author_id=OPERATOR, channel=thread))
    check(len(rig.supervisor.calls) == 1, "an authorized directive is relayed")
    call = rig.supervisor.calls[0]
    check(call["method"] == "_sierralab/lab/inject" and call["params"]["pod"] == "orchestrator", "through the Lab's own guidance path, not a second mechanism")
    check(call["params"]["text"] == "revert that last loop", "with the `> steer:` prefix stripped")
    ack = thread.sent[-1]["content"]
    check("chars queued" in ack and "revert that last loop" not in ack, "acknowledged by length: the thread is not a second copy of the prompt")

    await rig.client.dispatch("on_message", fake_discord.Message("and skip the migration", author_id=OPERATOR, channel=thread))
    check(rig.supervisor.calls[1]["params"]["text"] == "and skip the migration", "a plain reply in a task thread is a directive too")

    await rig.client.dispatch("on_message", fake_discord.Message("x" * 5000, author_id=OPERATOR, channel=thread))
    check(len(rig.supervisor.calls[2]["params"]["text"]) == bot.MAX_STEER_CHARS, "an over-long directive is capped to what the Lab will accept")

    before = len(rig.supervisor.calls)
    await rig.client.dispatch("on_message", fake_discord.Message("> steer: do something else", author_id=OTHER, channel=thread))
    await rig.client.dispatch("on_message", fake_discord.Message("> steer: do something else", author_id=OPERATOR, channel=thread, bot=True))
    await rig.client.dispatch("on_message", fake_discord.Message("   ", author_id=OPERATOR, channel=thread))
    check(len(rig.supervisor.calls) == before, "a stranger, a bot, and an empty directive all steer nothing")

    check(not thread.sent[-1]["content"].startswith("Could not"), "the last thing said was not an error")


async def test_telemetry_channel_steers_only_when_unambiguous() -> None:
    rig = await boot()
    await rig.client.dispatch("on_message", fake_discord.Message("> steer: focus on the tests", author_id=OPERATOR, channel=rig.channel))
    check(len(rig.supervisor.calls) == 1 and rig.supervisor.calls[0]["session_id"] == "sess-1", "with one live session, the channel can steer it")

    rig.supervisor.sessions_map["sess-2"] = SimpleNamespace(cwd=str(_TMPDIR / "other"), alive=True)
    await rig.client.dispatch("on_message", fake_discord.Message("> steer: focus on the tests", author_id=OPERATOR, channel=rig.channel))
    check(len(rig.supervisor.calls) == 1, "with two, the channel refuses to guess which one was meant")
    await rig.remote.stop()


async def test_shutdown_unsubscribes_first() -> None:
    rig = await boot()
    await rig.remote.stop()
    check(not rig.remote._unsubscribes, "stop() unsubscribes from the supervisor before closing the client")
    check(not rig.supervisor.listeners.get("intervention"), "so a prompt posted during shutdown cannot arrive afterwards")
    check(rig.client.closed and not rig.remote.status()["connected"], "and the client is closed")
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.99)})
    await settle()
    check(not [entry for entry in rig.channel.sent if entry["view"] is not None], "nothing is posted after a stop")


async def main() -> None:
    test_config_fails_closed()
    await test_start_is_a_no_op_without_a_token()
    await test_startup_failure_does_not_raise()
    await test_destructive_pending_posts_an_embed_and_two_buttons()
    await test_quiet_pendings_are_not_posted()
    await test_threshold_follows_the_project()
    await test_a_stranger_click_is_not_a_decision()
    await test_the_operator_unblocks_the_agent()
    await test_a_prompt_answered_elsewhere_is_not_answered_twice()
    await test_secrets_in_the_view_are_masked()
    await test_a_long_diff_is_truncated_visibly()
    await test_workflow_tasks_open_one_thread_each()
    await test_task_storms_are_coalesced()
    await test_steering_takes_the_lab_path()
    await test_telemetry_channel_steers_only_when_unambiguous()
    await test_shutdown_unsubscribes_first()
    print("\nPASS: the Discord front door stays inert without a token, refuses everyone but the operator, relays approvals through the daemon's own arbitration, and steers through the Lab")


if __name__ == "__main__":
    asyncio.run(main())
