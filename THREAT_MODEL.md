# Threat Model

This document states the trust boundary and the real gates behind it, so a
contributor (or anyone deciding whether to run this) doesn't have to
reconstruct that picture from scattered code comments first.

## Trust boundary

This is a **local-only, single-user agent harness with real host access** —
not a hardened multi-tenant service. There is no login, no accounts, no
per-user permission model. Whoever can reach the frontend origin *is* the
user, with the same capability the person running it has. This is
deliberate — the whole design (README's "no cloud calls, everything runs on
localhost") assumes one person, one machine or small home LAN, not a
service exposed to strangers.

The threat model does not try to stop the person running this from using
its own tools (`shell`, `file_write`, etc.) — that's the product. It does
try to stop:

- A request that skips the frontend entirely (a raw `curl`, another device
  on the LAN) from reaching a tool that's supposed to require a human's
  approval click.
- Content the model reads from outside the conversation — a fetched web
  page, a recalled memory that itself originated from one — being treated
  as an instruction instead of data.
- A model-generated tool (`skill_forge_tool.py`'s output under
  `backend/skills_generated/`) running before a human has actually looked
  at it, twice over.
- A sub-agent (`local_swarm`/`local_graph`) reaching a capability with no
  human-in-the-loop gate of its own, since sub-agents have no approval UI
  to pause into at all.

## Human-in-the-loop gates

Both front doors gate the same four tools before they run for real —
`shell`, `python_repl`, `file_write`, `editor` — plus any tool loaded from
`backend/skills_generated/`, by name, dynamically:

- **AG-UI (web)** — `main.py`'s `ToolBehavior(interrupt_on_call=True)`,
  implemented by `ag_ui_strands`'s `StrandsInterruptHook`. The call pauses
  server-side; the frontend renders an Approve/Deny card
  (`frontend/app/AgentTools.tsx`) and the tool only runs once a human clicks
  Approve. Verified live, repeatedly, that this cannot be bypassed by a
  denied click, a stale approval, or a tool call racing ahead of the card.
- **ACP (editor)** — `acp_server.py`'s own `_ApprovalHandler`, wired through
  Strands' `interventions` system, surfaced as a native Zed permission
  prompt. Same four tools, same "the call genuinely does not execute until
  approved" property, independent implementation from the AG-UI path (no
  shared trust between the two front doors — each enforces its own). The
  client's answer only approves if the selected `option_id` maps to an
  allow-kind option the server offered (`_permission_approved`); a selected
  "Reject", an unknown id, or a cancelled outcome all deny.

**Generated tools get a second, independent gate on top**, not just the
`file_write` approval that put them on disk:
`skill_validation_hooks.py`'s `SkillValidationHooks` AST-parses any
`skills_generated/*.py` tool for a narrow banned list (`subprocess` in any
form, `eval`/`exec`, `os.system`) and dry-runs it with synthetic arguments
before its first real call is allowed to proceed — a human approving the
*file write* is not the same as a human having verified the *tool's
behavior*, and this closes that gap. Trust is in-memory only and resets on
restart, on purpose — `skills_generated/` is an audit trail, not an
unconditional allowlist.

**Sub-agents get none of the above** — `local_swarm`/`local_graph` build
plain `strands.Agent` instances directly in Python
(`sub_agent_common.py`), with no AG-UI/ACP transport wrapping them and
nowhere to pause for a human. `sub_agent_common.DENIED_TOOLS` (`shell`,
`python_repl`, `file_write`, `editor`) and `ORCHESTRATOR_ONLY_TOOLS`
(`local_swarm`, `local_graph`, `skill_forge`, `proposer`, the
model/sandbox-mode control tools, `open_game`) are refused to every
sub-agent regardless of what a spec asks for, or omits — the default
(`"tools"` unset) hands a sub-agent every tool *not* on either list rather
than erring toward "give it everything." `resolve_tools` also excludes
every tool `ag_ui_strands.client_proxy_tool.registered_proxy_names()`
reports (`toggle_theme`, `get_game_state`, `apply_game_move`, and any
future `useFrontendTool`) — these get proxied into the top-level agent's
own tool registry, and their proxy body unconditionally returns a fake
`"success"` with no real effect; a sub-agent given one wouldn't just lack a
capability, it would silently report doing something that never happened.

## Network-edge gate: SANDBOX_API_TOKEN

`main.py` binds `0.0.0.0`, not `127.0.0.1` — this deployment is meant to
support frontend and backend running on separate LAN machines, which means
"only reachable from localhost" was never actually true for
`/sandbox/*` (`sandbox_functions_api.py` — the `openGenerativeUI` bridge
functions) or `/games/*` (`games_static_api.py` — served game files).
Anything on the same LAN could call `runSnippet` (arbitrary Python)
directly and skip the frontend's client-side approval card entirely — the
card is real UI state, but on its own it was never a network-level gate.

`SANDBOX_API_TOKEN` closes that: every request to either route family must
carry the matching `X-Sandbox-Token` header. It is **never unset in
practice** — `settings.py` auto-generates a fresh `secrets.token_hex(32)`
at process start whenever it isn't explicitly configured, so a checkout
with zero setup is still gated, not silently open. The frontend discovers
that auto-generated value itself, at runtime, via `GET
/sandbox/_local-token` — a route gated not by the token (asking for it
implies not having it yet) but by `_require_localhost`, which only answers
a caller whose own connection originates from `127.0.0.1`/`::1`. A
same-machine caller (the Next.js server process, in the common
single-machine setup) could already read `backend/.env` directly if it
wanted to, so hopping to `_local-token` instead of a shared file gives it
nothing new; a different machine on the LAN — exactly who this token
exists to keep out — gets a plain 403 with no token revealed, same as
every other gated route. Set `SANDBOX_API_TOKEN` explicitly instead once
frontend and backend really are on separate hosts; auto-discovery cannot
cross that hop by design.

## Prompt-injection hardening

`prompt_security.py`'s `UntrustedToolResultHooks` rewrites `http_request`'s
fetched content and `agent_memory`'s `retrieve`/`list` results in place,
via a Strands `AfterToolCallEvent` hook, before either ever reaches the
model: each gets wrapped in an explicit
`<<<UNTRUSTED_TOOL_OUTPUT>>>...<<<END_UNTRUSTED_TOOL_OUTPUT>>>` block with
a header stating the content may contain attempted instructions and should
be treated as data, not commands. The guard markers themselves are escaped
inside the wrapped text first, so fetched content containing the literal
marker strings can't forge a fake close and break out into unwrapped
context. `UNTRUSTED_CONTEXT_POLICY` states the same rule once, briefly, in
every agent's system prompt (top-level and sub-agent) as a second,
independent reinforcement that doesn't depend on any one tool call's own
wrapping.

Registered on every agent construction site that can actually reach
`http_request` or `agent_memory` — confirmed by grep, not assumed: `main.py`'s
`StrandsAgent`, `agent_common.build_agent_for_session` (ACP),
`sub_agent_common.build_agent_nodes` (every `local_swarm`/`local_graph`
node), and `skill_forge_tool.py`'s debugger sub-agent (the one place among
`skill_forge`/`proposer`'s internal sub-agents that gets `http_request` at
all — the rest only ever get `think`/`file_read`/`python_repl`, none of
which read external content). A shared `HookProvider`, not copied logic per
call site, so a *future* tool that reads untrusted external content still
needs to be wired to it explicitly — this is a real search-and-check, not
something that happens automatically just by adding `UntrustedToolResultHooks`
once, anywhere.

This is defense-in-depth, not a guarantee. A hook can mark content as
untrusted; it cannot force a language model to actually disregard
instructions embedded in it. The realistic goal is raising the bar against
casual/naive injection attempts, not eliminating the underlying risk that
any tool ingesting arbitrary external text carries.

### Sub-agent output is wrapped too

`local_swarm`/`local_graph` results are wrapped in the same guard block
(`prompt_security._SUBAGENT_TOOLS`) before the orchestrator reads them. A
sub-agent's own tool results were already wrapped inside *its* context, but
its final reply is model-written text built from what it read — returned
unwrapped, an injected instruction a sub-agent repeated in its summary would
reach the orchestrator looking trusted.

## Provider API keys

The cloud-provider keys (`CLOUD_OPENAI_API_KEY`, alias `OLLAMA_API_KEY`;
`ANTHROPIC_API_KEY`) must never be readable by an agent. Without care they
are, in four ways: `shell` children inherit the process environment,
`file_read`/`cat` can open `backend/.env`, `ps eww` prints a process's
startup environment, and module globals are one `import` from `python_repl`.
`secret_guard.py` + `prompt_security.SecretRedactionHooks` close those:

- **Sourced from the shell environment only.** `settings.py` reads each key
  once via `take_secret`, wraps it in a `Secret` (prints `Secret(****)`; only
  `reveal()` returns it, called at the one place a model client is built) and
  removes it from `os.environ`, so a `shell` child never inherits it. The
  backend logs a warning if the key is found in `backend/.env`.
- **Never persisted.** `provider_settings.set_provider_config` no longer
  writes API keys to `.env`: a key set from the settings UI lasts until the
  process restarts. Export it in the launching shell for a durable key.
- **Masked in every tool result.** `SecretRedactionHooks` replaces the literal
  value (and any credential-looking environment variable's value — `*KEY*`,
  `*TOKEN*`, `*SECRET*`, `*PASSWORD*`) with `[REDACTED:secret]` in every tool
  result, success or error, before the model or any UI sees it, at every
  agent construction site (`main.py`, the ACP front door, every
  `local_swarm`/`local_graph` node, `skill_forge`'s debugger/validator,
  `proposer`'s writer). This is what makes `env`, `cat .env` and `ps eww`
  harmless even when the read succeeds — `ps eww` *does* still show the key on
  macOS, because popping `os.environ` can't change the startup environment
  block. A tool call whose *input* already contains the literal value is
  cancelled.
- **Checked live** against the real backend and a cloud model in `auto` mode
  (no approval gate), prompting for the key via `env`, `ps eww`,
  `python_repl` (`os.environ`, `agent_common.ACTIVE_CLOUD_API_KEY`,
  `.reveal()`) and `file_read .env`: 5,074 streamed events and every persisted
  session file held no key. `_secret_guard_smoke_test.py` covers the layers
  without a model, including a real Strands agent whose scripted model is
  shown to receive only the masked text.

**What this does not do.** It is not a sandbox. `python_repl` executes in the
backend process, so code it runs can still call `Secret.reveal()` and send the
value out in a form a literal match can't see (encoded, split across calls).
That tool is approval-gated in `build` mode and denied to sub-agents, but
`auto` mode removes the gate — so in `auto` mode a determined or
prompt-injected agent with `python_repl` is the residual risk. Closing it
means running `python_repl` out of process, with no key in that process; not
done. Output masking also only sees literal values, and the model's own
reply is not the only channel a leaked value could take (`http_request`
bodies are not scanned beyond the literal-in-input check). Rotate any key that
has been pasted into a chat or a shell history.

### Sub-agent providers and Laya triage

- **Sub-agents follow the active provider.** `sub_agent_common.build_sub_agent_model`
  goes through `agent_common.build_model`, so with a cloud `MODEL_PROVIDER` a
  `local_swarm`/`local_graph` team's prompts and tool results also go to that cloud
  (previously sub-agents were hardwired to local oMLX). The orchestrator was
  already sending its own context there; a team adds more of the same. With the
  default `local` provider nothing leaves the machine. Sub-agents use the
  orchestrator's key through the same client builder — they get no key of their
  own, and it never enters their tools, prompts or environment
  (`_sub_agent_model_smoke_test.py`).
- **The Laya routing hint is a fixed string** chosen by Laya's enum answer
  (`laya_triage._ROUTE_HINT`), added to the system prompt for one turn and
  restored — it contains no text from the user or from any tool result, so it is
  not an injection channel, and it is not persisted. It is advisory: it cannot
  grant a tool, skip an approval, or change a mode. Laya itself is a classifier
  that can be wrong; the confidence gate (`LAYA_TRIAGE_THRESHOLD`) bounds how often
  it speaks, and zero-shot it is near chance (docs/laya-triage.md).
- **The triage log holds user prompts** (`backend/laya_triage.jsonl`, gitignored,
  secret-masked by `secret_guard.redact`). It sits on disk where the agent's file
  tools can read it — it contains only what the user already typed to that agent.
  Turn triage `off` if prompts must not be written down.

## Response headers

- **Frontend** (`frontend/next.config.js`) — `X-Frame-Options: DENY` +
  `Content-Security-Policy: frame-ancestors 'none'` on every page route,
  preventing this app's own UI from being framed by a third-party site
  (clickjacking). Deliberately *excluded* from `/api/games/*` and
  `/api/sandbox/*` — those exist specifically to be loaded into this app's
  own `<iframe sandbox="allow-scripts">` (`GameSandbox.tsx`, the
  generative-UI renderer), and a frame-blocking header on the content
  meant to be framed would just break that feature. `X-Content-Type-
  Options: nosniff` and `Referrer-Policy: no-referrer` apply everywhere,
  no exception needed — neither interacts with framing.
- **Backend** (`security_headers.py`) — the same `nosniff`/`no-referrer`
  pair on every response. No frame-related header here: this backend never
  serves the actual UI a human looks at (every response is either JSON or
  a file meant to be framed by the frontend), so there's no page here for
  a frame-blocking header to protect.
- **Sandboxed iframes** — both `GameSandbox.tsx` and the existing
  generative-UI renderer use `sandbox="allow-scripts"` with no
  `allow-same-origin`, giving the framed content an opaque origin. That
  means `postMessage` handlers can't authenticate a sender by
  `event.origin` (it always reads the literal string `"null"`) —
  `gameSandboxRegistry.ts` authenticates by `event.source` identity
  (comparing against the exact `iframe.contentWindow` a message was sent
  to) instead.

## Known gaps

Acknowledged, not fixed:

1. **No filesystem or process sandbox.** Once a human approves `shell` /
   `python_repl` / `file_write`, it runs directly as this process's own
   user, with no network-egress filtering or filesystem confinement beyond
   the tools' own default-relative-path convention (an absolute path can
   still leave `backend/workspace/`). Approval is real, but it's the only
   containment — a human approving a genuinely malicious or badly-reasoned
   command has no second line of defense.
2. **No rate limiting** on any route, including the token-gated ones —
   `SANDBOX_API_TOKEN` stops an unauthenticated caller, not a slow brute
   force by one who's guessing, though a 32-byte hex token makes that
   impractical regardless.
3. **`agent_memory`'s Qdrant-backed store is unencrypted at rest.**
   Whatever gets stored via `agent_memory` (`backend/agent_memory_tool.py`)
   sits in plain form in Qdrant's own on-disk storage — reasonable for a
   personal local deployment, worth knowing before storing anything
   sensitive through it.
4. **No accounts, so no per-capability scoping.** Every capability this
   harness has is available to whoever can reach it and has (or can fetch)
   the sandbox token — there's no way to grant a narrower slice, unlike a
   multi-user system with real roles. Fine for the one-person-one-machine
   case this is built for; would need real auth before being anything
   else.
5. **CSP has no script-src lockdown yet** — `frame-ancestors 'none'` is
   the only directive set. A stricter policy (restricting script/style
   sources) is a reasonable follow-up but needs auditing what the frontend
   actually loads first, to avoid silently breaking something.
