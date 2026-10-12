"""Regression check for the Discord front door (daemon/discord_bot.py, chat.py, discord_cards.py).

What it holds the line on, in the order a person meets it:

1. **Fail closed.** No token, no `ALLOWED_USER_ID`, an id that is not an id, or `CORDATA_DISCORD=off`
   all leave the bot inert — and every one says why.
2. **Zero trust on the caller.** A click or a message from anyone but the operator changes nothing.
3. **You can talk to an agent.** A message in the channel opens a thread and the agent *answers* — a
   real prompt turn, streamed, with a receipt (time, tool calls, tokens). A reply continues the same
   conversation; a message mid-turn steers it and is acknowledged with a reaction, not a paragraph.
   "What are you working on?" is answered from daemon state without a model turn.
4. **A prompt makes sense.** An approval card says what, where and why before how risky, has honest
   buttons, pings only when something is destructive, offers "Allow routine steps in this chat" only
   where it can help, and collapses to one line when answered — by Discord or by another front door.
5. **It does not spam.** One "Daemon connected" per run, not per reconnect; sub-threshold asks in a
   terminal session stay in the GUI's tray; task threads open only for real workflows.
6. **It cannot hurt.** An agent's reply cannot ping anyone and is secret-masked; a card that cannot be
   delivered to a chat is denied, not parked; an idle child is reaped and the conversation resumes from
   disk; the number of live agent processes is capped.

Hermetic: `fake_discord` stands in for the library; the supervisor is either a stub (the click/route
properties) or the real `Supervisor` driving `fixtures/fake_acp_chat_child.py` (the conversation
properties — a subprocess speaking the ACP wire, no model). Nothing touches the network.

    backend/.venv/bin/python _discord_remote_smoke_test.py
"""

import asyncio
import json
import os
import sys
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace

_TMP = tempfile.TemporaryDirectory()
_TMPDIR = Path(_TMP.name).resolve()  # macOS: /var is a symlink to /private/var, and the daemon resolves folders
_PROJECT = _TMPDIR / "proj"
_PROJECT.mkdir()
os.environ["CORDATA_HOME"] = str(_TMPDIR / "home")
os.environ["CORDATA_STATE_DB"] = str(_TMPDIR / "state.db")
os.environ["CORDATA_DISCORD_CWD"] = str(_PROJECT)
os.environ["FAKE_ACP_STATE"] = str(_TMPDIR / "fake_acp_state.json")
HERE = Path(__file__).resolve().parent
os.environ["CORDATA_ACP_ENTRYPOINT"] = f"{sys.executable} {HERE / 'fixtures' / 'fake_acp_chat_child.py'}"
sys.path.insert(0, str(HERE))

import fake_discord  # noqa: E402
import secret_guard  # noqa: E402

sys.modules["discord"] = fake_discord  # before discord_bot's lazy `import discord`

import daemon.discord_bot as bot  # noqa: E402
from daemon import discord_cards as cards  # noqa: E402
from daemon.chat import Chats, ChatError  # noqa: E402
from daemon.supervisor import Supervisor  # noqa: E402

bot.EDIT_INTERVAL = 0.05  # a live reply's edit throttle: real in production, instant here
REAL_SCRATCH_ROOTS = bot.SCRATCH_ROOTS
bot.SCRATCH_ROOTS = ()  # every path in this test lives under a temp dir; `test_scratch_sessions_are_labelled_not_paged` puts the real list back

OPERATOR = 424242
OTHER = 999999
CHANNEL_ID = 555001
TOKEN = "MTIzNDU2Nzg5.fake-token-abcdefghijklmnop"
ENV = {"DISCORD_BOT_TOKEN": TOKEN, "ALLOWED_USER_ID": str(OPERATOR), "TELEMETRY_CHANNEL_ID": str(CHANNEL_ID)}


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


async def until(predicate, timeout: float = 20.0, step: float = 0.02) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        await asyncio.sleep(step)
    return False


async def settle(times: int = 4) -> None:
    """Let the scheduled listener tasks run. The listeners themselves must not await (a raising
    callback is swallowed by `Supervisor._notify`), so everything they do is one hop away."""
    for _ in range(times):
        await asyncio.sleep(0)


# --------------------------------------------------------------------------- stubs


class StubSupervisor:
    """The supervisor surface `DiscordRemote` uses, with every relay recorded and one scriptable
    refusal (`fail`) so the racing-front-doors path can be exercised."""

    def __init__(self) -> None:
        self.listeners: dict[str, list] = {}
        self.answers: list[dict] = []
        self.calls: list[dict] = []
        self.sessions_map: dict[str, SimpleNamespace] = {"sess-1": SimpleNamespace(cwd=str(_PROJECT), alive=True, pending={}, owner=None)}
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
        return [{"sessionId": sid, "cwd": s.cwd, "alive": s.alive, "owner": s.owner, "started": 1.0} for sid, s in self.sessions_map.items()]

    def session(self, session_id: str):
        return self.sessions_map.get(session_id)

    def lab_states(self) -> dict:
        return {}

    async def answer_permission(self, session_id: str, key: str, approve: bool, *, decided_by: str = "gui", always: bool = False):
        if self.fail == "lookup":
            raise LookupError(f"no pending permission {key!r}")
        if self.fail == "value":
            raise ValueError("the session offered no allow_once option")
        self.answers.append({"session_id": session_id, "key": key, "approve": approve, "decided_by": decided_by, "always": always})
        return SimpleNamespace(session_id=session_id, key=key)

    async def call(self, session_id: str, method: str, params: dict | None = None, *, timeout: float = 5.0, qualify: bool = True) -> dict:
        self.calls.append({"session_id": session_id, "method": method, "params": params or {}})
        return {"result": {"queued": 2}}


def pending(
    score: float = 0.9, *, kind: str = "shell", tool: str = "shell", body: str = "rm -rf build/", session_id: str = "sess-1", armed: bool = True, options: list | None = None
) -> SimpleNamespace:
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
        "armed": armed,
        "reason": "Laya Guard: removes files — risk 0.90 is at or above this agent's gate of 0.80",
        "goal": "clean the build directory",
        "said": "I'll remove the stale output first.",
        "project": str(_PROJECT),
        "redactions": 0,
    }
    return SimpleNamespace(
        session_id=session_id, key="7", meta=meta, title=f"Run {tool}", raw_input={"command": body},
        options=options or [{"optionId": "allow", "kind": "allow_once"}, {"optionId": "reject", "kind": "reject_once"}],
    )


class Rig:
    """One configured, connected bot with the fake client's channel wired up."""

    def __init__(self, supervisor, remote: "bot.DiscordRemote", channel: fake_discord.Channel) -> None:
        self.supervisor = supervisor
        self.remote = remote
        self.channel = channel

    @property
    def client(self) -> fake_discord.Client:
        return self.remote._client

    def say(self, text: str, *, channel=None, author: int = OPERATOR, bot_flag: bool = False) -> fake_discord.Message:
        return fake_discord.Message(text, author_id=author, channel=channel or self.channel, bot=bot_flag)

    async def send(self, text: str, **kwargs) -> fake_discord.Message:
        message = self.say(text, **kwargs)
        await self.client.dispatch("on_message", message)
        return message


async def boot(*, environ: dict | None = None, token: str | None = TOKEN, channel_id: int = CHANNEL_ID, supervisor=None, chats: Chats | None = None) -> Rig:
    supervisor = supervisor if supervisor is not None else StubSupervisor()
    remote = bot.DiscordRemote(supervisor, environ=ENV if environ is None else environ, token=token, chats=chats)
    channel = fake_discord.Channel(channel_id)
    assert remote.start() is not None, "the test rig's environment is a complete one"
    # The task `start()` scheduled has not run yet (no await since), so the channel is in place
    # before `on_ready` looks for it.
    remote._client.channels[channel_id] = channel
    await settle()
    return Rig(supervisor, remote, channel)


async def boot_real(*, idle_seconds: float = 1200.0, max_live: int = 3, store: str = "chats.json") -> Rig:
    """The real `Supervisor`, driving the scripted ACP child."""
    supervisor = Supervisor()
    chats = Chats(supervisor, store=_TMPDIR / store, idle_seconds=idle_seconds, max_live=max_live)
    return await boot(supervisor=supervisor, chats=chats)


async def teardown(rig: Rig) -> None:
    await rig.remote.stop()
    if isinstance(rig.supervisor, Supervisor):
        await rig.supervisor.stop_all()


def approvals(channel: fake_discord.Channel) -> list[fake_discord.SentMessage]:
    """Cards that still have their buttons."""
    return [m for m in channel.messages if m.view is not None]


def cards_in(channel: fake_discord.Channel) -> list[fake_discord.SentMessage]:
    """Every approval card ever posted here, live or collapsed — they are the embeds with a verdict or a question."""
    return [m for m in channel.messages if m.embed is not None and ("Approve this step?" in m.embed.title or "Needs your decision" in m.embed.title or "Allowed" in m.embed.title or "Denied" in m.embed.title)]


def said(channel: fake_discord.Channel, needle: str) -> bool:
    """Did any message here read like this *now* (edits applied)? A turn's reply is edited in place, so the
    newest message is not necessarily the reply once a card or a status line has been posted after it."""
    return any(needle in (m.content or "") for m in channel.messages)


# =========================================================================== config and lifecycle


def test_config_fails_closed() -> None:
    no_token = bot.RemoteConfig.from_env({}, token="")
    check(not no_token.enabled and "DISCORD_BOT_TOKEN" in no_token.reason, "no token: the bot does not start, and says which name is missing")

    no_operator = bot.RemoteConfig.from_env({"TELEMETRY_CHANNEL_ID": "1", "DISCORD_BOT_TOKEN": "x" * 20})
    check(not no_operator.enabled and "ALLOWED_USER_ID" in no_operator.reason, "no allowlist: an open front door is refused outright")

    bad_id = bot.RemoteConfig.from_env({"DISCORD_BOT_TOKEN": "x" * 20, "ALLOWED_USER_ID": "me", "TELEMETRY_CHANNEL_ID": "1"})
    check(not bad_id.enabled and "not an integer" in bad_id.reason, "an unparsable id is refused, never guessed at")

    no_channel = bot.RemoteConfig.from_env({"DISCORD_BOT_TOKEN": "x" * 20, "ALLOWED_USER_ID": "1"})
    check(not no_channel.enabled and "TELEMETRY_CHANNEL_ID" in no_channel.reason, "no channel: nowhere to post is a reason not to start")

    killed = bot.RemoteConfig.from_env({**ENV, "CORDATA_DISCORD": "off"})
    check(not killed.enabled, "CORDATA_DISCORD=off stops the bot without deleting the token")

    ok = bot.RemoteConfig.from_env(ENV)
    check(ok.enabled and ok.allowed_user_id == OPERATOR and ok.channel_id == CHANNEL_ID, "a complete environment enables it, with both ids as ints")
    check(secret_guard.redact(f"token is {TOKEN}") == f"token is {secret_guard.REDACTED}", "the token is registered for masking the moment it is read")


async def test_start_is_a_no_op_without_a_token() -> None:
    supervisor = StubSupervisor()
    remote = bot.DiscordRemote(supervisor, environ={}, token="")
    check(remote.start() is None, "an unconfigured bot returns no task at all")
    check(not remote.status()["enabled"] and remote.status()["allowedUser"] is None, "and `/health` says so rather than claiming a connection")
    check(not remote._unsubscribes and not supervisor.listeners, "and it never subscribed to the supervisor, so nothing is posted")
    check(remote._chats is None, "and no chat store was read or created")


async def test_startup_failure_does_not_raise() -> None:
    fake_discord.FAIL_NEXT_START = fake_discord.PrivilegedIntentsRequired("message content intent is not enabled")
    supervisor = StubSupervisor()
    remote = bot.DiscordRemote(supervisor, environ=ENV, token=TOKEN)
    remote.start()
    await settle()
    check(not remote.status()["connected"], "a privileged-intent refusal leaves the bot disconnected, not the daemon down")
    await remote.stop()


async def test_a_reconnect_edits_the_announcement_instead_of_reposting() -> None:
    rig = await boot()
    sent = [m for m in rig.channel.messages if m.embed is not None and "Daemon connected" in m.embed.title]
    check(len(sent) == 1, "connecting announces itself once")
    check("!help" in sent[0].embed.description and "write here" in sent[0].embed.description, "and says how to use it, not just that it is up")
    await rig.client.handlers["on_ready"]()  # the gateway reconnected
    await rig.client.handlers["on_ready"]()
    again = [m for m in rig.channel.messages if m.embed is not None and "Daemon connected" in m.embed.title]
    check(len(again) == 1 and len(again[0].edits) == 2, "two reconnects edit that message — a flaky connection is not channel spam")
    await rig.remote.stop()


async def test_shutdown_unsubscribes_first() -> None:
    rig = await boot()
    await rig.remote.stop()
    check(not rig.remote._unsubscribes, "stop() unsubscribes from the supervisor before closing the client")
    check(not rig.supervisor.listeners.get("intervention"), "so a prompt posted during shutdown cannot arrive afterwards")
    check(rig.client.closed and not rig.remote.status()["connected"], "and the client is closed")
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.99)})
    await settle()
    check(not approvals(rig.channel), "nothing is posted after a stop")


# =========================================================================== approval cards (stub supervisor)


async def test_an_armed_card_says_what_where_and_why() -> None:
    rig = await boot()
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.93)})
    await settle()
    posted = approvals(rig.channel)
    check(len(posted) == 1, "a pending at or above the destructive line is posted exactly once")
    card = posted[0]
    text = card.embed.text
    check("The **debugger** agent wants to run a shell command in **proj**" in text, "it opens with who, what and where — in words, not a score")
    check("You asked:" in text and "clean the build directory" in text, "it says what the person asked, so the call has a reason")
    check("It said:" in text and "stale output" in text, "and what the agent itself said just before, so the call makes sense")
    check("```sh" in text and "rm -rf build/" in text, "the command is in a shell-fenced block")
    check("removes files" in card.embed.field("Why it stopped") and "risk 0.90" not in card.embed.field("Why it stopped"), "Laya's reason is shown without its own arithmetic")
    check("destructive" in card.embed.field("Laya's reading") and "0.93" in card.embed.field("Laya's reading"), "the score is evidence, in its own field")
    check("Allow" in card.embed.footer and "Deny" in card.embed.footer, "the footer says what each button does")
    check("Reject & Isolate" not in text and "Approve & Execute" not in text, "no button promises a containment it does not perform")
    check(card.view.timeout is None, "the buttons do not expire: a late decision is still the right one")
    check(card.view.labels() == [bot.APPROVE_ONCE_LABEL, bot.DENY_LABEL], "an armed call offers Allow once and Deny — no always-allow, no 'allow routine'")
    check(card.view.button(bot.APPROVE_ONCE_LABEL).style == fake_discord.ButtonStyle.success and card.view.button(bot.DENY_LABEL).style == fake_discord.ButtonStyle.danger, "green allow, red deny")
    check(card.content and f"<@{OPERATOR}>" in card.content and card.allowed_mentions.users, "a destructive ask pings the operator — and only the operator")
    await rig.remote.stop()


async def test_scratch_sessions_are_labelled_not_paged() -> None:
    """A session in a temp folder is nearly always a test run; its prompts were paging the operator about a
    folder they had never heard of. Still posted — a parked call nobody was told about is worse — but never a ping."""
    check(not bot.is_scratch(str(_PROJECT)), "with no scratch roots configured, nothing is scratch (this test's own temp dirs)")
    bot.SCRATCH_ROOTS = REAL_SCRATCH_ROOTS
    try:
        check(bot.is_scratch("/private/tmp/gate-scratch") and bot.is_scratch("/tmp/x/y") and bot.is_scratch("/var/folders/ab/cd/T/t"), "/tmp, /private/tmp and /var/folders are scratch")
        check(not bot.is_scratch("/Users/me/code/app") and not bot.is_scratch("/private/tmpfoo") and not bot.is_scratch(None), "a real project, a look-alike prefix and no folder at all are not")
        rig = await boot()
        item = pending(0.99)
        item.meta["project"] = "/private/tmp/gate-scratch"
        rig.supervisor.notify("intervention", {"event": "requested", "pending": item})
        await settle()
        card = approvals(rig.channel)[0]
        check("temporary folder" in card.embed.description and card.embed.title.startswith("🧪"), "the card says it is a scratch session")
        check(card.content is None and not (card.allowed_mentions and card.allowed_mentions.pings_anyone), "and does not ping")
        check(card.view.labels() == [bot.APPROVE_ONCE_LABEL, bot.DENY_LABEL], "but is still answerable — nothing is parked unseen")
        await rig.remote.stop()
    finally:
        bot.SCRATCH_ROOTS = ()


async def test_quiet_pendings_are_not_posted() -> None:
    rig = await boot()
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.3, armed=False)})
    await settle()
    check(not approvals(rig.channel), "a sub-threshold ask in a terminal session stays in the GUI's tray, out of the channel")

    unreadable = pending(0.9)
    unreadable.meta["risk_score"] = None
    rig.supervisor.notify("intervention", {"event": "requested", "pending": unreadable})
    await settle()
    check(not approvals(rig.channel), "a pending with no score is not guessed at")
    await rig.remote.stop()


async def test_threshold_follows_the_project() -> None:
    """`laya.riskDestroy` is per project (settings_store), so the line the bot applies is the line
    the gate applied for that session's folder — not a constant in this module."""
    home = Path(os.environ["CORDATA_HOME"])
    home.mkdir(parents=True, exist_ok=True)
    (home / "settings.json").write_text(json.dumps({"laya.riskDestroy": 0.5}), encoding="utf-8")
    try:
        check(bot.destructive_line(str(_PROJECT)) == 0.5, "the project's own destructive line is read, not the default")
        rig = await boot()
        rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.6)})
        await settle()
        check(len(approvals(rig.channel)) == 1, "a 0.60 call is posted in a project whose line is 0.50")
        await rig.remote.stop()
    finally:
        (home / "settings.json").unlink()


async def test_a_stranger_click_is_not_a_decision() -> None:
    rig = await boot()
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.95)})
    await settle()
    view = approvals(rig.channel)[0].view

    stranger = fake_discord.Interaction(OTHER)
    await view.press(bot.APPROVE_ONCE_LABEL, stranger)
    check(stranger.response.messages and stranger.response.messages[0]["ephemeral"], "an unauthorized click gets an ephemeral refusal")
    check("only accepts decisions" in stranger.response.messages[0]["content"], "and is told why, not left guessing")
    check(not rig.supervisor.answers, "and the daemon never hears about it: a stranger's click is not a decision")
    check(not stranger.response.edits, "and the prompt is left standing for the person it is waiting on")
    await rig.remote.stop()


async def test_the_operator_unblocks_the_agent_and_the_card_collapses() -> None:
    rig = await boot()
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.95)})
    await settle()
    view = approvals(rig.channel)[0].view

    operator = fake_discord.Interaction(OPERATOR)
    await view.press(bot.APPROVE_ONCE_LABEL, operator)
    check(len(rig.supervisor.answers) == 1 and rig.supervisor.answers[0]["approve"], "an authorized allow relays into the paused request")
    check(rig.supervisor.answers[0]["decided_by"] == "discord", "and the decision is attributed to the front door that made it")
    edit = operator.response.edits[0]
    check(edit["view"] is None and edit["content"] is None, "the buttons and the ping are gone, so nobody answers it twice")
    check("Allowed" in edit["embed"].title and "rm -rf build/" in edit["embed"].text and len(edit["embed"].fields) == 1, "it collapses to a one-line record: verdict, call, who and when")

    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.95)})
    await settle()
    reject = fake_discord.Interaction(OPERATOR)
    await approvals(rig.channel)[1].view.press(bot.DENY_LABEL, reject)
    check(len(rig.supervisor.answers) == 2 and rig.supervisor.answers[1]["approve"] is False, "deny relays a refusal, not an approval")
    check("Denied" in reject.response.edits[0]["embed"].title, "and the record says Denied")
    await rig.remote.stop()


async def test_a_prompt_answered_elsewhere_closes_the_card() -> None:
    rig = await boot()
    item = pending(0.95)
    rig.supervisor.notify("intervention", {"event": "requested", "pending": item})
    await settle()
    card = approvals(rig.channel)[0]

    rig.supervisor.notify("intervention", {"event": "resolved", "pending": item, "outcome": "approved", "decided_by": "gui"})
    await settle()
    check(card.view is None and "answered from the GUI" in card.embed.text and "Allowed" in card.embed.title, "when the GUI answers first, the card says so and loses its buttons")

    rig.supervisor.fail = "lookup"
    late = fake_discord.Interaction(OPERATOR)
    stale = pending(0.95)
    stale.key = "8"
    rig.supervisor.notify("intervention", {"event": "requested", "pending": stale})
    await settle()
    await approvals(rig.channel)[-1].view.press(bot.APPROVE_ONCE_LABEL, late)
    check(late.response.messages and "Already decided" in late.response.messages[0]["content"], "a late click on a prompt that was settled meanwhile is told it is moot")
    check(not rig.supervisor.answers, "and sends nothing to the child that already moved on")

    rig.supervisor.fail = "value"
    broken = fake_discord.Interaction(OPERATOR)
    await approvals(rig.channel)[-1].view.press(bot.APPROVE_ONCE_LABEL, broken)
    check(broken.response.messages and "did not offer" in broken.response.messages[0]["content"], "a session that offered no matching option is an error message, not a crash")
    rig.supervisor.fail = ""
    await rig.remote.stop()


async def test_secrets_in_the_view_are_masked() -> None:
    secret_guard.register_secret("correct-horse-battery-staple")
    rig = await boot()
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.9, body="export KEY=correct-horse-battery-staple; rm -rf /")})
    await settle()
    text = approvals(rig.channel)[0].embed.text
    check("correct-horse-battery-staple" not in text, "a registered credential never reaches the channel")
    check(secret_guard.REDACTED in text, "the fenced command is masked on the way out")
    await rig.remote.stop()


async def test_a_long_diff_is_truncated_visibly() -> None:
    rig = await boot()
    diff = "--- a/src/app.py\n+++ b/src/app.py\n@@ -1 +1 @@\n+" + "x" * 4000
    rig.supervisor.notify("intervention", {"event": "requested", "pending": pending(0.9, kind="diff", tool="editor", body=diff)})
    await settle()
    embed = approvals(rig.channel)[0].embed
    check("edit `src/app.py`" in embed.description, "an edit names the file it changes")
    check("```diff" in embed.text, "a diff gets the diff fence")
    check("(truncated" in embed.text, "a clipped diff says it was clipped, rather than looking complete")
    check(len(embed.field("Changes")) < 1024, "and the field stays inside Discord's own limit")
    await rig.remote.stop()


# =========================================================================== task threads and steering (stub supervisor)


async def test_workflow_tasks_open_one_thread_each() -> None:
    rig = await boot()
    rig.supervisor.notify("task", {"session_id": "sess-1", "source": "team", "state": "running", "title": "team: architect → debugger"})
    await settle()
    check(len(rig.channel.threads) == 1, "a multi-step workflow opens exactly one thread")
    thread = rig.channel.threads[0]
    check(thread.name.startswith("🧵") and "architect" in thread.name, "the thread is named after the task")
    check(any("Following **proj**" in text for text in thread.contents()), "and says which project it is following, in words")
    check(thread.messages[-1].content.startswith("•"), "the step that opened it is the first thing in it")

    rig.supervisor.notify("task", {"session_id": "sess-1", "source": "heal", "state": "running", "title": "self-heal: attempt 1"})
    await settle()
    check(len(rig.channel.threads) == 1 and len(thread.messages) == 2, "further steps reuse the session's thread (coalesced into it) instead of opening another")

    for source in ("memory", "model", "plugins", "lsp", ""):
        rig.supervisor.notify("task", {"session_id": "sess-9", "source": source, "state": "running", "title": f"{source} chatter"})
    await settle()
    check(len(rig.channel.threads) == 1, "infrastructure tasks (a model load, an LSP start) never open a thread")

    rig.supervisor.notify("laya", {"session_id": "sess-1", "route": "chat", "risk": {"band": "destructive"}})
    rig.supervisor.notify("laya", {"session_id": "sess-1", "route": "chat", "risk": {"band": "safe"}})
    await settle()
    check(len(thread.messages) == 3 and "◆ laya" in thread.messages[-1].content, "only a destructive Laya reading is worth a line in the thread")
    await rig.remote.stop()


async def test_task_storms_are_coalesced() -> None:
    rig = await boot()
    rig.supervisor.notify("task", {"session_id": "sess-1", "source": "team", "state": "running", "title": "step"})
    await settle()
    thread = rig.channel.threads[0]
    for n in range(20):
        rig.supervisor.notify("task", {"session_id": "sess-1", "source": "team", "state": "running", "title": "step", "message": f"{n}"})
    await settle(times=30)
    check(len(thread.messages) < 20, "a 20-update storm does not become 20 messages")
    rig.supervisor.notify("task", {"session_id": "sess-1", "source": "team", "state": "completed", "title": "step"})
    await settle()
    check(thread.messages[-1].content.startswith("✔"), "a terminal state always goes through, and says what was folded in")
    await rig.remote.stop()


async def test_steering_a_terminal_session_is_quiet_and_honest() -> None:
    rig = await boot()
    rig.supervisor.notify("task", {"session_id": "sess-1", "source": "team", "state": "running", "title": "team"})
    await settle()
    thread = rig.channel.threads[0]
    before = len(thread.messages)

    first = await rig.send("revert that last loop", channel=thread)
    check(len(rig.supervisor.calls) == 1, "an authorized note is relayed")
    call = rig.supervisor.calls[0]
    check(call["method"] == "_cordata/lab/inject" and call["params"]["pod"] == "orchestrator", "through the Lab's own guidance path, not a second mechanism")
    check(call["params"]["text"] == "revert that last loop", "verbatim")
    explained = thread.messages[before].content
    check("next step" in explained and "terminal" in explained and "revert that last loop" not in explained, "the first note is told what steering means — and the note is not echoed back")

    second = await rig.send("> steer: and skip the migration", channel=thread)
    check(rig.supervisor.calls[1]["params"]["text"] == "and skip the migration", "the `> steer:` prefix is stripped")
    check(len(thread.messages) == before + 1 and second.reactions == ["📨"], "later notes are acknowledged with a reaction, not a message each")

    await rig.send("what are you working on?", channel=thread)
    check(len(rig.supervisor.calls) == 2, "a status question in a thread is answered, not sent to the agent as an instruction")
    check("Answered from the daemon's own state" in thread.messages[-1].embed.footer, "and says it cost nothing")

    await rig.send("x" * 5000, channel=thread)
    check(len(rig.supervisor.calls[2]["params"]["text"]) == bot.MAX_STEER_CHARS, "an over-long note is capped to what the Lab will accept")

    before_calls = len(rig.supervisor.calls)
    await rig.send("> steer: do something else", channel=thread, author=OTHER)
    await rig.send("> steer: do something else", channel=thread, bot_flag=True)
    await rig.send("   ", channel=thread)
    await rig.send("> steer:", channel=thread)
    check(len(rig.supervisor.calls) == before_calls, "a stranger, a bot, and an empty note all steer nothing")
    await rig.remote.stop()


async def test_the_channel_steers_only_when_explicit_and_unambiguous() -> None:
    rig = await boot()
    await rig.send("> steer: focus on the tests")
    check(len(rig.supervisor.calls) == 1 and rig.supervisor.calls[0]["session_id"] == "sess-1", "`> steer:` in the channel steers the one running terminal session")

    rig.supervisor.sessions_map["sess-2"] = SimpleNamespace(cwd=str(_TMPDIR / "other"), alive=True, pending={}, owner=None)
    await rig.send("> steer: focus on the tests")
    check(len(rig.supervisor.calls) == 1, "with two, the channel refuses to guess which one was meant")
    check("reply inside the thread" in rig.channel.messages[-1].content, "and says how to choose")
    await rig.remote.stop()


async def test_messages_from_elsewhere_are_ignored() -> None:
    rig = await boot()
    other = fake_discord.Channel(777001, name="general")
    await rig.send("hello there", channel=other)
    await rig.send("hello there", author=OTHER)
    check(not rig.channel.threads and not other.messages and not [m for m in rig.channel.messages if "hello" in m.text], "a stranger, or a channel this bot has no business in, gets nothing")
    await rig.remote.stop()


# =========================================================================== pure helpers


SHOT_REPLY = (
    "I’m a full-stack **coding agent**. Here’s a rundown:\n\n| Category | What I can do |\n|----------|---------------|\n"
    "| **Code & files** | • Edit, view, create (via the `editor` tool).<br>• Read files (`file_read`).<br>• Apply diffs (`apply_patch`). |\n"
    "| **Shell / command execution** | • Run any shell command with timeout. |\n"
    "| **Plain** | Just a sentence, no bullet |\n\nFeel free to ask:\n\n- Run a command\n\n---\n\nDone!"
)


def test_agent_markdown_is_made_discord_safe() -> None:
    """The first live reply was a GitHub table with `<br>` and bullets inside cells: Discord showed a wall of pipes
    and the literal tag. Tables become one block per row, `<br>` a line break; code is left exactly as written."""
    out = cards.discord_markdown(SHOT_REPLY)
    check("|" not in out and "<br>" not in out and "---" not in out.replace("───", ""), "no pipes, no `<br>`, no markdown rule survive")
    check("**Code & files**\n- Edit, view, create (via the `editor` tool).\n- Read files (`file_read`).\n- Apply diffs (`apply_patch`)." in out, "a bulleted cell becomes the row's label over a list")
    check("**Shell / command execution**\n- Run any shell command with timeout." in out, "a one-bullet cell stays a list item under its label")
    check("**Plain** — Just a sentence, no bullet" in out, "a plain two-column row reads as 'label — text'")
    check("───────────" in out and out.rstrip().endswith("Done!") and "Feel free to ask:\n\n- Run a command" in out, "a rule becomes a line; prose and ordinary lists are untouched")
    check("Category" not in out and "What I can do" not in out, "a two-column table's header names add nothing and are dropped")

    three = "| Tool | Cost | Use |\n|---|---|---|\n| shell | low | runs commands |\n| think | high<br>• nested | deep |"
    out3 = cards.discord_markdown(three)
    check("**shell**\n- **Cost:** low\n- **Use:** runs commands" in out3 and "- **Cost:** high; nested" in out3, "three columns keep their header names as labels")

    fenced = "```\n| a | b |\n|---|---|\n| 1<br>2 | 3 |\n```\n\n| x | y |\n|---|---|\n| 1 | 2 |"
    out_f = cards.discord_markdown(fenced)
    check(out_f.startswith("```\n| a | b |\n|---|---|\n| 1<br>2 | 3 |\n```"), "a table inside a code fence is code, left exactly as written")
    check("**1** — 2" in out_f, "…and a real one after it is still converted")
    check(cards.discord_markdown("```\n| a | b |\n|---|---|\n| 1<br>2 |") == "```\n| a | b |\n|---|---|\n| 1<br>2 |", "an unterminated fence (still streaming) stays code to the end")
    check(cards.discord_markdown("| a | b |\n|---|---|") == "", "a table that has only its header so far (streaming) shows nothing yet rather than raw pipes")
    check(cards.discord_markdown("| a | b |\n|---|---|\n| **x** | partial") == "**x** — partial", "a half-streamed row renders with what has arrived")
    check(cards.discord_markdown("a<br>b<br/>c\n#### Deep\n• one\n• two") == "a\nb\nc\n**Deep**\n- one\n- two", "`<br>`, deep headings and `•` bullets outside tables")
    check(cards.discord_markdown("2 | 3 and a | pipe, a ---b, 3 > 2") == "2 | 3 and a | pipe, a ---b, 3 > 2", "ordinary text with pipes or dashes is not mistaken for a table or a rule")
    check(cards.discord_markdown("") == "" and cards.discord_markdown("plain") == "plain", "nothing to convert is returned as it is")


def test_cards_helpers() -> None:
    check(all(cards.is_status_question(t) for t in ["what are you working on ?", "status", "how's it going", "are you still working?", "any progress", "what are you doing <@1>"]), "status questions are recognised, mention or no")
    check(not any(cards.is_status_question(t) for t in ["what can you do", "fix the status page", "update the readme", "are you there", "are you alive", "what is the status of the build and why did it fail on windows please explain it all"]), "…and an instruction, a ping, or a long question, is not mistaken for one")
    check(cards.parse_command("!stop") == ("stop", "") and cards.parse_command("!mode auto") == ("mode", "auto") and cards.parse_command("!bogus x") == ("?", "bogus"), "commands parse; an unknown one is reported as unknown")
    check(cards.parse_command("!back 2") == ("back", "2") and cards.parse_back("") == {"turns": 1} and cards.parse_back("3") == {"turns": 3} and cards.parse_back("to 2") == {"index": 2} and cards.parse_back("list") == "list" and cards.parse_back("0") is None and cards.parse_back("sideways") is None, "`!back`'s arguments parse")
    check(cards.parse_command("hello !stop") is None and cards.parse_command("! hi") is None, "a bang that is not a command is just text")
    long = "intro.\n\n" + "word " * 500 + "\n\n```py\n" + "x = 1\n" * 300 + "```\n\nend"
    chunks = cards.split_message(long, 500)
    check(len(chunks) > 3 and all(len(c) <= 500 for c in chunks), "a long reply splits into Discord-sized messages")
    check(all(c.count("```") % 2 == 0 for c in chunks), "and no chunk leaves a code fence open to swallow the next")
    check(cards.fmt_tokens(12400) == "12.4k" and cards.fmt_tokens(950) == "950" and cards.fmt_seconds(75) == "1m 15s", "numbers are human-sized")
    check(cards.turn_footer({"seconds": 41, "calls": 3, "tokensIn": 12400, "tokensOut": 1100}) == "✓ 41s · 3 tool calls · 12.4k in · 1.1k out", "the receipt under a reply: time, calls, tokens")
    check(cards.turn_footer({"seconds": 12}, stop="cancelled") == "■ stopped after 12s", "a stopped turn says so")
    check("steps" in cards.what_it_wants({"tool": "claude_code_engineer"}) or "Claude Code usage" in cards.what_it_wants({"tool": "claude_code_engineer"}), "a Claude Code hand-off warns that it spends Claude Code usage")


# =========================================================================== conversations (real Supervisor, scripted ACP child)


async def test_talking_to_an_agent() -> None:
    rig = await boot_real()
    channel = rig.channel
    message = await rig.send("what can you do")
    check(len(message.created_threads) == 1, "a message in the channel opens a thread under it")
    thread = message.created_threads[0]
    check(thread.name == "💬 what can you do", "named after what was asked")
    check(thread.messages[0].content.startswith("-# 📁") and str(_PROJECT) in thread.messages[0].content and "!stop" in thread.messages[0].content, "its first line says where the agent works and how to stop it")
    check("mode **build**" in thread.messages[0].content, "and which mode it starts in — a remote chat never starts in a mode its operator has not seen")
    reply = thread.messages[1]
    check("You said: what can you do" in reply.content, "the agent's reply comes back — this is a conversation, not a steering note")
    check("Steering" not in reply.content and "chars queued" not in reply.content, "and it is not the old acknowledgement")
    check("✓" in reply.content and "1.2k in" in reply.content and "300 out" in reply.content and "1 tool call" in reply.content, "with a receipt: time, tool calls, tokens in and out")
    check(reply.allowed_mentions is not None and not reply.allowed_mentions.pings_anyone, "an agent's words are posted with mentions disabled")
    check(thread.typing_entered == 1 and thread.typing_exited == 1, "the typing indicator ran for the turn")
    conv = rig.remote.chats.get(str(thread.id))
    first_acp = conv.acp_id
    check(bool(first_acp) and conv.cwd == str(_PROJECT) and conv.title == "what can you do", "the conversation is registered with its project and ACP session")

    await rig.send("and the second thing", channel=thread)
    check("You said: and the second thing" in thread.messages[-1].content, "a reply in the thread continues the conversation")
    check(rig.remote.chats.get(str(thread.id)).acp_id == first_acp, "in the same ACP session")
    check(len(channel.threads) == 1, "without opening another thread")
    check(rig.remote.chats.totals["turns"] == 2 and rig.remote.chats.totals["tokensIn"] == 2400, "and the daemon counted what the two turns cost")
    await teardown(rig)


async def test_one_conversation_every_door() -> None:
    """The hub from Discord's side: a thread names its conversation, a terminal's turn in it is posted
    here, `!conversations` lists every door's, and `!continue` picks one up in a new thread."""

    class Quiet:
        def text(self, delta: str) -> None:
            pass

        def activity(self, label) -> None:  # noqa: ANN001
            pass

    rig = await boot_real(store="hub-chats.json")
    message = await rig.send("first question")
    thread = message.created_threads[0]
    conv = rig.remote.chats.get(str(thread.id))
    check(f"conversation `{conv.handle}`" in thread.messages[0].content and f"cordata --conversation {conv.handle}" in thread.messages[0].content, "a Discord thread says its conversation's handle, and how to continue it in a terminal")

    before = len(thread.messages)
    await rig.remote.chats.send(conv.key, "asked from the terminal", Quiet(), origin="terminal")
    await settle()
    posted = [m.content or "" for m in thread.messages[before:]]
    check(any("**Terminal** asked" in p and "asked from the terminal" in p for p in posted), f"a turn the terminal ran in this conversation is posted in its thread ({posted[:1]})")
    check(any("You said: asked from the terminal" in p for p in posted), "…with the agent's reply")

    other = rig.remote.chats.create(cwd=str(_PROJECT), title="from the menu bar", origin="menubar")
    await rig.remote.chats.send(other.key, "menu bar question", Quiet(), origin="menubar")
    await rig.send("!conversations")
    listing = rig.channel.messages[-1].content or ""
    check(conv.handle in listing and other.handle in listing and "from Menu bar" in listing, f"!conversations lists every door's conversations ({listing[:160]})")

    reply = await rig.send(f"!continue {other.handle}")
    check(len(reply.created_threads) == 1, "!continue opens a thread for that conversation")
    continued = reply.created_threads[0]
    check(continued.messages[0].content.startswith(f"-# ↪ continuing `{other.handle}`") and "Menu bar" in continued.messages[0].content, "…saying which conversation, and where it started")
    await rig.send("and now from discord", channel=continued)
    check("You said: and now from discord" in continued.messages[-1].content, "a message in the thread is a turn in that conversation")
    check(rig.remote.chats.resolve(other.handle).turns == 2, "…the same conversation, not a new one")
    before = len(continued.messages)
    await rig.remote.chats.send(other.key, "menu bar again", Quiet(), origin="menubar")
    await settle()
    check(any("**Menu bar** asked" in (m.content or "") for m in continued.messages[before:]), "and the menu bar's next turn shows up in the thread that continued it")
    await rig.send("!continue cnope00")
    check("No conversation named" in (rig.channel.messages[-1].content or ""), "an unknown handle is said, not guessed")
    await teardown(rig)

    rebooted = await boot_real(store="hub-chats.json")
    check(rebooted.remote.chats.get(str(continued.id)) is not None and rebooted.remote.chats.get(str(continued.id)).handle == other.handle, "the thread's binding survives a daemon restart")
    await teardown(rebooted)


async def test_agent_text_cannot_ping_or_leak() -> None:
    secret_guard.register_secret("hunter2-secret-value")
    rig = await boot_real()
    message = await rig.send("@everyone look hunter2-secret-value")
    thread = message.created_threads[0]
    reply = thread.messages[1]
    check("@everyone" in reply.content and not reply.allowed_mentions.pings_anyone, "@everyone in a reply is text, not a notification")
    check("hunter2-secret-value" not in reply.content and secret_guard.REDACTED in reply.content, "a registered secret in the reply is masked")
    await teardown(rig)


async def test_a_reply_with_a_table_is_posted_readably() -> None:
    rig = await boot_real()
    message = await rig.send("table")
    thread = message.created_threads[0]
    posted = "\n".join(m.content or "" for m in thread.messages[1:])
    check("|---" not in posted and "<br>" not in posted and "####" not in posted, "a reply with a table is posted with no pipes, no `<br>` and no unrendered heading")
    check("**Code & files**\n- Edit files (`editor`).\n- Read files (`file_read`)." in posted and "**Notes**\nLine one\nline two" in posted, "…laid out as blocks the way Discord renders them")
    check("✓" in thread.messages[1].content, "and the receipt is still under it")
    await teardown(rig)


async def test_a_long_reply_is_split() -> None:
    rig = await boot_real()
    message = await rig.send("long")
    thread = message.created_threads[0]
    replies = thread.messages[1:]
    check(len(replies) > 1 and all(len(m.content) <= 2000 for m in replies), "a long reply becomes several Discord-sized messages")
    check(all(m.content.count("```") % 2 == 0 for m in replies), "none of them leaves a code fence open")
    check("✓" in replies[-1].content and not any("✓" in m.content for m in replies[:-1]), "and the receipt is under the last one only")
    await teardown(rig)


async def test_status_is_answered_without_a_turn() -> None:
    rig = await boot_real()
    await rig.send("what are you working on ? <@1>")
    check(not rig.channel.threads, "a status question does not open a chat")
    card = [m for m in rig.channel.messages if m.embed is not None and "Daemon connected" not in m.embed.title][-1].embed
    check(card.title == "Nothing running", "with nothing running it says so")
    check(rig.remote.chats.totals["turns"] == 0 and not rig.supervisor.sessions(), "and no agent was started — nothing was spent to answer it")

    message = await rig.send("hello")
    thread = message.created_threads[0]
    conv = rig.remote.chats.get(str(thread.id))
    task = asyncio.create_task(rig.client.dispatch("on_message", rig.say("slow", channel=thread)))
    check(await until(lambda: conv.busy), "a slow turn is running")
    await rig.send("what are you working on", channel=thread)
    status = thread.messages[-1].embed
    check("working" in status.title, "asked mid-turn, it reports the agent as working")
    check(any("your chat" in f["name"] and "working" in f["value"] for f in status.fields), "naming the chat and its state")
    await rig.send("!stop", channel=thread)
    await asyncio.wait_for(task, 20)
    check(any("■ stopped" in (m.content or "") for m in thread.messages), "`!stop` ends the turn and the receipt says it was stopped")
    await teardown(rig)


async def test_a_message_mid_turn_steers() -> None:
    rig = await boot_real()
    message = await rig.send("hello")
    thread = message.created_threads[0]
    conv = rig.remote.chats.get(str(thread.id))
    task = asyncio.create_task(rig.client.dispatch("on_message", rig.say("slow", channel=thread)))
    check(await until(lambda: conv.busy), "a turn is running")
    before = len(thread.messages)
    note = await rig.send("also check the tests", channel=thread)
    check(note.reactions == ["📨"] and len(thread.messages) == before, "a message mid-turn is acknowledged with a reaction, not a paragraph")
    check(len([m for m in thread.messages if "Steering" in (m.content or "")]) == 0, "and never the old 'Steering … chars queued' line")
    await rig.send("!stop", channel=thread)
    await asyncio.wait_for(task, 20)
    await rig.send("go on", channel=thread)
    check("also check the tests" in thread.messages[-1].content, "the note reached the agent and landed on its next turn")
    await teardown(rig)


async def test_chat_approvals_are_quiet_and_actionable() -> None:
    rig = await boot_real()
    message = await rig.send("hello")
    thread = message.created_threads[0]
    conv = rig.remote.chats.get(str(thread.id))

    task = asyncio.create_task(rig.client.dispatch("on_message", rig.say("ask: pytest -q", channel=thread)))
    check(await until(lambda: approvals(thread)), "a routine ask reaches the chat's own thread")
    card = approvals(thread)[0]
    check(card.view.labels() == [bot.APPROVE_LABEL, "Always allow", bot.ALL_ROUTINE_LABEL, bot.DENY_LABEL], "a routine ask offers Allow, Always allow (the child offered it), Allow routine steps, Deny")
    check(card.content is None, "a routine ask pings nobody")
    check("Approve this step?" == card.embed.title and "Why it stopped" not in "".join(f["name"] for f in card.embed.fields), "and is not dressed up as an alarm")
    check("clean the build directory" in card.embed.text and "pytest -q" in card.embed.text, "with what was asked and what runs")
    check("`shell(pytest *)`" in card.embed.field("Always allow"), "the rule that would be remembered is shown before it is chosen")

    click = fake_discord.Interaction(OPERATOR)
    await card.view.press(bot.ALL_ROUTINE_LABEL, click)
    await asyncio.wait_for(task, 20)
    check(said(thread, "Allowed: pytest -q"), "'Allow routine steps' allowed this one")
    check(conv.mode == "auto", "and put the chat in auto mode, so routine calls stop asking")
    check("routine steps will now run on their own" in click.response.edits[0]["embed"].title, "the collapsed card says what changed")
    await rig.send("anything", channel=thread)
    check("(mode: auto" in thread.messages[-1].content, "the agent really is in auto mode (the child saw `session/set_mode`)")

    # An armed ask: the shape differs.
    task = asyncio.create_task(rig.client.dispatch("on_message", rig.say("ask-armed: rm -rf build", channel=thread)))
    check(await until(lambda: len(cards_in(thread)) == 2 and approvals(thread)), "a destructive ask is posted too")
    armed = approvals(thread)[-1]
    check(armed.view.labels() == [bot.APPROVE_ONCE_LABEL, bot.DENY_LABEL, bot.DENY_STOP_LABEL], "with no 'allow routine' and no 'always allow' — a destructive call is decided one at a time, and can be denied *and stopped*")
    check(armed.content and armed.allowed_mentions.users and "needs a decision" in armed.content, "and it pings the operator")
    check("⚠ Needs your decision" == armed.embed.title and "removes files" in armed.embed.field("Why it stopped"), "titled as what it is, with Laya's reason")
    check("Deny & stop" in armed.embed.footer, "and the footer explains the extra button — only where it is offered")
    await armed.view.press(bot.DENY_LABEL, fake_discord.Interaction(OPERATOR))
    await asyncio.wait_for(task, 20)
    check(said(thread, "Denied: rm -rf build"), "Deny reaches the agent")
    await teardown(rig)


async def test_deny_and_stop_ends_the_turn() -> None:
    rig = await boot_real()
    message = await rig.send("hello")
    thread = message.created_threads[0]
    conv = rig.remote.chats.get(str(thread.id))
    task = asyncio.create_task(rig.client.dispatch("on_message", rig.say("ask-armed: rm -rf build", channel=thread)))
    check(await until(lambda: approvals(thread)), "a destructive ask is up")
    click = fake_discord.Interaction(OPERATOR)
    await approvals(thread)[0].view.press(bot.DENY_STOP_LABEL, click)
    await asyncio.wait_for(task, 20)
    check("Denied, and the turn was stopped" in click.response.edits[0]["embed"].title, "the card records that it denied and stopped")
    check(said(thread, "Denied: rm -rf build"), "the agent was told no")
    check(not rig.supervisor.session(conv.daemon_id).pending, "and nothing is left parked")
    await teardown(rig)


async def test_always_allow_is_the_childs_own_option() -> None:
    rig = await boot_real()
    message = await rig.send("hello")
    thread = message.created_threads[0]
    task = asyncio.create_task(rig.client.dispatch("on_message", rig.say("ask: pytest -q", channel=thread)))
    check(await until(lambda: approvals(thread)), "an ask with an always-allow offer is posted")
    pending_key = next(iter(rig.supervisor.session(rig.remote.chats.get(str(thread.id)).daemon_id).pending))
    pending_item = rig.supervisor.session(rig.remote.chats.get(str(thread.id)).daemon_id).pending[pending_key]
    check(any(o["kind"] == "allow_always" for o in pending_item.options), "the supervisor holds the child's own option list")
    click = fake_discord.Interaction(OPERATOR)
    await approvals(thread)[0].view.press("Always allow", click)
    await asyncio.wait_for(task, 20)
    check("Always allowed" in click.response.edits[0]["embed"].title, "choosing it is recorded as such")
    check(said(thread, "Allowed: pytest -q"), "and the agent was told yes")
    await teardown(rig)


async def test_another_front_door_answering_collapses_the_chat_card() -> None:
    rig = await boot_real()
    message = await rig.send("hello")
    thread = message.created_threads[0]
    conv = rig.remote.chats.get(str(thread.id))
    task = asyncio.create_task(rig.client.dispatch("on_message", rig.say("ask: pytest -q", channel=thread)))
    check(await until(lambda: approvals(thread)), "a card is up")
    card = approvals(thread)[0]
    key = next(iter(rig.supervisor.session(conv.daemon_id).pending))
    await rig.supervisor.answer_permission(conv.daemon_id, key, True, decided_by="gui")
    await asyncio.wait_for(task, 20)
    check(await until(lambda: card.view is None), "when the GUI answers, the Discord card loses its buttons")
    check("answered from the GUI" in card.embed.text and "Allowed" in card.embed.title, "and says who answered")
    await teardown(rig)


async def test_stop_denies_a_parked_approval() -> None:
    rig = await boot_real()
    message = await rig.send("hello")
    thread = message.created_threads[0]
    conv = rig.remote.chats.get(str(thread.id))
    task = asyncio.create_task(rig.client.dispatch("on_message", rig.say("ask: rm -rf build", channel=thread)))
    check(await until(lambda: approvals(thread)), "a call is parked on a decision")
    stop = await rig.send("!stop", channel=thread)
    await asyncio.wait_for(task, 20)
    check(stop.reactions == ["🛑"], "`!stop` is acknowledged")
    check(said(thread, "Denied: rm -rf build"), "the parked call was denied — a child parked on a prompt never reaches a cancel")
    check(await until(lambda: not approvals(thread)), "and its card collapsed")
    check(not rig.supervisor.session(conv.daemon_id).pending, "nothing is left waiting")
    await teardown(rig)


async def test_an_undeliverable_card_is_a_denial_not_a_parked_child() -> None:
    rig = await boot_real()
    message = await rig.send("hello")
    thread = message.created_threads[0]

    def breaking(original):
        async def send(*args, **kwargs):
            if kwargs.get("view") is not None:  # only the card cannot be delivered; ordinary text still can
                raise fake_discord.HTTPException("discord is down")
            return await original(*args, **kwargs)

        return send

    thread.send = breaking(thread.send)
    rig.channel.send = breaking(rig.channel.send)
    task = asyncio.create_task(rig.client.dispatch("on_message", rig.say("ask: rm -rf build", channel=thread)))
    await asyncio.wait_for(task, 20)
    check(any("Denied: rm -rf build" in m.content for m in thread.messages if m.content), "with no way to ask, the call is denied — not left parked, never waved through")
    await teardown(rig)


async def test_idle_agents_are_reaped_and_conversations_resume() -> None:
    rig = await boot_real(idle_seconds=0.0)
    secret_guard.register_secret("hunter2-secret-value")
    message = await rig.send("remember this hunter2-secret-value")
    thread = message.created_threads[0]
    conv = rig.remote.chats.get(str(thread.id))
    acp = conv.acp_id
    first_child = conv.daemon_id
    check(bool(first_child), "a child is running after a turn")
    check(await rig.remote.chats.reap_idle() == [conv.key] and not conv.daemon_id, "an idle child is stopped")
    check(await until(lambda: rig.supervisor.session(first_child) is None or not rig.supervisor.session(first_child).alive), "…and really gone")
    check(conv.acp_id == acp, "but the conversation keeps its ACP session id")

    await rig.send("are you back", channel=thread)
    reply = thread.messages[-1].content
    check("You said: are you back" in reply, "a reply after the reap just works: a new child loaded the session")
    check("earlier answer" not in reply and not any("earlier answer" in (m.content or "") for m in thread.messages), "and the history it replayed was muted — it is not new output")
    check(conv.acp_id == acp and conv.daemon_id and conv.daemon_id != first_child, "the same ACP session, in a new process")

    fresh = Chats(rig.supervisor, store=_TMPDIR / "chats.json")
    restored = fresh.get(str(thread.id))
    check(restored is not None and restored.acp_id == acp and restored.cwd == str(_PROJECT), "the conversation survives a daemon restart (persisted)")
    check("hunter2-secret-value" not in (_TMPDIR / "chats.json").read_text(), "a secret in the opening message is masked in the title written to disk")
    mode = (_TMPDIR / "chats.json").stat().st_mode & 0o777
    check(mode == 0o600, f"and the store is private ({oct(mode)})")
    await teardown(rig)


async def test_a_lost_history_is_admitted() -> None:
    """If a chat's saved session is gone from disk, the agent still answers — from a blank slate. It must not
    look like it remembers."""
    rig = await boot_real(idle_seconds=0.0, store="lost.json")
    message = await rig.send("remember the number 7")
    thread = message.created_threads[0]
    conv = rig.remote.chats.get(str(thread.id))
    await rig.remote.chats.reap_idle()
    conv.acp_id = "acp-this-session-was-deleted"
    await rig.send("what number?", channel=thread)
    check(said(thread, "You said: what number?"), "the agent still answers")
    check(said(thread, "couldn't restore this chat's earlier history"), "and the thread says its history could not be restored")
    await rig.send("and again", channel=thread)
    check(len([m for m in thread.messages if "couldn't restore" in (m.content or "")]) == 1, "…once, not on every later turn")
    await teardown(rig)


async def test_status_lists_sleeping_chats() -> None:
    rig = await boot_real(idle_seconds=0.0)
    message = await rig.send("hello")
    await rig.remote.chats.reap_idle()
    await rig.send("!status")
    card = [m for m in rig.channel.messages if m.embed is not None and "Daemon connected" not in m.embed.title][-1].embed
    check("asleep" in card.description and "reply in" in card.description, "a sleeping chat is mentioned, with how to wake it")
    await teardown(rig)


async def test_a_dying_agent_is_reported_and_replaced() -> None:
    rig = await boot_real()
    message = await rig.send("hello")
    thread = message.created_threads[0]
    await rig.send("die", channel=thread)
    check(thread.messages[-1].content.startswith("⚠") and "went away" in thread.messages[-1].content and "session '" not in thread.messages[-1].content, "an agent that exits mid-turn is a plain message with no internal ids, not silence")
    await rig.send("are you there", channel=thread)
    check("You said: are you there" in thread.messages[-1].content, "and the next message — even 'are you there' — starts a fresh one rather than being answered as a status question")
    await teardown(rig)


async def test_live_agents_are_capped() -> None:
    rig = await boot_real(max_live=1, store="capped.json")
    first = (await rig.send("one")).created_threads[0]
    first_conv = rig.remote.chats.get(str(first.id))
    check(bool(first_conv.daemon_id), "one agent is running")
    second = (await rig.send("two")).created_threads[0]
    check("You said: two" in second.messages[-1].content, "a second chat still works: the idle first agent made room")
    check(not first_conv.daemon_id and len(rig.remote.chats.live()) == 1, "by being reaped — never more than the cap")

    conv2 = rig.remote.chats.get(str(second.id))
    task = asyncio.create_task(rig.client.dispatch("on_message", rig.say("slow", channel=second)))
    check(await until(lambda: conv2.busy), "now the only slot is mid-turn")
    third = (await rig.send("three")).created_threads[0]
    check("already working" in third.messages[-1].content and "!stop" in third.messages[-1].content, "a third chat is told plainly, instead of loading a third model stack")
    await rig.send("!stop", channel=second)
    await asyncio.wait_for(task, 20)
    await teardown(rig)


async def test_commands() -> None:
    rig = await boot_real(store="commands.json")
    await rig.send("!help")
    check("Only <@" in rig.channel.messages[-1].embed.description and any("Talk to an agent" == f["name"] for f in rig.channel.messages[-1].embed.fields), "`!help` explains how it works")
    await rig.send("!nonsense")
    check("!nonsense" in rig.channel.messages[-1].content, "an unknown command is named, not silently treated as a prompt")
    check(not rig.channel.threads and rig.remote.chats.totals["turns"] == 0, "and spent nothing")

    await rig.send(f"!new {_PROJECT}")
    thread = rig.channel.threads[0]
    check(rig.remote.chats.get(str(thread.id)).cwd == str(_PROJECT) and "Ready" in thread.messages[-1].content, "`!new <folder>` opens a chat in that project")
    await rig.send("!new /definitely/not/a/folder")
    check("not a folder" in rig.channel.messages[-1].content and len(rig.channel.threads) == 1, "a folder that does not exist is refused")

    await rig.send("!mode", channel=thread)
    check("Mode here" in thread.messages[-1].content, "`!mode` reports the current mode")
    await rig.send("!mode auto", channel=thread)
    check("**auto**" in thread.messages[-1].content and rig.remote.chats.get(str(thread.id)).mode == "auto", "`!mode auto` sets it")
    await rig.send("!mode chaos", channel=thread)
    check("one of plan, build, auto" in thread.messages[-1].content, "a mode that does not exist is refused")
    await rig.send("!mode auto")
    check("inside that chat's thread" in rig.channel.messages[-1].content, "`!mode` outside a chat says where it applies")

    await rig.send("hello", channel=thread)
    await rig.send("!usage")
    usage = rig.channel.messages[-1].embed
    check(usage.title.startswith("Usage") and usage.field("Tokens in") == "1.2k", "`!usage` reports what the turns cost, from their receipts")
    await rig.send("!stop")
    check("Nothing is running" in rig.channel.messages[-1].content, "`!stop` with nothing running says so")

    # `!back` — the agent forgets turns; checkpoints are listed; nothing is spent
    await rig.send("second thing to do", channel=thread)
    spent = rig.remote.chats.totals["turns"]
    await rig.send("!back list", channel=thread)
    listing = thread.messages[-1].content
    check("Checkpoints" in listing and "before turn 2: second thing to do" in listing and "before turn 1: hello" in listing, f"`!back list` shows each turn's prompt ({listing[:90]!r})")
    await rig.send("!back", channel=thread)
    said = thread.messages[-1].content
    check("Went back 1 turn" in said and "second thing to do" in said and "Files are untouched" in said, f"`!back` says what the agent forgot, and that files are left alone ({said[:80]!r})")
    await rig.send("!back 5", channel=thread)
    check("Went back 1 turn" in thread.messages[-1].content and "hello" in thread.messages[-1].content, "asking for more turns than there are goes back as far as there is")
    await rig.send("!back", channel=thread)
    check("nothing to go back to" in thread.messages[-1].content, "with nothing left it says so")
    await rig.send("!back sideways", channel=thread)
    check("`!back 2`" in thread.messages[-1].content, "an argument it does not take shows what it does")
    await rig.send("!back")
    check("inside that chat's thread" in rig.channel.messages[-1].content, "`!back` outside a chat says where it applies")
    check(rig.remote.chats.totals["turns"] == spent, "going back spends no tokens")
    await teardown(rig)


async def test_a_stranger_cannot_talk_to_an_agent() -> None:
    rig = await boot_real()
    await rig.send("hello", author=OTHER)
    await rig.send("!stop", author=OTHER)
    check(not rig.channel.threads and not rig.supervisor.sessions(), "a stranger's message starts nothing — not a thread, not a process")
    await teardown(rig)


# =========================================================================== main


async def main() -> None:
    test_config_fails_closed()
    test_cards_helpers()
    test_agent_markdown_is_made_discord_safe()
    await test_start_is_a_no_op_without_a_token()
    await test_startup_failure_does_not_raise()
    await test_a_reconnect_edits_the_announcement_instead_of_reposting()
    await test_shutdown_unsubscribes_first()
    await test_an_armed_card_says_what_where_and_why()
    await test_scratch_sessions_are_labelled_not_paged()
    await test_quiet_pendings_are_not_posted()
    await test_threshold_follows_the_project()
    await test_a_stranger_click_is_not_a_decision()
    await test_the_operator_unblocks_the_agent_and_the_card_collapses()
    await test_a_prompt_answered_elsewhere_closes_the_card()
    await test_secrets_in_the_view_are_masked()
    await test_a_long_diff_is_truncated_visibly()
    await test_workflow_tasks_open_one_thread_each()
    await test_task_storms_are_coalesced()
    await test_steering_a_terminal_session_is_quiet_and_honest()
    await test_the_channel_steers_only_when_explicit_and_unambiguous()
    await test_messages_from_elsewhere_are_ignored()
    await test_talking_to_an_agent()
    await test_one_conversation_every_door()
    await test_agent_text_cannot_ping_or_leak()
    await test_a_reply_with_a_table_is_posted_readably()
    await test_a_long_reply_is_split()
    await test_status_is_answered_without_a_turn()
    await test_a_message_mid_turn_steers()
    await test_chat_approvals_are_quiet_and_actionable()
    await test_deny_and_stop_ends_the_turn()
    await test_always_allow_is_the_childs_own_option()
    await test_another_front_door_answering_collapses_the_chat_card()
    await test_stop_denies_a_parked_approval()
    await test_an_undeliverable_card_is_a_denial_not_a_parked_child()
    await test_idle_agents_are_reaped_and_conversations_resume()
    await test_a_lost_history_is_admitted()
    await test_status_lists_sleeping_chats()
    await test_a_dying_agent_is_reported_and_replaced()
    await test_live_agents_are_capped()
    await test_commands()
    await test_a_stranger_cannot_talk_to_an_agent()
    print("\nPASS: you can talk to an agent from Discord; prompts say what, where and why; approvals are quiet, honest and collapse when decided; nothing an agent writes can ping or leak")


if __name__ == "__main__":
    asyncio.run(main())
