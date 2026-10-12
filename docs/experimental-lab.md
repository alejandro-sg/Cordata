# The Experimental Lab

Ollama Cloud models through the **local** Ollama daemon, a per-role reasoning budget, native
Strands model routing, and a live control deck for every agent in a session.

```
make tui-lab        # TUI on this provider; F2 (or /lab) opens the Lab
```

This covers Phases 1–5 (the Lab *inside* the TUI). For what came after — the Claude Code CLI as a
tool, local Prism inference under the unified-memory budget, and the native GPUI window over the
gRPC bridge — see [`meta-harness.md`](meta-harness.md).

Contents: [Provider](#provider) · [Reasoning budget](#reasoning-budget) ·
[Routing and swapping](#routing-and-swapping) · [The Lab view](#the-lab-view) ·
[Protocol](#protocol) · [Daemon stream](#daemon-stream) · [What is and isn't true](#what-is-and-isnt-true) ·
[Tests](#tests)

## Provider

`MODEL_PROVIDER=ollama_cloud` (`backend/ollama_cloud.py`). The model is Strands'
`OpenAIModel` pointed at the daemon's OpenAI-compatible proxy, `http://localhost:11434/v1`
(`OLLAMA_BASE_URL`), with the dummy key `ollama` — the daemon ignores it and talks to Ollama
with its own `ollama signin` identity, so there is no key to store, find or leak.

| alias | wire id (what the daemon is asked for) | hosting |
|---|---|---|
| `glm-5.3-flash` | `glm-5.3-flash:cloud` | cloud |
| `kimi-k2.7-code` | `kimi-k2.7-code:cloud` | cloud |
| `laguna-xs-2.1` | `laguna-xs-2.1` | **local weights only** |

Two things worth knowing, both verified against ollama 0.35:

* **The `:cloud` suffix is required.** `glm-5.3-flash` alone is a 404 on the daemon; only
  `glm-5.3-flash:cloud` is offloaded.
* **`laguna-xs-2.1` has no cloud tag.** It is a 20 GB local model (33B MoE / 3B active). Serving
  it means `ollama pull`, which this harness never does. It is registered, but usable only if
  you installed it yourself; otherwise building it raises `ModelUnavailable` with the reason, a
  router leaves it out, and the model picker doesn't offer it.

"No local pulls" is enforced, not hoped for: the model's HTTP transport (`EndpointGuard`) lets
exactly `POST <base>/chat/completions` through and raises on anything else — `/api/pull`,
`/api/chat`, another host, a redirect. The one other request the module makes is the read-only
`GET /api/tags` (is the local model installed?) and `GET /api/version` (is the daemon up?).

## Reasoning budget

`reasoning_effort` is `low | medium | high | max`, sent in `extra_body` (so it lands at the top
level of the request JSON whatever the SDK's own enum allows). It is read at request time from a
`ReasoningControl`, so it can change between agent-loop iterations:

```python
model.set_reasoning_effort("max")                      # next request
agent.hooks.add_hook(bundle.throttle(lambda event: ...))  # per model call, from a policy
```

Roles carry a default budget and model (`ollama_cloud.ROLES`, overridable with
`OLLAMA_ROLE_<ROLE>=<model>[:effort]` or `ROLES.set(...)`):

| role | model | effort |
|---|---|---|
| orchestrator | glm-5.3-flash | low |
| architect | glm-5.3-flash | high |
| debugger | kimi-k2.7-code | max |
| observer | glm-5.3-flash | low |

(The architect's model isn't specified anywhere; glm is a default, not a finding.)

## Routing and swapping

`build_models(role)` returns a Strands `ModelRouter` over the usable candidates with a
`RoleStrategy`: the opening candidate is `invocation_state["ollama_model"]`, else the model the
caller pinned, else the agent's role profile; after a failed call it defers to Strands'
`FallbackStrategy`. Specialists and the observer are built this way (`sub_agent_common.
build_sub_agent_model(tier, role=...)`), and a configured `MODEL_TIER_*` beats the role table.
The orchestrator is a single model so `/model` and `agent.model` keep their meaning; the model
picker lists the registry (cloud tags, plus laguna only if installed) and switches through
`switch_model`.

> The harness doesn't serve A2A — `a2a_client` only *consumes* remote agents, which choose
> their own models — so "swap during delegation" here means the orchestrator → specialist
> delegation (agents-as-tools) and the Laya/team dispatches.

## The Lab view

`F2` or `/lab` in the TUI. The live region becomes the canvas (the transcript stays in your
scrollback). Keys are in `tui/README.md`.

* **Containment pods** — one card per agent with a status badge: `[IDLE 💤]`, `[SYNTHESIZING 🧪]`
  (model call in flight), `[EXECUTING ⚡]` (tool running — also a caller whose delegate is parked
  on you), `[CONTAINED ⚠️]` (waiting on a human), `[ISOLATED 🛑]`; model, effort, dial, calls.
  Team-pipeline nodes appear as pods too.
* **A2A links** — `orchestrator ━━━▶ debugger`, pulsing while live: delegations (agent-as-tool),
  Laya dispatches, team-pipeline `depends_on` edges.
* **Threat gauge** — Laya's destructiveness score (0–1) and its confidence for the pending call.
  The score is the *turn's* reading (`laya_risk.py`), shown as context, never as a clearance.
* **Memory radar** — the selected pod's recent reads: LanceDB vector search (`trace_store`),
  knowledge-graph reads (`kg/`: embedded sink or Cognee), Qdrant `agent_memory`.
* **Monitor** — the model and `reasoning_effort` *as sent on the wire*, busy time, process CPU/RSS
  (process-wide: agents share one), attached MCP server names.
* **Inspector** (Enter) — system prompt, tool trace (operator actions in magenta), live stream,
  queued guidance.
* **HITL Intervention Station** — a pending approval with its diff (`d` for all of it):
  `[Y] Approve & Execute`, `[N] Reject & Discard`, `[K] E-Stop` (reject + isolate the pod that asked).

## The menu bar

`make lab-menubar` (`lab/crates/harness-menubar`) is the same harness as a menu-bar app: an
accessory app (`NSApplicationActivationPolicy::Accessory`, so no Dock icon and no window on
launch) with a status item and a deck.

* **The icon is the state**, from the same telemetry the Lab draws (`harness_client::deck`):
  a thin ring when idle, a filled cyan disc while a pod is executing, an amber disc when
  something is waiting for you. The idle glyph is a *template* image so macOS tints it for a
  light or dark menu bar; the other two are never tinted, because their colour is the message.
  The badge count lives in the tooltip, not on the glyph.
* **Three states, no fourth.** Offline is not a menu-bar state — the daemon being unreachable
  asks nothing of the person, so it is said in words in the deck's status strip rather than
  guessed from a colour.
* **The deck** (`⌘⇧Space`, or a click) is a 420×520 popover pinned under the status item: the
  state in words, the approvals that *interrupt*, the three newest notices, and a composer that
  submits a turn over the bridge (`SubmitTurn`). Esc closes the composer first, then the deck.
  A click outside closes it too — polled from AppKit, never observed, because a global event
  monitor would cost Accessibility, the same permission the capture needs.
* **The hotkey is Carbon `RegisterEventHotKey`**, so it needs neither Accessibility nor Input
  Monitoring. `⌘⇧Space` is the default; if another app holds it the next binding in the list is
  tried, and if every one is taken the failure is a chip in the deck rather than a keystroke
  that silently does nothing. Clicking the status item always works.
* **Approvals must not block, here either.** The same rule as the Lab's containment modal
  (`deck::interrupts`, one implementation shared by both): a `risk_score >= 0.8`, or an unscored
  `armed` request, opens the deck by itself; a `0.58` one waits in the list and never takes over.

Two front doors may run at once — the daemon already arbitrates them (latest-wins telemetry,
intervention fan-out, first answer wins). Only the Lab runs the workspace observer.

### Screen context

The deck can attach what you are looking at to a turn — and only what you allowed:

1. **Summon it over the app** (hotkey or click). The deck remembers which app was frontmost — the
   one thing that costs no permission — and takes the keyboard (closing it hands the keyboard back).
2. **Capture** (or `c`). Off by default: the first time, the deck names the app and offers
   **Always allow <app>**, which adds its bundle id to `capture.allowlist` in your *user* settings.
   Password managers, Keychain Access and Passwords never get that button.
3. **The checklist** shows each field as it would be sent — `✓ window title`, `✓ selected text (2
   masked)`, `✗ selected text — Accessibility is not granted [Grant Accessibility]` — already
   masked, so you approve what goes, not what will be masked later. **Don't send** drops it.
4. **Add screenshot** (optional) captures that app's frontmost window only (ScreenCaptureKit,
   ≤2048 px, ≤4 MiB JPEG), and says where it may go: a local vision model, or a cloud one only with
   `capture.screenshotCloud`. It is used only when `capture.visionModel` names a model; the turn
   runs on it once and the image is removed from the history afterwards.
5. **Send** — the context goes with this turn only, as wrapped untrusted data (`os_context.py`),
   and the daemon logs what arrived: `grpc: submit_turn … context app='Safari' (com.apple.Safari)
   title='…' selection=42 chars '…'`.

Permissions are macOS's: Accessibility for the title and selection, Screen Recording for the image.
Grant them to a bundled build — `make lab-menubar-app INSTALL=1` builds `~/Applications/Cordata.app`
(`LSUIElement`, signed; set `SIGN_IDENTITY` to a code-signing certificate so a grant survives
rebuilds — ad-hoc signing makes every rebuild a new app to macOS). Policy and its limits:
`THREAT_MODEL.md`, "OS context capture"; settings: `docs/settings.md`, "Screen context".

**Checking it.** `make lab-menubar-selftest` cycles the three icons and prints where AppKit placed
the status item (the window server's own list does not report status items to an unprivileged
process). `make lab-menubar-deck-selftest` opens the deck over whatever app is frontmost and prints
whether it became key, which app it recorded as the target, the backdrop (Liquid Glass or the blur
fallback) and the frontmost app after it closed. `make lab-live` drives a capture (from a fake OS
source) through the real Rust client and Python bridge and reads the daemon's log and ledger. What
no script can check — the real Carbon hotkey over another app, the TCC prompts, the glass's look,
a real selection from Safari — is a hand check:

```bash
make lab-menubar-app INSTALL=1
```

then open `~/Applications/Cordata.app`, grant it Accessibility, select text in Safari, press
`⌘⇧Space`, Capture, Always allow, type a prompt, Enter, and read `~/.cordata/daemon.log` (or the
demo's `daemon.log`) for the `submit_turn` line.

## Protocol

Backend → client: the ACP extension notification `_cordata/lab`, always a **full snapshot**
with a monotonic `seq` — a dropped one is healed by the next, and a client ignores any whose
`seq` is older than the one it has (notifications are written from concurrent tasks, so two
can swap):

```jsonc
{ "event": "state", "version": 1, "seq": 42, "ts": …, "sessionId": "…", "focus": "debugger",
  "pods":  [{ "id", "name", "role", "parent", "model", "effort", "status", "isolated",
              "dial", "dial_default", "calls", "busy_ms", "pending_tool", "guidance_pending",
              "messages", "trace": […], "memory": […], "stream",
              /* only for the focused pod: */ "system_prompt", "guidance": […] }],
  "links": [{ "from", "to", "kind", "active", "count", "label" }],
  "threat": { "pod", "tool", "score", "confidence", "band", "pending" } | null,
  "hitl":  [{ "id", "pod", "tool", "input", "status", "risk" }],
  "resources": { "cpu_percent", "rss_mb", "scope": "process", "mcp": […] } }
```

Client → backend, ACP extension requests (each recorded in `state.db` as a `lab` event first):

| method | params | effect |
|---|---|---|
| `_cordata/lab/state` | – | the snapshot |
| `_cordata/lab/select` | `pod` \| null | which pod's detail rides on snapshots |
| `_cordata/lab/isolate` | `pod` | E-Stop the pod and its descendants |
| `_cordata/lab/release` | `pod` | lift the isolation (and any stale cancel) |
| `_cordata/lab/dial` | `pod`, `value` | clamped to [0.30, 0.80]; only tightens |
| `_cordata/lab/inject` | `pod`, `text` | merged into the pod's next model call |

Approving or rejecting a staged call is the ordinary ACP permission response — the Lab draws
that request, it doesn't replace it.

## Daemon stream

`backend/daemon/` taps `_cordata/lab` frames as they pass (the frame still reaches the driving
client byte for byte) and keeps the newest per session:

* `GET /lab` → `{"sessions": {id: snapshot}, "observer": pod | null}`
* `WS /lab/ws?token=…[&session=id]` → `{"type":"snapshot", …}` once, then `{"type":"state",
  "sessionId", "state"}` per change (and `{"type":"observer", …}`); **read-only**, any number of
  watchers, never displaces the driving client.

Same rules as `/ws` (loopback, token, **no browser origins** — so a browser tab can't be the canvas;
use a native client). The observer is a daemon child process, not a pod in any session, so the
daemon contributes its pod (only when `observer.enabled`).

## What is and isn't true

* **E-Stop refuses; it doesn't kill.** Pods are agents inside one process, with no process of their
  own and no per-agent proxy ticket. See `THREAT_MODEL.md`, "The Experimental Lab".
* **The dial only tightens** the gate; the router's own destructive gate stays authoritative.
* **`reasoning_effort` is on the wire, its effect is the provider's.** Ollama accepts any value
  (even nonsense) without an error, documents `low/medium/high` (and `none` to turn thinking off),
  and `max` is passed through; on a one-line arithmetic question the reasoning length wasn't
  monotonic in the level. The harness guarantees what it *sends*, not how a cloud model spends it.
* **Emoji widths.** The badges use emoji (`⚠️` is a text symbol plus a variation selector);
  terminals disagree about its width, which can shift a card's right edge by a column.
* **Per-pod CPU/memory isn't measurable** in one process; the monitor says "(process)".

## Tests

All of these run offline unless marked live; none needs the cloud.

| file | covers |
|---|---|
| `backend/_ollama_cloud_smoke_test.py` | registry, request shape (`/v1/chat/completions`, bearer, `:cloud`), endpoint guard, `reasoning_effort` low→max between invocations and loop iterations, role budgets, router (role pick, per-call swap, failover), picker/config swap, provider wiring |
| `backend/_lab_smoke_test.py` | statuses from real Strands loops, links, E-Stop (mid-loop, parked, idle), dial, guidance, HITL, redaction |
| `backend/_lab_memory_smoke_test.py` | the radar against the real LanceDB/KG stores |
| `backend/_lab_acp_smoke_test.py` | the Lab through the real ACP server: threat + HITL, dial changes the gate, E-Stop while parked, ledger |
| `backend/_e2e_lab_smoke_test.py` | **Phase 5, deterministic**: orchestrator (glm/low) → debugger (kimi/max) → destructive call at the HITL station, picker swap, E-Stop — real provider over HTTP to `fake_ollama.py` |
| `backend/_e2e_lab_tui_smoke_test.py` | the **real `acp-tui` binary in a pty** + a real backend child + the fake daemon: F2, Y/N/K, release, dial, guidance → the next request |
| `backend/_daemon_smoke_test.py` | the `/lab` tap and read-only socket |
| `tui/` `cargo test --workspace` | event parsing, keys → requests, rendering (incl. through the emulated terminal), approvals-don't-block-notifications |
| `backend/_ollama_cloud_live_test.py`, `backend/_e2e_lab_live_test.py` | **live**: the real daemon, real glm/kimi, real Laya; asserts nothing was pulled |
| `lab/` `cargo test --workspace` (`make lab-test`) | the tray's three states and the tiering rule (`0.58` does not take over, `0.93` does), the hotkey's fallback list and its failure message, the deck's placement arithmetic (edges, a second display, a deck taller than the space under the icon) and the deck itself in headless GPUI: rows, composer, hotkey-error chip, Esc |

Not covered by any test, and checked by hand instead: the real `NSStatusItem`, the real Carbon
hotkey over another app, `--deck-selftest` (a popover window with a transparent background cannot
be created headlessly), and TCC prompts later.
