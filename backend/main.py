"""AG-UI front door: the Strands agent (agent_common.py) over HTTP, for the
CopilotKit web client. See acp_server.py for the ACP/editor front door onto
the same agent definition.

Local-only research skeleton — no Okta/O4AA, no AgentCore, no cloud calls.
Swap OMLX_BASE_URL / OMLX_MODEL_ID in agent_common.py for a real Bedrock or
Anthropic model later; the agent/tool code doesn't change.

Tools granted here have real capability on this machine (shell, python_repl,
editor, file_read, file_write). Confirmation prompts are bypassed at the
process level (BYPASS_TOOL_CONSENT) because a headless server has no TTY to
prompt on — but the three most dangerous tools (shell, python_repl,
file_write) are gated by a real human-in-the-loop approval instead, via
ag_ui_strands's ``interrupt_on_call``: the tool call pauses, the frontend
shows an approve/deny card (frontend/app/page.tsx, useHumanInTheLoop), and
the tool only runs if the human approves. Tools default to a relative-path
CWD of backend/workspace/, but nothing stops an absolute path from leaving
it. Don't expose this beyond localhost.
"""

import hashlib
import re
import uuid

from ag_ui_strands import StrandsAgent, StrandsAgentConfig, ToolBehavior, create_strands_app
from fastapi import HTTPException
from pydantic import BaseModel

# Imported before agent_common: importing agent_common chdirs the process
# into backend/workspace/, which breaks any later bare-module import relying
# on the '' (cwd) entry in sys.path — see agent_common.py's own comment on
# why its early tool imports precede that chdir, for the same reason.
# sandbox_functions_api is a bare local module too (backend/sandbox_functions_
# api.py); it must precede the agent_common import below, not just live
# somewhere in this block — the chdir is a side effect of agent_common's
# first import, wherever that's triggered from, so this line has to be the
# one that triggers it (sandbox_functions_api's own `from agent_common
# import WORKSPACE_DIR` does the loading transitively). Putting it after
# agent_common's own import here breaks with exactly the ModuleNotFoundError
# this comment warns about, even though both look equally "early".
from skill_validation_hooks import SkillValidationHooks
from laya_triage import triage_hooks
from prompt_security import SecretRedactionHooks, UntrustedToolResultHooks
from security_headers import register_security_headers
from model_control_tool import _context_window_limit, _fetch_available_models
import mcp_manager
import provider_settings
# games_static_api before sandbox_functions_api, not just before it in this
# file's own read order: games_static_api's own import of sandbox_functions_
# api (which is what actually triggers agent_common's chdir, transitively —
# see that module's own comment) has to be the first thing to pull in this
# whole chain, or else sandbox_functions_api's chdir fires first and leaves
# the bare `games_static_api` name unresolvable via the now-stale '' (cwd)
# sys.path entry. Confirmed live: the naive order raised "ModuleNotFoundError:
# No module named 'games_static_api'" at this exact line.
from games_static_api import register_game_routes
from sandbox_functions_api import register_sandbox_routes
from agent_common import GENERATED_TOOL_NAMES, MCP_TOOL_NAMES, build_plugins, build_template_agent, start_mcp_clients
from settings import EXTRA_CORS_ORIGINS

# Tools whose real-world impact (arbitrary shell/code execution, overwriting
# files) is worth a human confirming before they actually run. `editor` is
# here for the same reason as file_write — per its own module docstring it
# can Create/Replace/Insert file content, real host-filesystem writes, not
# just view them. acp_server.py's own APPROVAL_REQUIRED_TOOLS already gated
# it for the ACP/Zed front door and flagged this front door as the
# not-yet-actioned gap; this closes it. Extended with every tool name loaded
# from skills_generated/ — the self-healing loop's own output is exactly as
# untrusted as shell/python_repl/file_write until a human has approved its
# first real call (docs/self-healing-architecture.md §4, hand-off #2). Also
# extended with every MCP-server-provided tool name (agent_common.py's
# MCP_TOOL_NAMES) — a user-configured MCP server (mcp_manager.py) can call
# out to anything (send a message, delete a resource), and the fixed name
# list above has no way to know its real-world impact ahead of time; until
# this repo can classify a specific server's tools as safe, every one of
# them defaults to the same pause as shell/python_repl.
_APPROVAL_REQUIRED_TOOLS = ["shell", "python_repl", "file_write", "editor", *GENERATED_TOOL_NAMES, *MCP_TOOL_NAMES]


def _agent_config_payload() -> dict:
    """The subset of agent_common's ACTIVE_* globals worth showing the
    frontend — shared by /models, _model_state, and _context_state so the
    three never drift out of sync with each other.
    """
    import agent_common
    import sandbox_mode_tool

    return {
        "active_model": agent_common.ACTIVE_MODEL_ID,
        "available_models": _fetch_available_models(),
        "temperature": agent_common.ACTIVE_TEMPERATURE,
        "memory_window": agent_common.ACTIVE_MEMORY_WINDOW,
        "context_window_limit": _context_window_limit(agent_common.ACTIVE_MODEL_ID),
        "sandbox_mode": sandbox_mode_tool.CURRENT_SANDBOX_MODE,
    }


def _model_state(_ctx) -> dict:
    """AG-UI shared-state payload after list_models/set_active_model/
    set_temperature/set_memory_window runs.

    Re-derives state from scratch (the ACTIVE_* globals + a fresh oMLX
    /models query) instead of parsing the tool's own text return — decouples
    the shared-state shape from whatever prose the tool happens to return.
    Read on the frontend via useAgent({agentId: "sierralab_agent"}).state.
    """
    return _agent_config_payload()


def _context_state(ctx) -> dict:
    """AG-UI shared-state payload after context_usage runs.

    Unlike _model_state, this genuinely needs the tool's own return value —
    token usage lives in the per-thread Agent's message history, which
    nothing outside a running tool call (this callback included) has a
    handle on. context_usage returns a JSON object, so ag_ui_strands has
    already parsed ctx.result_data into a plain dict by the time this runs.
    """
    if isinstance(ctx.result_data, dict):
        return {"context_usage": ctx.result_data}
    return {}


def _game_state(ctx) -> dict:
    """AG-UI shared-state payload after open_game runs, read by GameSandbox.tsx
    via useAgent(...).state.active_game.

    ctx.tool_input (verified field, ag_ui_strands/config.py:36) carries the
    call's own game_id/title straight through — no need to re-derive them.
    state_from_result fires unconditionally, even when open_game's own
    _GAME_ID_RE/index.html checks failed and it returned an "Error: ..."
    string (verified reading ag_ui_strands/agent.py) — checking for that
    prefix here is what keeps a failed call from ever reaching the frontend
    as "active" (an empty dict is a verified no-op merge: current_state.
    update(snapshot) only runs `if snapshot`).
    """
    if isinstance(ctx.result_data, str) and ctx.result_data.startswith("Error:"):
        return {}
    tool_input = ctx.tool_input if isinstance(ctx.tool_input, dict) else {}
    game_id, title = tool_input.get("game_id"), tool_input.get("title")
    if not game_id or not title:
        return {}
    # version: a hash of the file's own bytes, not a fresh uuid every call.
    # Live-reported regression from the first cut of this (a fresh uuid on
    # every open_game): a human who had already been clicking through a real
    # game themselves lost all of it the moment the agent re-announced the
    # *same*, *unchanged* game_id for an unrelated reason (here, "check its
    # state") — a fresh uuid forces GameSandbox.tsx's <iframe> key to change
    # regardless of whether anything on disk actually did, wiping its live
    # JS state (the board) back to whatever the freshly-reloaded script
    # starts with. Hashing the content instead means the *only* thing that
    # still forces a remount is the file genuinely changing (e.g. a model
    # fixing a validation error and calling file_write again before retrying
    # open_game) — re-announcing identical content is now a no-op key-wise,
    # so an in-progress human game survives being "reopened".
    import agent_common

    index_path = agent_common.WORKSPACE_DIR / "games" / game_id / "index.html"
    try:
        version = hashlib.sha256(index_path.read_bytes()).hexdigest()[:16]
    except OSError:
        # Shouldn't happen — open_game already confirmed this file exists
        # before ever returning success — but a remount is the safe default
        # over crashing this callback if it somehow did.
        version = uuid.uuid4().hex
    return {"active_game": {"game_id": game_id, "title": title, "version": version}}


def _style_context_builder(input_data, user_message: str) -> str:
    """Read the frontend's tone/length picks (agent.setState) and prefix a
    one-line style hint onto the user's message.

    This is CopilotKit's own documented "Agent Config" pattern (tone,
    expertise, response length as shared state the agent rebuilds its
    behavior from each turn) — deterministic and tool-call-free, unlike the
    ACTIVE_* config above: no LLM decision is involved in applying it, so it
    carries none of this app's usual local-model tool-calling-reliability
    risk. What it does carry: every extra token here is a token added to
    every single turn's prompt, and agent_common.SYSTEM_PROMPT is kept
    deliberately short because a longer prompt measurably hurt this small
    model's own tool-calling reliability — so this stays a single short
    line, not a rebuilt system prompt.
    """
    state = getattr(input_data, "state", None) or {}
    tone = state.get("tone") if isinstance(state, dict) else None
    length = state.get("length") if isinstance(state, dict) else None
    if not tone and not length:
        return user_message
    bits = [b for b in (tone, length and f"{length} replies") if b]
    return f"[Style preference: {', '.join(bits)}]\n{user_message}"


def session_manager_provider(run_input):
    """Give each AG-UI thread its own durable Strands session on local disk."""
    from agent_common import SESSIONS_DIR
    from strands.session.file_session_manager import FileSessionManager

    return FileSessionManager(session_id=run_input.thread_id, storage_dir=str(SESSIONS_DIR))


agui_agent = StrandsAgent(
    agent=build_template_agent(),
    name="sierralab_agent",
    description="Local Strands agent with shell/file/code tools and durable per-thread memory.",
    # Plugins (AgentSkills) don't survive the template agent -> per-thread
    # agent cloning ag_ui_strands does internally — they have to be given to
    # StrandsAgent directly, per its own runtime warning.
    plugins=build_plugins(),
    # AST-checks, dry-runs, and (on failure) rejects any tool call selected
    # from backend/skills_generated/ before it executes — the second,
    # independent gate on generated tools, on top of the approval below.
    hooks=[
        SkillValidationHooks(),
        UntrustedToolResultHooks(extra_untrusted_tools=MCP_TOOL_NAMES),
        SecretRedactionHooks(),
        *triage_hooks(),
    ],
    config=StrandsAgentConfig(
        session_manager_provider=session_manager_provider,
        # Deterministic, tool-call-free config: applied to every turn's
        # prompt regardless of whether the model decides to do anything.
        state_context_builder=_style_context_builder,
        tool_behaviors={
            **{name: ToolBehavior(interrupt_on_call=True) for name in _APPROVAL_REQUIRED_TOOLS},
            # Not approval-gated: these are benign config toggles / reads,
            # not code execution — but every call pushes fresh state to the
            # frontend.
            "list_models": ToolBehavior(state_from_result=_model_state),
            "set_active_model": ToolBehavior(state_from_result=_model_state),
            "set_temperature": ToolBehavior(state_from_result=_model_state),
            "set_memory_window": ToolBehavior(state_from_result=_model_state),
            "context_usage": ToolBehavior(state_from_result=_context_state),
            "set_sandbox_mode": ToolBehavior(state_from_result=_model_state),
            "open_game": ToolBehavior(state_from_result=_game_state),
        },
    ),
)

app = create_strands_app(
    agui_agent,
    "/",
    # localhost:3000 always allowed (a fresh single-machine checkout needs no
    # config); anything past that is your own deployment's own origin(s) —
    # see settings.py's EXTRA_CORS_ORIGINS comment.
    origins=["http://localhost:3000", *EXTRA_CORS_ORIGINS],
)
register_security_headers(app)


@app.on_event("startup")
async def _start_mcp_clients() -> None:
    """Runs inside uvicorn's own real event loop — see start_mcp_clients'
    own docstring for why that's the whole point: Agent(tools=TOOLS) above
    already tried to start these same clients itself, from a throwaway
    thread/loop that doesn't survive to serve the real request this event
    loop will actually handle.
    """
    await start_mcp_clients()


@app.get("/models")
def get_agent_config() -> dict:
    """Plain REST escape hatch for the frontend's initial config values —
    independent of the chat/tool-call loop, so the UI can populate its
    controls before the agent has run any tool this session. Same
    underlying values _model_state pushes as AG-UI state once a config tool
    actually runs. (Route name kept as /models for compatibility with the
    original model-only version; payload has grown since.)
    """
    return _agent_config_payload()


class AgentDefaultsPatch(BaseModel):
    model_id: str | None = None
    temperature: float | None = None
    memory_window: int | None = None


@app.post("/models")
def set_agent_defaults(patch: AgentDefaultsPatch) -> dict:
    """Silent counterpart to the set_active_model/set_temperature/
    set_memory_window tools (model_control_tool.py), for restoring a
    frontend-persisted default onto a brand-new, zero-message thread.

    Those tools mutate a *live* per-thread Agent via a real chat turn, on
    purpose — an in-progress conversation's config should visibly change
    right there. A fresh thread has no live Agent yet: build_model()/
    build_conversation_manager() (agent_common.py) read these same ACTIVE_*
    globals when ag_ui_strands lazily constructs one, so mutating them here
    has the identical effect on the *next* thread with none of a chat
    turn's downsides — no synthetic message baked permanently into that
    thread's history, no dependence on this local model's occasionally
    flaky tool-calling to actually make the restore stick.

    frontend/app/AgentSettings.tsx's restore-on-mount effect used to call
    the real tools via a fake user message instead — harmless the moment
    the persisted value already matched the backend default, but every
    mismatch (e.g. this process restarting and resetting ACTIVE_TEMPERATURE
    while the browser's localStorage still remembered an old value) sent
    that fake message into every new thread's permanent history, forever,
    until the mismatch was resolved. Reported live: a brand new thread's
    first "message" was "Call set_temperature with temperature=1.1."
    """
    import agent_common

    if patch.model_id is not None:
        available = _fetch_available_models()
        if available and patch.model_id not in available:
            raise HTTPException(status_code=400, detail=f"'{patch.model_id}' is not currently loaded in oMLX.")
        agent_common.ACTIVE_MODEL_ID = patch.model_id

    if patch.temperature is not None:
        if not 0.0 <= patch.temperature <= 2.0:
            raise HTTPException(status_code=400, detail="temperature must be between 0.0 and 2.0.")
        agent_common.ACTIVE_TEMPERATURE = patch.temperature

    if patch.memory_window is not None:
        if patch.memory_window < 2:
            raise HTTPException(status_code=400, detail="memory_window must be at least 2.")
        agent_common.ACTIVE_MEMORY_WINDOW = patch.memory_window

    return _agent_config_payload()


@app.get("/provider-config")
def get_provider_config() -> dict:
    """Redacted provider state for the settings modal's Model tab — see
    provider_settings.get_provider_state's own docstring for why this never
    carries a raw API key.
    """
    return provider_settings.get_provider_state()


class ProviderConfigPatch(BaseModel):
    provider: str | None = None
    anthropic_api_key: str | None = None
    anthropic_model_id: str | None = None
    openai_base_url: str | None = None
    openai_api_key: str | None = None
    openai_model_id: str | None = None


@app.post("/provider-config")
def set_provider_config(patch: ProviderConfigPatch) -> dict:
    """Switch provider and/or save credentials from the settings modal.

    Same trust level as /models (set_agent_defaults) — a human acting
    directly through a settings form, no approval-card gate needed on top.
    Takes effect immediately for any thread built after this call; an
    already-running thread keeps whatever Model it was constructed with.
    """
    try:
        return provider_settings.set_provider_config(**patch.model_dump(exclude_unset=True))
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc


@app.get("/mcp-servers")
def list_mcp_servers() -> list[dict]:
    """Every configured server from backend/mcp_servers.json, for the
    settings modal's MCP tab. See mcp_manager.list_servers's own docstring
    for why this isn't redacted the way /provider-config is.
    """
    return mcp_manager.list_servers()


class McpServerConfig(BaseModel):
    name: str
    config: dict


@app.post("/mcp-servers")
def add_mcp_server(body: McpServerConfig) -> dict:
    """Add or replace one server entry. Same trust level as /models and
    /provider-config — a human acting directly through the settings form.

    restart_required=True always: mcp_manager.load_configured_mcp_clients()
    only runs once, at process start (agent_common.py's TOOLS) — see that
    module's own docstring for why this isn't hot-reloaded into an
    already-running agent.
    """
    try:
        mcp_manager.add_server(body.name, body.config)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc
    return {"servers": mcp_manager.list_servers(), "restart_required": True}


@app.delete("/mcp-servers/{name}")
def delete_mcp_server(name: str) -> dict:
    """Remove one server entry. 404s if it wasn't there to begin with."""
    if not mcp_manager.remove_server(name):
        raise HTTPException(status_code=404, detail=f"No MCP server named '{name}'.")
    return {"servers": mcp_manager.list_servers(), "restart_required": True}


@app.get("/skills")
def list_skills() -> list[dict]:
    """Every skill under backend/skills/, for the frontend's `/` command menu
    (ChatPane.tsx). Skill.from_directory is a plain sync classmethod that
    reads SKILL.md files straight off disk — no live Agent or sandbox
    indirection needed (that machinery in AgentSkills.get_available_skills
    exists for per-agent filesystem resolution, irrelevant for "what's on
    disk right now"). Source of truth stays the actual directory: add or
    remove a skill folder and this list changes with it, no separate
    registry to keep in sync.

    A malformed SKILL.md is skipped with a warning by Skill.from_directory
    itself (strict=False, this deployment's default), not raised here.
    """
    from strands.vended_plugins.skills import Skill

    from agent_common import SKILLS_DIR

    return [{"name": skill.name, "description": skill.description} for skill in Skill.from_directory(SKILLS_DIR)]


# skill-authoring/SKILL.md's own naming rule, enforced here too so a skill
# created through the settings UI is discoverable the same way one the agent
# writes itself is — lowercase/digits/hyphens, must start and end with a
# letter or digit.
_SKILL_NAME_RE = re.compile(r"^[a-z0-9]([a-z0-9-]*[a-z0-9])?$")


class SkillCreate(BaseModel):
    name: str
    description: str
    body: str


@app.post("/skills")
def create_skill(skill: SkillCreate) -> dict:
    """Write a new backend/skills/<name>/SKILL.md from the settings modal's
    Skills tab, same frontmatter shape skill-authoring/SKILL.md documents
    for the agent's own file_write path. Refuses to overwrite an existing
    skill directory — no accidental clobber from the UI.

    Takes effect starting the *next* conversation, same constraint
    skill-authoring/SKILL.md already documents for an agent-authored skill:
    AgentSkills only discovers SKILL.md files once, at session start.
    """
    from agent_common import SKILLS_DIR

    if not _SKILL_NAME_RE.match(skill.name):
        raise HTTPException(
            status_code=400,
            detail="name must be lowercase letters, digits, and hyphens only, starting and ending with a letter or digit.",
        )
    skill_dir = SKILLS_DIR / skill.name
    if skill_dir.exists():
        raise HTTPException(status_code=400, detail=f"Skill '{skill.name}' already exists.")

    skill_dir.mkdir(parents=True)
    content = f"---\nname: {skill.name}\ndescription: {skill.description}\n---\n\n{skill.body}\n"
    (skill_dir / "SKILL.md").write_text(content)
    return {"name": skill.name, "description": skill.description}


@app.delete("/skills/{name}")
def delete_skill(name: str) -> dict:
    """Remove a skill directory. The frontend shows a native confirm()
    first — directory deletion has no undo — but that's a client-side
    courtesy, not a server-side gate; same trust level as every other
    settings-UI route in this file.
    """
    import shutil

    from agent_common import SKILLS_DIR

    skill_dir = SKILLS_DIR / name
    # Reject a name that would resolve outside SKILLS_DIR (e.g. "../../etc")
    # before ever touching the filesystem — unlike the create path, `name`
    # here comes from the URL path, not a value already checked against
    # _SKILL_NAME_RE at creation time.
    if skill_dir.resolve().parent != SKILLS_DIR.resolve() or not skill_dir.is_dir():
        raise HTTPException(status_code=404, detail=f"No skill named '{name}'.")

    shutil.rmtree(skill_dir)
    return {"name": name, "deleted": True}


_WORKSPACE_FILES_LIMIT = 500


@app.get("/workspace-files")
def list_workspace_files() -> list[str]:
    """Relative paths of every file under agent_common.WORKSPACE_DIR, for the
    frontend's `@` mention autocomplete (MentionAutocomplete.tsx). Read-only
    listing, no approval gate needed the way sandbox_functions_api.py's
    routes are gated — nothing here can be used to read a file's contents,
    only to learn a path exists, and the workspace root is already the same
    directory every tool's relative paths resolve against by default.

    Capped rather than exhaustive: this deployment's workspace can accumulate
    a lot of scratch output over a long session, and an autocomplete list
    with hundreds of entries stops being useful long before it stops being
    technically correct.
    """
    from agent_common import WORKSPACE_DIR

    paths = []
    for path in sorted(WORKSPACE_DIR.rglob("*")):
        if path.is_file():
            paths.append(str(path.relative_to(WORKSPACE_DIR)))
            if len(paths) >= _WORKSPACE_FILES_LIMIT:
                break
    return paths


# /sandbox/* — the REST bridge for openGenerativeUI.sandboxFunctions
# (frontend/app/page.tsx), not the chat agent's own tool-call loop. See
# sandbox_functions_api.py's module docstring for why these calls need their
# own client-side approval gate instead of reusing interrupt_on_call.
register_sandbox_routes(app, _agent_config_payload)

# /games/<game_id>/* — static file serving for GameSandbox.tsx's iframe, gated
# by the same shared-secret token as /sandbox/* (see games_static_api.py's
# own module docstring for why StaticFiles/app.mount couldn't be used here).
register_game_routes(app)

# No backend route for the chat input's `$` trigger (frontend/app/
# useQueueingAgentInput.ts): unlike /sandbox/*, a `$ <command>` from the
# human's own chat input rewrites into a normal chat message ("Call the
# shell tool with command=...") and goes through the *real* agent turn —
# the same server-side-enforced ToolBehavior(interrupt_on_call=True) gate
# every other shell call gets, not a new REST endpoint a client-side-only
# approval card would be the sole gate for. An earlier draft of this file
# had exactly that endpoint; the harness's own auto-mode classifier refused
# to create it ("Create RCE Surface") because a plain POST route that runs
# strands_tools.shell with no server-side gate is a real unauthenticated
# remote-code-execution surface regardless of what the frontend promises to
# do before calling it -- correctly, on the same reasoning this session's
# own earlier security-warning about bridging generateSandboxedUi straight
# to shell already laid out. Reusing the agent's own tool-call path instead
# needed no new backend code at all.


if __name__ == "__main__":
    import uvicorn

    import agent_common

    # reload_dirs is required, not optional: agent_common's own import (line
    # 40 above) already os.chdir()'d the process into backend/workspace/
    # before this line ever runs, and uvicorn's reloader otherwise defaults
    # to watching the *current* working directory at startup — meaning
    # without this, it silently watches workspace/ (a near-empty sandbox
    # dir) instead of backend/ itself, and no edit to any real source file
    # ever triggers a restart. Confirmed live: every edit made against a
    # server started before this fix went completely unpicked-up, with no
    # error or warning — `Will watch for changes in these directories:
    # ['.../backend/workspace']` in the startup log was the only sign.
    # agent_common.BACKEND_DIR is the absolute path computed before that
    # chdir happens, so it's correct regardless of where cwd ends up.
    uvicorn.run("main:app", host="0.0.0.0", port=8001, reload=True, reload_dirs=[str(agent_common.BACKEND_DIR)])
