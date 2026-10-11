"""Regression check for vault_proxy.py — the fail-closed egress contract.

Hermetic: the only socket this opens is a listener it makes itself, on an
ephemeral loopback port, and the only "unreachable" address it uses is a port it
just closed. The real Agent Vault is never started, the Keychain is never read
(an explicit `token` is passed), and `install` is handed its own `environ` dict
so the assertions can be exact about what was and was not written.

What it is checking, in one line: with `vault.enabled` on and the vault down,
nothing that could carry a credential off the machine runs — and nothing is
quietly left pointed at a proxy that is not there.

    backend/.venv/bin/python _vault_proxy_smoke_test.py
"""

import os
import socket
from types import SimpleNamespace

import vault_proxy


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


def listener():
    """A socket that accepts connections, so `check_health` has something real
    to be right about. Returns (addr, close)."""
    server = socket.socket()
    server.bind(("127.0.0.1", 0))
    server.listen(1)
    port = server.getsockname()[1]
    return f"http://127.0.0.1:{port}", server.close


def closed_addr() -> str:
    """A loopback address nothing is listening on: bind, note the port, close."""
    probe = socket.socket()
    probe.bind(("127.0.0.1", 0))
    port = probe.getsockname()[1]
    probe.close()
    return f"http://127.0.0.1:{port}"


class Prefs:
    def __init__(self, **values) -> None:
        self.values = values

    def get(self, key, default=None):
        return self.values.get(key, default)


ENABLED = Prefs(**{"vault.enabled": True})
ENABLED_DENY = Prefs(**{"vault.enabled": True, "vault.egressPolicy": "deny"})
ENABLED_PROXY = Prefs(**{"vault.enabled": True, "vault.egressPolicy": "proxy"})


def test_addr_parsing() -> None:
    check(vault_proxy.split_addr("http://127.0.0.1:14322") == ("127.0.0.1", 14322), "url with port")
    check(vault_proxy.split_addr("127.0.0.1:14322") == ("127.0.0.1", 14322), "bare host:port")
    check(vault_proxy.split_addr("https://vault.internal") == ("vault.internal", 443), "https url defaults to 443")
    check(vault_proxy.split_addr("vault.internal") == ("vault.internal", 80), "bare host defaults to 80")
    check(vault_proxy.split_addr("http://127.0.0.1:not-a-port") == (None, None), "a non-numeric port is unusable")
    check(vault_proxy.split_addr("") == (None, None), "empty addr is unusable")
    # A mistyped address must not read as "fine": the health check fails closed.
    check(vault_proxy.check_health("http://127.0.0.1:not-a-port") is False, "unparseable addr is unhealthy")

    check(vault_proxy.addr_from(Prefs()) == vault_proxy.DEFAULT_ADDR, "blank vault.addr falls back to the default")
    check(vault_proxy.addr_from(Prefs(**{"vault.addr": "  http://v:1  "})) == "http://v:1", "configured addr is stripped")


def test_health() -> None:
    check(vault_proxy.check_health(closed_addr()) is False, "a closed port is unhealthy")
    addr, close = listener()
    try:
        check(vault_proxy.check_health(addr) is True, "a listening port is healthy")
    finally:
        close()


def test_disabled_is_inert() -> None:
    env: dict[str, str] = {}
    status = vault_proxy.install(Prefs(), environ=env)
    check(status.enabled is False and status.ok is True and status.blocks_egress is False, "vault off -> inert")
    check(env == {}, "vault off -> environment untouched")


def test_healthy_sets_the_environment() -> None:
    addr, close = listener()
    try:
        env: dict[str, str] = {}
        status = vault_proxy.install(Prefs(**{"vault.enabled": True, "vault.addr": addr}), environ=env, token="stub")
        check(status.ok and status.credentialed and not status.blocks_egress, "healthy vault -> credentialed, not blocking")
        check(env["HTTPS_PROXY"] == addr and env["https_proxy"] == addr, "proxy set in both cases")
        check("127.0.0.1" in env["NO_PROXY"], "loopback stays off the proxy")
        check(env.get("NODE_USE_ENV_PROXY") == "1", "node clients honour the env proxy")
        check(env.get("OPENAI_API_KEY") == "__openai_api_key__", "placeholder credential is a marker, not a key")
        check("ANTHROPIC_API_KEY" not in env, "no placeholder under a name this harness reads itself")
        check("SSL_CERT_FILE" not in env, "no CA bundle is guessed when none was configured")

        withca = vault_proxy.build_proxy_env(addr, environ={"SIERRALAB_VAULT_CA": "/tmp/vault-ca.pem"})
        check(withca["SSL_CERT_FILE"] == "/tmp/vault-ca.pem" and withca["CURL_CA_BUNDLE"] == "/tmp/vault-ca.pem", "CA bundle reaches the TLS clients")
        check("NODE_TLS_REJECT_UNAUTHORIZED" not in withca, "TLS verification is never switched off")
    finally:
        close()


def test_placeholders_never_shadow_the_harness_keys() -> None:
    env = {"SIERRALAB_VAULT_PLACEHOLDERS": "ANTHROPIC_API_KEY, OLLAMA_API_KEY,CUSTOM_TOKEN,custom_token"}
    names = vault_proxy.placeholders(env)
    check("ANTHROPIC_API_KEY" not in names and "OLLAMA_API_KEY" not in names, "reserved names are refused even on request")
    check(names.get("CUSTOM_TOKEN") == "__custom_token__", "an operator may add placeholder names")
    check(list(names).count("CUSTOM_TOKEN") == 1, "a repeated name lands once")
    check("OPENAI_API_KEY" in names, "the default set is still there")


def test_unreachable_denies() -> None:
    env: dict[str, str] = {}
    status = vault_proxy.install(ENABLED_DENY, environ=env)
    check(status.blocks_egress is True, "unreachable + deny -> egress is blocked")
    check("unreachable" in status.reason and "deny" in status.reason, "the reason says which policy blocked it")
    check(env == {}, "unreachable + deny -> nothing is left pointing at a proxy that is not there")
    check(vault_proxy.install(ENABLED, environ={}).blocks_egress is True, "deny is the default policy")

    # `proxy` keeps working without credentials, but still routes through the
    # configured address rather than falling through to direct egress.
    env = {"HTTPS_PROXY": "http://corporate:8080"}
    status = vault_proxy.install(ENABLED_PROXY, environ=env)
    check(status.ok is True and status.credentialed is False, "unreachable + proxy -> usable, uncredentialed")
    check(env["HTTPS_PROXY"] == vault_proxy.DEFAULT_ADDR, "the configured address replaces any existing proxy")
    check("OPENAI_API_KEY" not in env, "no placeholder without a vault to substitute it")


def test_launch_refuses_without_the_vault() -> None:
    check(vault_proxy.launch_under_vault(["ls"], exe="/bin/echo") == ["/bin/echo", "run", "--", "ls"], "wrapped argv")
    check(
        vault_proxy.launch_under_vault(["ls"], exe="/bin/echo", vault_name="prod") == ["/bin/echo", "run", "--vault", "prod", "--", "ls"],
        "vault.vault is passed through as the vault to draw from",
    )
    previous = os.environ.get(vault_proxy.BINARY_ENV)
    os.environ[vault_proxy.BINARY_ENV] = "/nonexistent/agent-vault"
    try:
        try:
            vault_proxy.launch_under_vault(["ls"])
        except vault_proxy.VaultUnavailableError:
            check(True, "no vault binary -> refuse to launch, never run directly")
        else:
            check(False, "no vault binary -> refuse to launch, never run directly")
    finally:
        if previous is None:
            os.environ.pop(vault_proxy.BINARY_ENV, None)
        else:
            os.environ[vault_proxy.BINARY_ENV] = previous


def test_no_proxy_keeps_the_users_entries_and_credentialed_needs_a_token() -> None:
    env = vault_proxy.build_proxy_env("http://127.0.0.1:14322", environ={"NO_PROXY": "internal.corp,10.0.0.0/8"})
    parts = env["NO_PROXY"].split(",")
    check("internal.corp" in parts and "10.0.0.0/8" in parts, "a host the user excluded from proxying stays excluded")
    check("127.0.0.1" in parts and "localhost" in parts, "and loopback is added")
    check(len(parts) == len(set(parts)), "without duplicates")
    check(env["no_proxy"] == env["NO_PROXY"], "in both spellings")

    server = socket.socket()
    server.bind(("127.0.0.1", 0))
    server.listen(1)
    try:
        addr = f"http://127.0.0.1:{server.getsockname()[1]}"
        status = vault_proxy.install(
            Prefs(**{"vault.enabled": True, "vault.addr": addr}), environ={}, token=""
        )
        check(status.ok and status.credentialed is False, "a reachable vault with no token is not reported as credentialed")
        check("token" in status.reason.lower(), f"and the reason says why ({status.reason!r})")
    finally:
        server.close()


def test_a_blocked_session_recovers_when_the_vault_comes_up() -> None:
    server = socket.socket()
    server.bind(("127.0.0.1", 0))
    port = server.getsockname()[1]
    addr = f"http://127.0.0.1:{port}"
    server.close()  # nothing listening: the vault is down when the session starts
    blocked = vault_proxy.install(Prefs(**{"vault.enabled": True, "vault.addr": addr}), environ={}, token="t")
    check(blocked.blocks_egress, "a vault that is down blocks egress at session start")
    env: dict[str, str] = {}
    check(vault_proxy.reprobe(blocked, environ=env, token="t").blocks_egress, "and still does while it is down")
    check(not env, "re-probing a vault that is still down touches nothing")

    revived = socket.socket()
    revived.bind(("127.0.0.1", port))
    revived.listen(1)
    try:
        recovered = vault_proxy.reprobe(blocked, environ=env, token="t")
        check(not recovered.blocks_egress and recovered.credentialed, "once the vault answers, egress is allowed again")
        check(env.get("HTTPS_PROXY") == addr, "and the proxy environment is applied, not just assumed")
        healthy = vault_proxy.VaultStatus(enabled=True, ok=True)
        check(vault_proxy.reprobe(healthy) is healthy, "a status that was not blocked is returned as it was")
    finally:
        revived.close()


def test_the_token_comes_from_the_keychain() -> None:
    """`vault.token` is not a setting; the token is read from the Keychain item
    this module names. A recorder stands in for `secret_guard` so the assertion
    is about *which* service was asked for."""
    addr, close = listener()
    asked: list[str] = []

    class Recorder:
        @staticmethod
        def take_secret(*names, keychain=None):
            asked.append(keychain)
            return SimpleNamespace(reveal=lambda: "stub-token")

    real = vault_proxy.secret_guard
    vault_proxy.secret_guard = Recorder()
    try:
        env: dict[str, str] = {}
        status = vault_proxy.install(Prefs(**{"vault.enabled": True, "vault.addr": addr}), environ=env)
        check(asked in ([vault_proxy.KEYCHAIN_SERVICE], [(vault_proxy.KEYCHAIN_SERVICE, vault_proxy.LEGACY_KEYCHAIN_SERVICE)]), "the Keychain service named by KEYCHAIN_SERVICE is asked")
        check("stub-token" not in env.values(), "the vault token never enters the tool environment")
        check(status.credentialed is True, "an available token still yields a credentialed session")
    finally:
        vault_proxy.secret_guard = real
        close()


def test_the_gate_refuses_egress_tools() -> None:
    """The gate itself, through the real `_ApprovalHandler` — the part that
    decides whether a blocked vault actually stops anything."""
    import acp_server
    from strands.interventions import Confirm, Deny, Proceed

    blocked = vault_proxy.VaultStatus(enabled=True, ok=False, addr="http://127.0.0.1:1", reason="the vault at http://127.0.0.1:1 is unreachable and vault.egressPolicy is 'deny'")
    offline = vault_proxy.VaultStatus(enabled=False)

    real_settings = acp_server.settings_store.current
    acp_server.settings_store.current = lambda cwd: acp_server.settings_store.Settings()
    try:
        def event(name, tool_input):
            return SimpleNamespace(tool_use={"name": name, "input": tool_input, "toolUseId": "tu-1"})

        handler = acp_server._ApprovalHandler("/tmp", "build", vault=blocked)
        for tool, tool_input in (("shell", {"command": "curl https://example.com"}), ("python_repl", {"code": "import requests"}), ("http_request", {"url": "https://example.com"})):
            verdict = handler.before_tool_call(event(tool, tool_input))
            check(isinstance(verdict, Deny), f"blocked vault refuses {tool}")
            check("Egress is blocked" in verdict.reason or "unreachable" in verdict.reason, f"{tool} refusal explains itself")

        # The fail-open case this exists to prevent: auto mode must not wave it
        # through, and no pause may be offered as a way around it.
        handler = acp_server._ApprovalHandler("/tmp", "auto", vault=blocked)
        check(isinstance(handler.before_tool_call(event("shell", {"command": "ls"})), Deny), "auto mode does not bypass a blocked vault")

        # A blocked vault is not a blanket freeze: reading a file is fine...
        handler = acp_server._ApprovalHandler("/tmp", "build", vault=blocked)
        check(isinstance(handler.before_tool_call(event("file_read", {"path": "/tmp/x"})), Proceed), "non-egress tools still run")

        # ...and with the vault off, nothing about the gate changed.
        handler = acp_server._ApprovalHandler("/tmp", "build", vault=offline)
        verdict = handler.before_tool_call(event("shell", {"command": "ls"}))
        check(isinstance(verdict, Confirm), "vault off -> shell keeps asking as before")
    finally:
        acp_server.settings_store.current = real_settings


def main() -> None:
    test_addr_parsing()
    test_health()
    test_disabled_is_inert()
    test_healthy_sets_the_environment()
    test_placeholders_never_shadow_the_harness_keys()
    test_unreachable_denies()
    test_launch_refuses_without_the_vault()
    test_no_proxy_keeps_the_users_entries_and_credentialed_needs_a_token()
    test_a_blocked_session_recovers_when_the_vault_comes_up()
    test_the_token_comes_from_the_keychain()
    test_the_gate_refuses_egress_tools()
    print("\nvault_proxy: all checks passed")


if __name__ == "__main__":
    main()
