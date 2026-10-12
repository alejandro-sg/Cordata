"""Zero-trust egress: run tool subprocesses through the Agent Vault proxy.

The problem this exists for. A `shell` command the model decided to run, or an
`http_request` it composed, can carry a credential the harness itself never
touched — because the credential is in the environment of the process that runs
it. `secret_guard` removes this harness's *own* provider keys from `os.environ`
at startup, and that covers the harness's own calls, but it cannot cover a
command that reads a key stored somewhere else (a `.env` in a project checkout,
`~/.aws/credentials`, an exported variable the user set before `make tui`).

Agent Vault closes that on **egress**: tool subprocesses do not hold real
credentials at all. They hold *placeholders* — `OPENAI_API_KEY=__openai_api_key__`
— and every outbound request goes through a local proxy that swaps the
placeholder for the real value, on the way out, for destinations the vault's
policy allows. A command that prints its environment, writes it to a file, or
sends it somewhere it shouldn't has exfiltrated nothing but a marker.

Three properties this module is built to hold:

- **Fail closed.** `vault.egressPolicy: "deny"` (the default) means an
  unreachable vault stops egress entirely — `blocks()` returns true and
  `_ApprovalHandler` denies the tools that could reach the network. There is no
  silent fallthrough to direct egress, which is the failure this whole thing
  exists to prevent: a proxy variable that has quietly stopped being set looks
  exactly like a working setup.
- **The token is never in a settings file.** It is read through
  `secret_guard.take_secret`, from the Keychain item named by
  [`KEYCHAIN_SERVICE`][], which is where `settings_store`'s schema comment
  insists credentials live. `vault.token` is deliberately not a setting.
- **TLS is re-pointed, not switched off.** The proxy terminates TLS with its own
  CA, so every client that goes through it must be told where that CA is
  (`SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `GIT_SSL_CAINFO`, `NODE_EXTRA_CA_CERTS`,
  `REQUESTS_CA_BUNDLE`). Nothing here disables verification
  (`NODE_TLS_REJECT_UNAUTHORIZED=0` and friends) — a proxy that needs that is
  not a proxy worth routing through.

Scope, stated plainly because it is easy to over-read: this closes *credential
exposure on egress*. It is not a filesystem or process sandbox — an approved
`shell` command still runs as the user, with the user's filesystem access. See
`THREAT_MODEL.md` ("Agent Vault is egress-only").

Off by default (`vault.enabled` is false), so a checkout with no vault installed
behaves exactly as it did before this module existed.
"""

from __future__ import annotations

import logging
import os
import shutil
import socket
from dataclasses import dataclass, field
from typing import Any, Mapping, Sequence
from urllib.parse import urlsplit

import secret_guard

logger = logging.getLogger(__name__)

#: Where the vault's proxy listens. `vault.addr` overrides it; the default is
#: the port the vault's own quickstart uses.
DEFAULT_ADDR = "http://127.0.0.1:14322"

#: The Keychain item holding the vault token (`security add-generic-password -a
#: "$USER" -s cordata-agent-vault-token -w`). Never a settings key — see the
#: module docstring.
KEYCHAIN_SERVICE = "cordata-agent-vault-token"

#: The vault binary, and the hook the smoke test uses to point at a stub.
BINARY_ENV = "CORDATA_AGENT_VAULT_BIN"
DEFAULT_BINARY = "agent-vault"

#: Comma- or space-separated extra placeholder names (see `placeholders()`).
PLACEHOLDERS_ENV = "CORDATA_VAULT_PLACEHOLDERS"

#: Names a tool subprocess may hold a placeholder for, so the vault can swap in
#: the real value on the way out.
DEFAULT_PLACEHOLDERS = ("OPENAI_API_KEY", "GITHUB_TOKEN")

#: Names this harness reads for *its own* calls, which therefore must never be
#: given a placeholder: `settings.py` takes `ANTHROPIC_API_KEY`,
#: `CLOUD_OPENAI_API_KEY` / `OLLAMA_API_KEY` at import, and `HF_TOKEN` is
#: registered for masking. A placeholder under one of these names is not
#: harmless — the harness would present `__openai_api_key__` to the provider as
#: if it were a key, which reads as an authentication failure rather than as a
#: vault misconfiguration. Excluded here, whoever asks for them.
RESERVED = ("ANTHROPIC_API_KEY", "CLOUD_OPENAI_API_KEY", "OLLAMA_API_KEY", "HF_TOKEN")

#: Which tools can reach the network at all. `shell` and `python_repl` can run
#: anything; `http_request` is named after what it does. Plugin and MCP tools are
#: not in this list because their egress is not knowable statically — a plugin
#: that ships its own HTTP client is outside what this can decide, which is
#: exactly why the vault proxy is pointed at the *process*, not at a tool.
#: `a2a_client` calls a remote agent and `skill_forge`'s researcher sub-agent holds `http_request` — both were missing
#: from this list until the Phase 11 audit, so a blocked vault did not stop them.
EGRESS_TOOLS = frozenset({"shell", "python_repl", "http_request", "web_fetch", "claude_code_engineer", "orchestrate", "a2a_client", "skill_forge"})
#: The egress tools a sub-agent can be handed (swarm, graph, team pipeline): dropped from its kit while egress is blocked.
SUB_AGENT_EGRESS_TOOLS = frozenset({"http_request", "web_fetch"})

_LAST: "VaultStatus | None" = None


def egress_blocked() -> bool:
    """Whether the vault this process installed (or last re-probed) refuses egress. For code that builds agents with
    no approval gate of their own — a sub-agent's kit is decided here, not at a prompt."""
    return _LAST is not None and _LAST.blocks_egress


def _remember(status: "VaultStatus") -> "VaultStatus":
    global _LAST
    _LAST = status
    return status

#: How long to wait for the vault to accept a connection. Local, so this is
#: generous; the cost of being wrong is a session that starts without egress.
HEALTH_TIMEOUT = 0.5

#: Loopback and the vault itself never go through the proxy: the harness's own
#: model server (oMLX on 127.0.0.1), the local services, and the vault's own
#: admin API would otherwise be proxied back into the thing doing the proxying.
LOCAL_NO_PROXY = "127.0.0.1,localhost,::1"


@dataclass
class VaultStatus:
    """What `install()` did, and what the approval gate should make of it."""

    enabled: bool = False
    ok: bool = True
    credentialed: bool = False
    addr: str = ""
    reason: str = ""
    env: dict[str, str] = field(default_factory=dict)

    @property
    def blocks_egress(self) -> bool:
        """Whether tools that can reach the network must be refused.

        Only ever true when the user turned the vault *on*: with it off, this
        module is inert and egress is whatever it always was.
        """
        return self.enabled and not self.ok


def addr_from(preferences: Any = None) -> str:
    """`vault.addr`, falling back to [`DEFAULT_ADDR`][]. A setting of "" is
    treated as unset rather than as "no address" — the key exists in the schema,
    so its blank default must not mean "broken"."""
    try:
        configured = (preferences.get("vault.addr") or "").strip() if preferences is not None else ""
    except (AttributeError, TypeError):
        configured = ""
    return configured or DEFAULT_ADDR


def _policy(preferences: Any = None) -> str:
    """`deny` (refuse egress when the vault is unreachable) or `proxy` (still
    route through the configured address, without the vault's credentials)."""
    try:
        policy = preferences.get("vault.egressPolicy", "deny") if preferences is not None else "deny"
    except (AttributeError, TypeError):
        return "deny"
    return policy if policy in ("deny", "proxy") else "deny"


def placeholders(environ: Mapping[str, str] | None = None) -> dict[str, str]:
    """`{name: placeholder}` for the credentials a tool subprocess may carry.

    Placeholder values are the name, lowercased, wrapped in double underscores —
    the vault's own convention, and recognisable at a glance in a leaked
    environment dump.
    """
    env = environ if environ is not None else os.environ
    extra = (env.get(PLACEHOLDERS_ENV) or "").replace(",", " ").split()
    names: list[str] = []
    for name in (*DEFAULT_PLACEHOLDERS, *extra):
        if name in RESERVED:
            logger.warning("vault: refusing a placeholder for %s — this harness reads it for its own calls", name)
            continue
        if name and name not in names:
            names.append(name)
    return {name: f"__{name.lower()}__" for name in names}


def ca_bundle(environ: Mapping[str, str] | None = None) -> str | None:
    """The vault's CA certificate, when one has been exported for us.

    `CORDATA_VAULT_CA` names the file. There is no default path: a guessed CA
    path that silently does not exist turns every TLS client into a failure, and
    one that does exist but is the *wrong* CA is worse. Without it the proxy
    variables are still set — a vault that mints credentials per request and
    does not re-sign TLS needs no CA file — and the caller is warned.
    """
    env = environ if environ is not None else os.environ
    path = (env.get("CORDATA_VAULT_CA") or "").strip()
    return path or None


def _merged_no_proxy(environ: Mapping[str, str] | None = None) -> str:
    """Loopback plus whatever the user already excluded.

    Replacing `NO_PROXY` outright would quietly route an internal registry or a
    corporate host the user had deliberately kept off a proxy through the vault —
    and, with the vault's policy denying it, break it. Their entries stay; ours
    are added."""
    env = environ if environ is not None else os.environ
    entries: list[str] = []
    for raw in (env.get("NO_PROXY"), env.get("no_proxy"), LOCAL_NO_PROXY):
        for item in (raw or "").replace(" ", ",").split(","):
            if item and item not in entries:
                entries.append(item)
    return ",".join(entries)


def build_proxy_env(
    addr: str | None = None,
    *,
    environ: Mapping[str, str] | None = None,
    placeholders_too: bool = True,
) -> dict[str, str]:
    """The environment a subprocess needs to reach the network through the vault.

    Proxy variables in both cases (curl, git, pip and node each look at a
    different spelling of them), the CA bundle in every variable the common TLS
    stacks consult, and — unless `placeholders_too` is false — the placeholder
    credential names. `NO_PROXY` keeps loopback and the vault's own address off
    the proxy, so the harness's local model server does not become a proxied
    request.
    """
    target = (addr or DEFAULT_ADDR).rstrip("/")
    no_proxy = _merged_no_proxy(environ)
    out = {
        "HTTP_PROXY": target,
        "HTTPS_PROXY": target,
        "http_proxy": target,
        "https_proxy": target,
        "NO_PROXY": no_proxy,
        "no_proxy": no_proxy,
        "NODE_USE_ENV_PROXY": "1",
    }
    bundle = ca_bundle(environ)
    if bundle:
        out.update(
            {
                "SSL_CERT_FILE": bundle,
                "REQUESTS_CA_BUNDLE": bundle,
                "CURL_CA_BUNDLE": bundle,
                "GIT_SSL_CAINFO": bundle,
                "NODE_EXTRA_CA_CERTS": bundle,
            }
        )
    if placeholders_too:
        out.update(placeholders(environ))
    return out


def check_health(addr: str | None = None, *, timeout: float = HEALTH_TIMEOUT) -> bool:
    """Whether something is accepting connections at the vault's proxy address.

    A TCP connect, not an HTTP request: the vault's *proxy* port speaks CONNECT,
    and a GET to it would be a request the vault has to make a policy decision
    about. `addr` may be a URL or a bare `host:port`; anything unparseable is
    unhealthy, because a mistyped address must not read as "fine, skip the
    proxy".
    """
    host, port = split_addr(addr or DEFAULT_ADDR)
    if host is None:
        return False
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


def split_addr(addr: str) -> tuple[str | None, int | None]:
    """`("127.0.0.1", 14322)` from `http://127.0.0.1:14322`, `127.0.0.1:14322` or
    `https://vault.internal`. A URL with no port falls back to 443/80 by scheme;
    a non-numeric port is `(None, None)`."""
    text = (addr or "").strip()
    if not text:
        return None, None
    if "//" not in text:
        text = f"//{text}"
    parts = urlsplit(text)
    host = parts.hostname
    if not host:
        return None, None
    try:
        if parts.port is not None:
            return host, parts.port
        return host, 443 if parts.scheme == "https" else 80
    except ValueError:  # a port that is not a number
        return None, None


def binary() -> str | None:
    """The vault executable, or None when it is not installed."""
    override = (os.environ.get(BINARY_ENV) or "").strip()
    if override:
        return override if os.path.exists(override) else None
    return shutil.which(DEFAULT_BINARY)


def launch_under_vault(argv: Sequence[str], *, exe: str | None = None, vault_name: str | None = None) -> list[str]:
    """`argv`, wrapped so it runs under the vault.

    Raises `VaultUnavailableError` rather than returning the bare `argv`: a
    caller that has asked for the vault and silently gets direct execution is
    the fail-open case this module exists to prevent.

    `vault_name` is `vault.vault` — which vault the credentials are drawn from —
    and it is passed as `--vault`. The vault CLI is not installed in this
    checkout, so that flag is the one line to check against `agent-vault run
    --help` the first time someone runs this with a name configured.

    There is no caller in this repo yet. `shell` and `python_repl` come from
    `strands_tools`, which spawns its own subprocesses and offers no argv to
    wrap, which is why `install()` mutates the environment instead; this is the
    API for a caller that does own the argv (a daemon-spawned child, a sandbox
    runner), and it exists so that caller cannot get a bare `argv` by mistake.
    """
    launcher = exe or binary()
    if not launcher:
        raise VaultUnavailableError(
            f"the Agent Vault CLI ({os.environ.get(BINARY_ENV) or DEFAULT_BINARY!r}) is not on PATH, "
            "so there is no way to run this under the vault"
        )
    return [launcher, "run", *(["--vault", vault_name] if vault_name else []), "--", *argv]


class VaultUnavailableError(RuntimeError):
    """The vault was asked for and could not be reached or started."""


def _route_through(addr: str, target: dict[str, str], token: str | None) -> VaultStatus:
    """Apply the proxy environment for a vault that answered, and say so."""
    applied = build_proxy_env(addr, environ=target)
    target.update(applied)
    if token is None:
        token = secret_guard.take_secret(keychain=KEYCHAIN_SERVICE).reveal()
    if token:
        # Not registered for masking: the token is the harness's own, and a
        # value in the environment of a tool subprocess is exactly what the
        # vault is meant to stop mattering. It is passed to the vault CLI,
        # never to the tools.
        target["CORDATA_VAULT_TOKEN_PRESENT"] = "1"
    # `credentialed` is a claim the TUI and the resume notice repeat to the
    # user, so it is only made when there is a token behind it: a reachable
    # vault with nothing in the Keychain item routes egress through the proxy
    # and substitutes nothing.
    return VaultStatus(
        enabled=True,
        ok=True,
        credentialed=bool(token),
        addr=addr,
        reason=(
            f"egress goes through {addr}"
            if token
            else f"egress goes through {addr}, but there is no token in the Keychain item "
            f"{KEYCHAIN_SERVICE!r}, so nothing will be substituted"
        ),
        env=applied,
    )


def reprobe(
    status: VaultStatus,
    *,
    environ: dict[str, str] | None = None,
    token: str | None = None,
) -> VaultStatus:
    """A blocked status, re-checked; anything else comes back unchanged.

    `install` runs once when a session starts, so a vault started afterwards
    would leave egress refused for the rest of the session — while the refusal
    text tells the user to start it. A call that is about to be refused asks
    again first. The same address is probed (a session does not get to
    re-point itself), and only a vault that now answers changes anything.
    """
    if not status.blocks_egress or not check_health(status.addr):
        return status
    return _remember(_route_through(status.addr, environ if environ is not None else os.environ, token))


def install(
    preferences: Any = None,
    *,
    environ: dict[str, str] | None = None,
    token: str | None = None,
) -> VaultStatus:
    """Wire the vault into this process, and say what happened.

    The environment is the only interception point available: `shell` and
    `python_repl` come from `strands_tools`, which passes its own environment to
    the subprocesses it spawns, so the proxy variables have to be in *our*
    environment by the time a tool runs. That also sets the ordering constraint
    worth knowing about — this must run after `settings.py` has taken the
    provider keys out of `os.environ` (it does; `acp_server` imports
    `agent_common`, which imports it, at the top). Running earlier would let a
    placeholder sit under a name the harness then reads for itself, which
    `RESERVED` refuses anyway, belt and braces.

    Returns a [`VaultStatus`][]; the caller decides what to do about a vault that
    is enabled and unreachable (`blocks_egress`), because only the caller knows
    which tools it is about to run.
    """
    target = environ if environ is not None else os.environ
    try:
        enabled = bool(preferences.get("vault.enabled", False)) if preferences is not None else False
    except (AttributeError, TypeError):
        enabled = False
    if not enabled:
        return _remember(VaultStatus(enabled=False))

    addr = addr_from(preferences)
    policy = _policy(preferences)

    health_ok = check_health(addr)
    exe = binary()
    if health_ok and exe is None:
        # Reachable, but nothing to launch through it. A tool subprocess still
        # gets the proxy variables and works; only the wrapped form is missing.
        logger.warning("vault: %s is up at %s but no %s on PATH", DEFAULT_BINARY, addr, DEFAULT_BINARY)

    if health_ok:
        return _remember(_route_through(addr, target, token))

    if policy == "proxy":
        # The user asked to keep working without credentials rather than stop:
        # the address is still set, so nothing silently egresses *directly*,
        # which is the property that matters. What is lost is substitution.
        applied = build_proxy_env(addr, environ=target, placeholders_too=False)
        target.update(applied)
        return _remember(VaultStatus(
            enabled=True,
            ok=True,
            credentialed=False,
            addr=addr,
            reason=f"the vault at {addr} is unreachable; routing through it anyway without credentials",
            env=applied,
        ))

    return _remember(VaultStatus(
        enabled=True,
        ok=False,
        addr=addr,
        reason=f"the vault at {addr} is unreachable and vault.egressPolicy is 'deny'",
    ))
