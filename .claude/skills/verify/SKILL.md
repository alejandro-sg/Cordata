# Verifying this repo

Two runtime surfaces: the Python daemon (HTTP + WebSocket + gRPC) and the Rust clients
(`tui/`, `lab/`). Neither `cargo test` nor the `_*_smoke_test.py` suites are verification —
they're CI. Drive the real processes.

## Handle: the daemon, isolated

```bash
export VER=/tmp/slverify          # any scratch dir
mkdir -p $VER/home $VER/proj
CORDATA_HOME=$VER/home backend/.venv/bin/python -m daemon.main --port 8790 --grpc-port 8791
```

`CORDATA_HOME` isolates *everything* (token, state.db, settings) — no need for
`CORDATA_DAEMON_CONFIG`. Ports 8790/8791 avoid clobbering a real daemon on 8765/8766.
The token lands at `$VER/home/daemon.json`, mode 0600.

To exercise a *live session* without a model, point the daemon at the repo's own fake child
(real daemon, real bridge, real wire, canned agent):

```bash
CORDATA_ACP_ENTRYPOINT="$PWD/backend/.venv/bin/python $PWD/backend/fixtures/fake_acp_bridge_child.py"
```

Its control methods (see the fixture's docstring): `bridge/lab`, `bridge/permit`, `bridge/diff`,
`bridge/memory`, and it answers `_cordata/lab/{effort,dial,isolate}`.

## Driving the three front doors

* **HTTP** (`httpx`): `POST /sessions {"cwd": …}` opens one; `GET /health` is the liveness read.
  Every route needs header `x-daemon-token`.
* **WebSocket** (`websockets`): `ws://127.0.0.1:8790/ws?token=…&session=<sid>` — the *driving*
  front door. Send `{"jsonrpc":"2.0","method":"bridge/permit","params":{…}}` to make the child
  emit a permission request.
* **gRPC** (`grpc` + `backend/daemon/harness_pb2*`): the GUI's surface. Metadata
  `("x-daemon-token", token)`. `StreamTelemetry(session_id=…)`, `InterventionStream`,
  `ProactiveIngest`, `SetKnob`, `UnloadMemory`, `GetDiff`, `Ping`.

## Gotchas that cost time

* **A TUI answer is a bare ACP response**, not a method call:
  `{"jsonrpc":"2.0","id":<child id>,"result":{"outcome":{"outcome":"selected","optionId":"reject"}}}`
  sent over the WS. `{"method": "session/respond"}` is forwarded to the child and never admitted
  as an answer — the "first answer wins" race won't happen and you'll misread it as a bug.
* **`InterventionStream` replays every still-pending request on attach.** Read the event and check
  its `intervention_id`; a stale one from a previous run will otherwise be mistaken for the fresh
  request. Use a **fresh cwd per run** (sessions attach-or-create by directory).
* **For a bidi gRPC stub, feed decisions from a `queue.Queue`-backed generator.** Appending to a
  list after the generator has finished starves it and `next(events)` raises `StopIteration`.
* **The fake child logs `{"kind": "response", "result": {...}}`** for answers — `optionId` is
  nested under `result.outcome`, not a top-level field.
* **`risk_*` / `armed` on a request come from the child's `_meta.cordata`**, so a hand-built
  permit with no risk fields shows `armed=False, band='', score=0.0`. Put them in the meta.
* **Pipe buffering**: run drivers with `python -u` writing to a file; piping through `tail`/`head`
  hides all output until the process exits.

## The GPU/window

`lab/target/debug/harness-lab` opens a real window when a display exists. On a headless session
`screencapture` fails (`could not create image from display`) — don't report the GUI as broken for
that. The app's *behavior* is still observable: it watches `--watch <dir>`, parses saved files with
tree-sitter and ingests changed symbols, which lands as `kind='ingest'` rows in `$CORDATA_HOME/state.db`.

```bash
CORDATA_HOME=$VER/home lab/target/debug/harness-lab --watch $VER/proj &
sqlite3 $VER/home/state.db "select kind, count(*) from events group by kind;"
```

The daemon's ledger (`state.db` → `events`) is the ground truth for whether a GUI action landed:
kinds `gui` (approve/reject/isolate/knob/unload_memory), `ingest`, `response`.
