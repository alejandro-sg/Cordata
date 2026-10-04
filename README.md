# sierralab-agents

Local-only research skeleton: Strands Agents SDK (Python) → AG-UI → CopilotKit (Next.js),
plus an [Agent Client Protocol](https://agentclientprotocol.com) front door for ACP editors
(Zed, etc.) onto the same agent. No Okta/O4AA, no AgentCore, no cloud calls — everything
runs on localhost (+ one local container, Qdrant, for semantic memory), backed by your
own local model server (built and tested against oMLX on Apple Silicon; any
OpenAI-compatible `/v1/chat/completions` server works — see `OMLX_BASE_URL`/`OMLX_MODEL_ID`
below).

## Quick start

Prerequisites: Python 3.12+, Node 22+, and an OpenAI-API-compatible local model server
already running (this repo doesn't start or manage the LLM itself). Docker or Podman if you
want the included Qdrant container for long-term agent memory — optional, everything else
works without it.

```bash
git clone <this-repo-url> && cd sierralab-agents
scripts/setup.sh    # backend venv + pinned deps, frontend deps, .env files from templates
scripts/dev-up.sh   # Qdrant (if available) + backend + frontend
```

Open `http://localhost:3000`.

- `scripts/dev-status.sh` — what's running and whether it's actually responding
- `scripts/dev-down.sh` — stop backend + frontend (Qdrant keeps running; `--with-qdrant` to stop it too)
- `scripts/dev-restart.sh` — restart backend + frontend
- `tail -f .dev/backend.log .dev/frontend.log` — logs (or `make logs`)

Same commands via `make setup` / `make up` / `make down` / `make restart` / `make status`.
Details, including what each script actually does, in `scripts/README.md`.

There's no confirmation gate at the process level (`BYPASS_TOOL_CONSENT` — a headless
server has no TTY to prompt on) — the real safety net is per-tool human-in-the-loop
approval for anything destructive (`shell`, `python_repl`, `file_write`; see "AG-UI
frontend tools & human-in-the-loop" below), plus a network-edge token gate on the routes
that bypass the agent's own tool-call loop entirely (`SANDBOX_API_TOKEN` — auto-generated
per process start when you don't set one, so this is never actually open even with zero
config; see `backend/.env.example`). Full reasoning, including what's *not* covered, in
[`THREAT_MODEL.md`](THREAT_MODEL.md).

## What the agent can do

`backend/agent_common.py` is the one agent definition both front doors share:

- `shell`, `python_repl` (code execution), `editor`, `file_read`/`file_write` — real
  filesystem and command access on this machine.
- `http_request`, `current_time`, `calculator`, `think` — general-purpose utility tools.
- `agent_memory` (`backend/agent_memory_tool.py`) — durable semantic memory, see below.
- `local_swarm` (`backend/local_swarm_tool.py`) — spins up specialized **Strands
  sub-agents** on demand (a real supervisor → specialist-tool-agents pattern; each
  sub-agent is a full Strands `Agent` with its own system prompt, tools, and model). Builds
  each sub-agent on the *same model provider as the orchestrator* (local oMLX by default;
  a cloud provider if `MODEL_PROVIDER` says so), server-side, in code — see "Why
  `local_swarm` and not `swarm`" below for why that's not just a style choice. `local_graph`
  is its parallel counterpart for independent sub-tasks.
- `skills` — Strands' native [Agent Skills](https://agentskills.io) plugin
  (`strands.vended_plugins.skills.AgentSkills`), loaded from `backend/skills/`. Each skill
  is a directory with a `SKILL.md` (YAML frontmatter + markdown instructions) — same shape
  as Claude Code's own skills, and only its name+description cost anything in the base
  system prompt; full instructions load on activation. Three are included:
  `project-memory` (check `agent_memory` before claiming ignorance), `sub-agent-team`
  (how to use `local_swarm`), and `skill-authoring` (below).

These tools run with **no confirmation gate at the process level** — `BYPASS_TOOL_CONSENT`
is set because a headless server has no TTY to prompt on — but `shell`, `python_repl`, and
`file_write` require a real human approval instead; see "AG-UI frontend tools &
human-in-the-loop" below. Default working directory is `backend/workspace/`; relative paths
stay inside it, absolute paths don't. Don't expose either front door beyond localhost.

**Tool-calling reliability is fundamentally model-dependent — this is the ceiling, not a
bug to keep chasing.** The local R1-distill model sometimes narrates a tool call in prose
(or fake pseudo-syntax) instead of actually invoking it. This was true from the start for
multi-parameter tools (`agent_memory`), and testing across this session showed it's not
simply a function of prompt length or tool count either: trimming the system prompt back
down and moving guidance into on-demand skills did *not* make a fresh-thread `run pwd in
shell` call reliably again. Every capability here is verified working when actually
invoked — direct Python tests for `agent_memory`, live shell execution earlier in this
session, `local_swarm`/`skills` confirmed registered, frontend tools and HITL confirmed
registered and visible in CopilotKit's own Inspector (see below) — the gap is specifically
this model's own judgment about *when* to call a tool versus describe one. A stronger local
model (or Bedrock/Anthropic, swappable in `agent_common.py`) would close this gap; no
amount of prompt/skill engineering here can substitute for model capability.

## Sub-agents: why `local_swarm` and not `swarm`

The raw `strands_tools.swarm` tool needs every sub-agent spec to carry a
`model_provider`/`model_settings` block pointing at oMLX — a nested JSON structure the
small local model reliably failed to reproduce (it would narrate the JSON as a code block
instead of calling the tool). `local_swarm` takes a simpler schema (`name`, `system_prompt`,
optional `tools`, optional `tier`) and injects the model config in Python, where it can't be
gotten wrong — `swarm` is itself a real `@tool`-decorated function, directly callable as
plain Python, so `local_swarm` just calls it with the completed specs. The
`sub-agent-team` skill documents the simplified schema for the model.

Sub-agents follow the active **model provider** (`backend/sub_agent_common.py`): with
`MODEL_PROVIDER=openai_compatible` (e.g. Ollama Cloud) they run on the cloud too, with the
same API key — none of their own — and each can run at a model **tier** (`fast` /
`standard` / `strong`, `MODEL_TIER_*`), chosen by the spec or by Laya's complexity
estimate. See [`docs/laya-triage.md`](docs/laya-triage.md).

## Model providers, API keys, and Laya

- **Provider.** `MODEL_PROVIDER` = `local` (oMLX, default) | `anthropic` |
  `openai_compatible` (`CLOUD_OPENAI_BASE_URL` / `CLOUD_OPENAI_MODEL_ID`; e.g. Ollama
  Cloud at `https://ollama.com/v1`). Set in the environment or `backend/.env`.
- **API keys never go in `.env`** (the agent's file tools can read it). Store one once in the
  macOS Keychain with `make key` (hidden prompt, then verifies the key against Ollama;
  the Keychain service is `sierralab-ollama-api-key`, or `sierralab-anthropic-api-key` for
  Anthropic) — or export
  `OLLAMA_API_KEY` / `CLOUD_OPENAI_API_KEY` / `ANTHROPIC_API_KEY` (an exported variable wins).
  The Keychain is only consulted when that provider is the active one. The backend reads the key once, removes it
  from its own environment, and masks it in every tool result so no agent can read it
  back — `THREAT_MODEL.md` ("Provider API keys") says exactly what that covers and what it
  doesn't (`python_repl` in `auto` mode).
- **Tool subprocesses can be routed through the Agent Vault** (`vault.enabled`, default off):
  they hold placeholder credentials (`OPENAI_API_KEY=__openai_api_key__`) and reach the network
  through a local proxy that substitutes the real value on the way out, so a command that prints
  or exports its environment leaks a marker. The vault token comes from the Keychain
  (`sierralab-agent-vault-token`), never a settings file. If the vault is enabled and unreachable,
  the default `vault.egressPolicy: deny` refuses `shell`/`python_repl`/`http_request` in **every**
  mode rather than falling through to direct egress — `docs/settings.md` ("Agent Vault") and
  `THREAT_MODEL.md` ("Agent Vault is egress-only"; it is not a filesystem sandbox).
- **Laya** (`LAYA_TRIAGE_MODE`, default `route`; `LAYA_GUARD_ENABLED`) is a fast local
  classifier: it triages each turn (an advisory routing hint above a confidence gate, and a
  model tier for sub-agents) and optionally scores untrusted tool output for injection.
  The LLM still does all generation and tool use. Full design, tuning, the log/report,
  and the fine-tuning loop: [`docs/laya-triage.md`](docs/laya-triage.md).

## The agentic harness

On top of the model loop sits a harness layer that runs the session like a coding-agent CLI
would: specialists (`architect` plans changes, `debugger` fixes failures — dispatched directly
on a confident Laya route), an autonomous self-healing loop (a failing command becomes a
diagnosed, sandbox-verified patch and a verification re-run), a team pipeline (a confident
"team" route decomposes the task and runs it as a parallel Strands graph), language-server
diagnostics, per-project MCP plugins, and remote agents over the A2A protocol (`a2a_client`).
What runs with and without a human gate, and the deliberate limits of each autonomous piece, is
in [`docs/agentic-harness.md`](docs/agentic-harness.md) and [`THREAT_MODEL.md`](THREAT_MODEL.md).

## Terminal clients (Rust)

`tui/` holds an interactive terminal client (`acp-tui`, a Claude-Code-style UI over ACP — it prints
into your terminal's own scrollback, so you scroll with the wheel as usual)
and a concurrent stress runner (`acp-stress`) for this backend — see
[`tui/README.md`](tui/README.md). `make tui`, `make tui-test`, `make stress`.

### The daemon (sessions that outlive the terminal)

By default `acp-tui` spawns `backend/acp_server.py` itself and the session ends with the terminal.
`make daemon` runs the supervisor daemon (`backend/daemon/`) instead: it spawns **one child per
project directory**, keeps it alive when a client disconnects, and replays what the client missed —
including an approval prompt the child is already blocked on. Attach to one with:

```bash
make daemon                                    # foreground; make daemon-install for a LaunchAgent
make tui ARGS="--transport ws"                 # attaches by cwd, or starts a session there
```

The daemon binds loopback only, requires the token in `~/.sierralab/daemon.json` (0600), and rejects
any WebSocket handshake carrying a browser `Origin`. It proxies ACP frames **verbatim** — it never
parses and re-encodes them — so `--transport ws` and the default `--transport stdio` are
semantically identical, and no protocol change here needs a matching change there. If the *child*
died rather than the client, the approval it was parked on is gone with it: `--resume` says so from
the ledger (`state.db`) instead of quietly dropping the question. See
[`THREAT_MODEL.md`](THREAT_MODEL.md).

While nobody is attached the daemon can watch the projects itself (`observer.enabled`, off by
default): it reads each one with a fixed `git` command list, hands that to a read-only agent, and
writes `~/.sierralab/nightly_briefing.md`. The agent holds `file_read` and `think` and nothing
else, so the zero-write rule is structural rather than a prompt — and a client attaching kills a
cycle in flight. A deploy hook or CI job can feed it with `POST /observe`. Keys and limits:
[`docs/settings.md`](docs/settings.md); what it can and can't do:
[`THREAT_MODEL.md`](THREAT_MODEL.md), "The background observer writes nothing".

Preferences work the way Claude Code's and Kilo's do: layered `settings.json` files (user / project /
local), `allow` / `ask` / `deny` permission rules, "always allow" from the approval prompt, and
`SIERRALAB.md` instructions — `/config`, `/permissions`, `/trust`, `/init` in the TUI. See
[`docs/settings.md`](docs/settings.md); what is and isn't at parity with Claude Code, Kilo CLI and
DeepSeek-TUI is in [`docs/harness-parity.md`](docs/harness-parity.md).

## Ported skills

`backend/skills/` also carries 43 skills ported from a personal Claude Code skill library
(`~/.claude/skills/`) — engineering methodology (`tdd`, `code-review`,
`diagnosing-bugs`, `domain-modeling`, ...), a full spec-driven-development system
(`spec`/`build`/`check`/`backprop`/`deepen`/`grill`/`review`), and writing/planning skills.
Available identically on both front doors — `AgentSkills` loads everything under
`backend/skills/` regardless of which one is talking to the agent. Ecosystem-specific
skills (AWS, Cloudflare, Google ADK CLI, a separate `ripwire` tool) were left out since
this agent doesn't have their underlying CLIs/SDKs; three more were left out for
referencing Claude-Code-only mechanisms with no equivalent here (spawning the `claude`
CLI, `.claude/settings.json` hooks, a hardcoded path from a different machine). See
`backend/skills/PORTED_FROM_CLAUDE_CODE.md` for the full exclusion list and the
Claude-Code-tool-name → this-agent's-tool-name translation table every ported skill
assumes (`Read`→`file_read`, `Bash`→`shell`, `Task`→`local_swarm`/`local_graph`, etc.).

## Skills the agent can write for itself

`skill-authoring` (`backend/skills/skill-authoring/SKILL.md`) teaches the agent the
`SKILL.md` format and naming rule, and tells it to use `file_write` (which — like any real
file write — needs human approval via the HITL flow below) to add a new skill when it has
actually solved something worth remembering, not speculatively. A new skill takes effect
starting the *next* conversation, since `AgentSkills` discovers skills once at agent
construction, not mid-thread. This is real, narrow self-improvement — crystallizing a
hard-won pattern into something the agent's future self reads automatically — not
closed-loop self-healing: it cannot detect its own errors, diagnose them, patch its own
Python, and verify the fix without a human in that loop, and the skill explicitly tells it
not to try rewriting tool code itself.

**Real bug this surfaced and fixed:** skills (and any plugin) were silently inactive on the
web path the whole time before this — `ag_ui_strands` explicitly does not forward
`plugins=` from the template `Agent` to the per-thread agents it derives per request (it
says so in its own source: "Agent consumes the list during init... there is no list to
read back"). Fixed by passing `plugins=build_plugins()` to `StrandsAgent(...)` itself in
`main.py`, not to the template `Agent()` in `agent_common.py`. `conversation_manager` does
not have this problem — Strands exposes it back on the agent under its own attribute name,
which `ag_ui_strands`'s generic per-thread-forwarding probe picks up automatically.

## AG-UI frontend tools & human-in-the-loop

`frontend/app/AgentTools.tsx` is the one place both features are registered — must render
under `<CopilotKitProvider>`:

- **Frontend tools** (`useFrontendTool`): a tool CopilotKit declares client-side, which
  `ag_ui_strands` automatically proxies into the Strands agent's tool registry every
  request (`RunAgentInput.tools` → `sync_proxy_tools`) — no backend wiring needed beyond
  what's already in `main.py`. One is included, `toggle_theme`, which calls the same
  function the sidebar's dark-theme checkbox uses.
- **Human-in-the-loop** (`useHumanInTheLoop`): `main.py` marks three tools
  `interrupt_on_call=True` via `ag_ui_strands`'s `ToolBehavior` — `shell`, `python_repl`,
  `file_write`, the ones with real destructive/execution capability. Each call pauses
  server-side (a native Strands intervention/interrupt, not a custom mechanism) until the
  matching `useHumanInTheLoop` render responds with `{approved: true | false}` — an
  Approve/Deny card in the chat. This is real HITL, not a demo: the tool genuinely does not
  run until a human clicks Approve. This is also the fix for the standing risk this README
  used to flag ("no confirmation gate") — those three tools now have one; the rest
  (`file_read`, `calculator`, etc.) stay frictionless since they're not destructive.

Verify either is wired without depending on the model actually calling anything: open the
CopilotKit Inspector (sidebar toggle) → **Frontend Tools** — `shell`, `python_repl`,
`file_write`, and `toggle_theme` all list there with a `handler` tag, confirming
registration independent of whether this particular local model chooses to call them.

## Agent-playable browser games

The agent can write a real, self-contained HTML/JS game (via `file_write`, same
approval-gated path as everything else) and open it in a live panel next to the chat —
`open_game` announces it, `backend/games_static_api.py` serves it, `frontend/app/
GameSandbox.tsx` renders it in a sandboxed `<iframe>`. A small `postMessage` contract
(documented in `backend/skills/build-a-game/SKILL.md`, with copy-pasteable boilerplate) lets
the agent read and drive the game live — `get_game_state`/`apply_game_move`, both
`useFrontendTool`s — alongside a human's own direct clicks in the same panel. `open_game`
statically checks the written file actually implements that contract before declaring the
game ready, since a model asked for "a tic-tac-toe game" will happily write one that never
calls the bridge at all. Two sub-agents can play each other, with the top-level agent
relaying moves — sub-agents have no path to call frontend tools themselves (see
`sub_agent_common.py`'s own tool-exclusion comments), so this is a hard architectural
constraint, not a missing feature.

## Context management

Both `Agent` builders in `agent_common.py` get a `SummarizingConversationManager`
(`strands.agent.conversation_manager`) — a native Strands feature, not custom code. Once a
thread grows past its window, older messages get summarized instead of just dropped
(`preserve_recent_messages=10`), using the agent's own oMLX model to write the summary (no
separate model needed). This also helps the oMLX prefill memory-guard issue documented
earlier — a long thread's prefill stays bounded instead of growing every turn.

## Conversation history & memory

Every thread/session gets its own [`FileSessionManager`](https://github.com/strands-agents/sdk-python)
writing to `backend/sessions/` — full conversation state persists across backend restarts,
keyed by the AG-UI `threadId` (web) or the ACP `session_id` (editor).

**Long-term semantic memory** is `agent_memory` (`backend/agent_memory_tool.py`): store,
retrieve (semantic search), list, delete — backed by the Qdrant container (below) and a
local `sentence-transformers` embedder, no cloud. This replaces an earlier attempt at
`strands_tools.mem0_memory`, which turned out to be broken against the installed
`strands-agents`: both its class-based tool and its standalone function register as plain
Python callables (not real `AgentTool` instances), and the current tool registry has no
case for that — a version-skew bug between `strands-agents` and `strands-agents-tools`,
not something to hand-patch. `agent_memory` sidesteps it entirely by using Strands' own
`@tool` decorator directly, the same mechanism `shell`/`editor`/etc. use.

## Conversation history sidebar

CopilotKit ships a ready-made `CopilotThreadsDrawer` — it requires **CopilotKit
Intelligence**, their cloud (or enterprise-self-hosted) thread-storage service, gated by a
license key. Plain self-hosted `CopilotRuntime` only exposes free, read-only REST endpoints
(`GET /api/copilotkit/threads`, `GET /api/copilotkit/threads/{id}/messages` — list +
inspect, no rename/archive/delete). `frontend/app/ThreadSidebar.tsx` is a small hand-built
sidebar against those free endpoints: list threads, switch between them (resumes real
history), start a new one. No CopilotKit account, no license key.

This required migrating the frontend from `@copilotkit/react-core` + `@copilotkit/react-ui`
(v1) to `@copilotkit/react-core/v2`, which exposes `CopilotChatConfigurationProvider` +
`setActiveThreadId`/a controlled `threadId` prop — the supported way to point `<CopilotChat>`
at a specific thread. (`useThreads`/`CopilotThreadsDrawer` in v2 are also Intelligence-gated;
only the lower-level `threadId` control is free.)

## Inspecting & changing agent config

Toggles in the sidebar footer, both native — no custom inspector built here:

- **CopilotKit dev tools** — toggles `enableInspector` on `CopilotKitProvider`. This is
  CopilotKit's own built-in Inspector: live AG-UI event log, per-agent state, thread
  contents, and a Playground tab to send test messages directly — the intuitive,
  ready-made way to see and poke at what's actually happening between the frontend and
  the agent, with zero custom code.
- **Dark theme** — toggles a `dark` class on the app root, Monokai-colored for our own
  chrome (sidebar, page background). CopilotKit's own chat component colors come off a
  shared Tailwind color-scale token set reused across its whole tree; reskinning those
  globally to Monokai risked breaking contrast in parts of the UI we don't render
  ourselves, so its own (reasonable) dark palette is left alone — the Monokai treatment is
  scoped to what we built.

For the agent's own config/state on the Python side, Strands agents expose their tool
registry, system prompt, and plugins as plain attributes (`agent.tool_registry`,
`agent.system_prompt`, `agent._plugin_registry`) — inspectable from a Python REPL or
`python_repl` tool call; there's no separate Strands-native visual config UI beyond that.

## Run

`scripts/setup.sh` + `scripts/dev-up.sh` (Quick start, above) do everything below
automatically. This is what they're actually running, if you'd rather do it by hand or want
to understand what each piece needs:

Terminal 0 — Qdrant (for `agent_memory`), local dev only:

```bash
podman machine start   # skip if using docker instead — same run command either way
podman run -d --name sierralab-agent-qdrant -p 6333:6333 -p 6334:6334 \
  -v sierralab-agent-qdrant-data:/qdrant/storage:z docker.io/qdrant/qdrant:latest
```

Terminal 1 — backend (needs your model server already running, see Quick start's
prerequisites and `backend/.env.example`):

```bash
cd backend
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
cp .env.example .env
python main.py
# http://localhost:8001 — AG-UI, for the web frontend
```

Terminal 2 — frontend:

```bash
cd frontend
npm install
npm run dev
# http://localhost:3000
```

Open `http://localhost:3000` — sidebar (threads + dark/inspector toggles) + chat.

### Running the whole stack on a separate always-on machine

Optional, for a small always-on LAN deployment (this repo's own setup, referred to below as
"the mini"): keep only the model server (oMLX or equivalent) on whichever machine actually
has the GPU/NPU, and run everything else — frontend, backend/agent loop, and Qdrant — as
Docker containers on a separate always-on box. This decouples uptime of the web UI from that
inference machine being awake; the inference machine only needs to be reachable when an agent
turn is actually running, not for the UI or memory store to be up.

`deploy/mini/` has example Docker Compose files for all three (`frontend/`, `backend/`,
`qdrant/`). `scripts/deploy-mini.sh` (or `make deploy-mini`) syncs `backend/` and `frontend/`
to the always-on box and rebuilds both containers there — `scripts/deploy-mini-backend.sh` /
`scripts/deploy-mini-frontend.sh` deploy just one side. First run on a fresh box bootstraps
each `docker-compose.yml` from the `deploy/mini/` template and stops so you can fill in:

- `backend`'s `OMLX_BASE_URL` — the inference machine (mDNS hostname or LAN IP), **not**
  the always-on box itself
- `backend`'s `QDRANT_HOST` — normally the always-on box's own hostname, since Qdrant lives
  there too by default
- `frontend`'s `BACKEND_URL` — the always-on box's own hostname, since the backend now lives
  there too
- **`SANDBOX_API_TOKEN`**, set to the same value in both `backend`'s and `frontend`'s compose
  files — required as soon as frontend and backend are different containers (even on the same
  host): the frontend's normal same-machine auto-discovery
  (`GET /sandbox/_local-token`, localhost-only) can't cross a container boundary. Generate one
  with `openssl rand -hex 32`.

Every redeploy after the first leaves each box's own `docker-compose.yml` alone, so these
customized values survive; only the source under them gets synced and rebuilt. Backend state
(`sessions/`, `workspace/`, `repl_state/`, `skills_generated/`, `mcp_servers.json`) lives in
bind-mounted `state/` dirs next to `docker-compose.yml` on the box itself, and persists across
redeploys the same way.

Put a reverse proxy in front of the frontend if you want a real domain instead of a bare
IP/mDNS hostname; set `EXTRA_CORS_ORIGINS` (`backend/.env.example`) to match once you do.

Note this is separate from local development: `scripts/dev-up.sh` (`make up`) still runs
backend + frontend + Qdrant directly on your own machine for coding/testing, unaffected by
whatever's deployed on the always-on box. The ACP (Zed) front door below is also unaffected —
Zed spawns `acp_server.py` locally on whichever machine is running the editor, always, regardless
of where the web deployment lives.

### ACP (editor) front door

No separate server process to manage per client — an ACP client (Zed, etc.) spawns this
itself and talks JSON-RPC over its stdin/stdout. Same agent definition as the web front
door, with real parity on the things that matter for using it day to day:

- **Tool-call visibility** — every tool call shows up in Zed's UI as a real tool-call card
  (`session/update` with `ToolCallStart`/`ToolCallProgress`), not just the model's prose.
- **Session modes** (`session/set_mode`) — the same three-way split as Claude Code's own
  agent: **Plan** (every tool call is denied — the model describes what it would do
  instead of doing it), **Build** (default — dangerous tools pause for approval, the rest
  run immediately), **Auto** (nothing pauses, ever — opt-in only, never the default).
- **Human-in-the-loop approval** (Build mode) — `shell`, `python_repl`, `file_write`,
  `editor`, and any `skills_generated/*` tool pause for a real Zed permission prompt
  before running, via Strands' own `strands.interventions` system (`_ApprovalHandler` in
  `acp_server.py`) translated into ACP's native `session/request_permission` — the same
  tools main.py gates behind an approval card on the web. Each prompt is staged in
  `~/.sierralab/state.db` before it is shown and closed with the answer, with Laya's risk
  score and band alongside it, so "what was asked, who approved it, under which rule" is
  answerable later and a turn that resumes does not ask the same call twice.
- **Model & temperature as native config-option dropdowns** (`session/set_config_option`)
  — reuses the exact same live model-swap mechanism `model_control_tool.py`'s
  `set_active_model`/`set_temperature` tools use, just surfaced as a picker in Zed's UI
  instead of a chat instruction.
- **Session titles** — the first message of a new conversation sets the session title
  Zed shows in its thread list, instead of a generic placeholder.
- **`@`-mention / resource content** — a file mentioned or attached in Zed used to be
  silently dropped (only plain text blocks were read); embedded file content now reaches
  the model directly, and a bare reference becomes a pointer the model can act on with
  `file_read`.
- **Session resume** (`session/load`) — reopening a past conversation replays its full
  history into Zed's chat view from the same on-disk `FileSessionManager` the web front
  door uses, keyed by session id. (Session mode resets to Build on reload — it isn't
  persisted across restarts, only conversation history is.)

**Zed setup** — add an entry under `agent_servers` in `~/.config/zed/settings.json`
(absolute paths, so it works regardless of Zed's own working directory):

```json
"agent_servers": {
  "sierralab-agent": {
    "type": "custom",
    "command": "/absolute/path/to/sierralab-agents/backend/.venv/bin/python3",
    "args": ["/absolute/path/to/sierralab-agents/backend/acp_server.py"],
    "env": {}
  }
}
```

No restart needed — Zed picks up settings changes automatically. It'll then show up as an
agent you can start a chat with, same as any other ACP agent.

To verify the protocol path without a real editor: `backend/_acp_smoke_test.py` spawns
`acp_server.py` and drives initialize → new_session → prompt through six cases (plain
reply, a gated tool approved, one denied, Plan mode blocking a tool outright, Auto mode
skipping the prompt entirely, and a live model switch via `set_config_option`), printing
the `ToolCallStart`/permission-request/title/final-text results for each —
`cd backend && source .venv/bin/activate && python _acp_smoke_test.py`.

## Extending toward the full architecture

- Swap `agent_common.py`'s `OpenAIModel` for a Bedrock model to test against real Claude/Nova — agent and tool code don't change.
- The Okta O4AA inbound-JWT check and the Lambda/STS outbound token exchange (boundaries ① and ② in the diagram) are deliberately not here — this repo is local-only, no IdP wired up.
- Containerizing for AgentCore Runtime later needs ARM64 + the HTTP/AG-UI container contract (`/ping`, `/invocations`) — `create_strands_app` already gives you an AG-UI-shaped app; the container/IAM/network work is additive, not a rewrite.
