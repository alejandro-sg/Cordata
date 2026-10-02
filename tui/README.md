# sierralab-agents terminal clients (Rust, ACP)

Two binaries over one protocol library, all talking to `backend/acp_server.py`
(the same Agent Client Protocol server Zed uses):

| crate | what |
|---|---|
| `acp-core` | all protocol logic, no UI: spawn backend → `SessionHandle` → `AppEvent` stream; permission round-trip; untrusted-output detection |
| `acp-tui` | interactive ratatui client (Claude-Code-style): streaming chat, tool cards, approval modal, mode/model/temperature, `--resume` |
| `acp-stress` | scriptable concurrent runner: scenarios in, one JSONL line per prompt-turn + a summary out |

Built on the official [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol)
crate (pinned `=2.2.0`, default features only).

## Run

```sh
cd tui
cargo run -p acp-tui                         # new session in the current directory
cargo run -p acp-tui -- --cwd ~/some/project # agent works in that project
cargo run -p acp-tui -- --resume <session-id>
cargo run -p acp-stress -- scenarios/smoke.json -c 4 -o results.jsonl
```

`make tui`, `make tui-test`, `make stress SCENARIOS=tui/scenarios/stress.json` from the repo root do the same.

The backend is found automatically (`backend/.venv/bin/python` +
`backend/acp_server.py`, walking up from the binary, then the cwd); override
with `--backend-root` or `ACP_TUI_BACKEND_ROOT`. You need `scripts/setup.sh`
done and your oMLX model server running — without it every turn fails with
`Connection error`. The backend takes ~8 s to start (it loads models), so the
status bar shows `connecting` at first.

### Using a cloud model (and keeping its key from the agent)

Export the provider settings in the shell that launches the client — the
backend inherits them. The API key goes in the environment, never in
`backend/.env` (the agent's file tools can read that file):

```sh
export MODEL_PROVIDER=openai_compatible
export CLOUD_OPENAI_BASE_URL=https://ollama.com/v1
export CLOUD_OPENAI_MODEL_ID=gpt-oss:20b
export OLLAMA_API_KEY=...        # or CLOUD_OPENAI_API_KEY
cargo run -p acp-tui
```

The backend reads the key once, removes it from its own environment, and masks
it in every tool result, so `env`, `ps eww` or `cat .env` can't surface it —
see `THREAT_MODEL.md`, "Provider API keys" (including what that does *not*
cover: `python_repl` in `auto` mode). Neither client ever reads the key
itself, and `--debug-log` records ACP traffic and the server's stderr, neither of which carries it.

## acp-tui keys

| key | action |
|---|---|
| Enter | send · `Ctrl+J` / `Alt+Enter` newline |
| `a` / `r` (or Esc) | allow / reject a pending tool call (modal) |
| Ctrl+C | cancel the running turn (see below) · clears the input when idle |
| Tab / Shift+Tab | cycle mode forward / back: plan → build → auto |
| Ctrl+O (also Ctrl+M where the terminal distinguishes it) | model picker |
| Ctrl+T | temperature picker (3 presets) |
| Ctrl+E / Ctrl+R | expand all tool results / hide reasoning |
| PgUp / PgDn | scroll |
| Ctrl+Q | quit (always works) |

Deliberate behaviours worth knowing:

- **Ctrl+C is inert while the approval modal is open.** `session/cancel` is only
  checked between streamed events and the server is parked on your permission
  request, so cancelling then would do nothing. Answer the request first.
  *Rejecting a tool call does not abort the turn* — the model sees the denial and continues.
- **Mode colours are risk-ordered:** plan = green (nothing runs), build = yellow
  (dangerous tools ask), auto = red (nothing asks).
- **Ctrl+M can't be the model key in most terminals** (it is byte-identical to
  Enter), hence Ctrl+O.
- **`--resume`** shows text history only (the backend doesn't replay tool calls)
  and always comes back in `build` mode; a divider says so. The protocol has no
  session listing, so you need to know the id (it's in the status bar, and in
  `backend/sessions/session_<id>/`).
- **Untrusted output**: results the backend wraps in `<<<UNTRUSTED_TOOL_OUTPUT>>>`
  (http_request, agent_memory reads, MCP tools) get a magenta double-line card
  labelled `⚠ UNTRUSTED CONTENT`. All agent text is stripped of terminal escape
  sequences before drawing.

`--debug-log FILE` appends the raw JSON-RPC traffic and the server's stderr to a
file (never stdout — the TUI owns the terminal).

## acp-stress

```jsonc
{ "scenarios": [{
  "name": "shell-roundtrip",
  "prompts": ["Call the shell tool to run exactly: echo hi"],
  "cwd": ".",                       // default: runner's cwd
  "mode": "build",                  // plan | build | auto
  "model": "…", "temperature": "balanced",   // focused | balanced | creative
  "approval_policy": "auto_allow",  // auto_allow | auto_reject (default) | {"shell":"allow","*":"reject"}
  "repeat": 4                       // independent copies
}]}
```

- **One backend process per session**, always. The backend does a process-wide
  `os.chdir` per session, so sessions sharing a process would corrupt each
  other's working directory. Each process loads its own models: size `-c` to your RAM.
- `build` + `auto_allow` exercises the real permission request/response
  round-trip; `auto` mode exercises the model's tool calling without it. They
  stress different things.
- Output: one JSONL line per prompt-turn (`stop_reason`, `ttft_ms`, `latency_ms`,
  `tool_calls`, `permission_requests`, `permissions`, `error`, best-effort
  `context_usage`), then a summary (p50/p95, stop reasons, most common tool,
  grouped problems). Exit code is non-zero if any session errored or any turn
  ended in anything but `end_turn`. A session that dies still gets a line for
  every prompt (later ones are `skipped: …`).
- **Memory:** with Laya on (`LAYA_TRIAGE_MODE=route`, the backend default, and/or
  `LAYA_GUARD_ENABLED`), every backend process also loads its own ~650 MB Laya checkpoint.
  For big concurrent runs that only stress the harness, set `LAYA_TRIAGE_MODE=off
  LAYA_GUARD_ENABLED=false` in the launching shell. Per-turn triage and sub-agent tiers
  show up in `backend/laya_triage.jsonl` — see `docs/laya-triage.md`.
- Hung turns are cancelled after `--turn-timeout` and reported; Ctrl+C shuts
  every backend down cleanly. No token/cost accounting — ACP has no structured usage push.
- `scenarios/smoke.json` is the five cases from `backend/_acp_smoke_test.py`.
  Reading it back: approved → `completed` + 1 permission request · denied →
  `failed` + 1 · plan → `failed` + 0 · auto → `completed` + 0.

## Tests

```sh
cargo test --workspace
```

Protocol paths (streaming, permission allow/reject/drop, plan/auto, cancel,
replay, config options, crash, timeouts, concurrency, orphan-process check) are
tested end-to-end against `crates/acp-core/tests/fixtures/fake_agent.py`, a
scripted ACP agent, so they need no model server.

## Not in v1

Provider switching, free-float temperature, skills/MCP management, session
enumeration (add `list_sessions` to `acp_server.py` first — don't read
`backend/sessions/` off disk behind the protocol's back).
