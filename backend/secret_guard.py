"""Keeps provider API keys out of everything an agent can read.

The threat is the agent itself (or text steering it, see prompt_security.py):
`shell` children inherit this process's environment, `file_read`/`cat` can
open backend/.env, `ps eww` prints a process's startup environment, and
module globals are one `import` away from `python_repl`. A key that sits in
any of those is a key the model can read, echo into a reply, or send out
through `http_request`.

What this module does about it (stdlib + python-dotenv only, so settings.py
can import it before anything else):

* ``take_secret`` — read a key from the environment exactly once, wrap it in
  a ``Secret`` (repr/str/format print ``Secret(****)``, only ``reveal()``
  returns the value), and remove it from ``os.environ`` so `shell` children
  and ``os.environ`` dumps never contain it.
* ``register_secret`` / ``redact`` — every secret value is remembered so
  ``redact`` can mask it as a literal anywhere it appears. prompt_security's
  ``SecretRedactionHooks`` applies that to every tool result before the model
  (or a UI) sees it, which also covers the paths scrubbing can't: ``ps eww``
  reads the *startup* environment block, which ``os.environ.pop`` doesn't
  touch, and a key someone put in backend/.env is still readable from disk.

Not a sandbox. ``python_repl`` runs in this process, so code it executes can
still reach ``Secret.reveal()`` and send the value somewhere in a form no
literal-match can see (encoded, split). That tool is approval-gated in build
mode and denied to sub-agents, and THREAT_MODEL.md says plainly that auto mode
removes the gate. This module closes the accidental and injected-instruction
paths (print it, `env`, `cat .env`, `ps eww`), not a determined in-process
program.
"""

import logging
import os
import re
import threading
from collections.abc import Iterable, Mapping
from typing import Any

from dotenv import dotenv_values

logger = logging.getLogger(__name__)

REDACTED = "[REDACTED:secret]"

# Shorter values aren't secrets worth masking, and masking them would
# corrupt ordinary output ("key", "token", a port number).
MIN_REDACT_LENGTH = 12

# Env vars whose *names* mark them as credentials (anywhere in the name, so
# AWS_SECRET_ACCESS_KEY and GITHUB_TOKEN both count). The backend inherits the
# whole environment of the shell that launched it, which `env` would print. Their values get masked in
# tool output but are NOT removed from the environment: libraries read some of
# them lazily (e.g. HF_TOKEN during a model download), so only the provider
# keys this app reads itself (take_secret callers) are scrubbed.
_SECRET_NAME = re.compile(r"(KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL)", re.IGNORECASE)

_lock = threading.Lock()
_registered: set[str] = set()


class Secret:
    """A credential that refuses to print itself.

    Deliberately not a ``str`` subclass: a str subclass would be formatted
    into f-strings and headers by any library handed it, so the only way to
    keep it from leaking via ``repr``/``str``/``format`` *and* still work as an
    API key is an explicit ``reveal()`` at the one place a client is built.
    """

    __slots__ = ("_value",)

    def __init__(self, value: str = "") -> None:
        object.__setattr__(self, "_value", value or "")
        register_secret(self._value)

    def reveal(self) -> str:
        return self._value

    def __bool__(self) -> bool:
        return bool(self._value)

    def __repr__(self) -> str:
        return "Secret(****)" if self._value else "Secret(<unset>)"

    __str__ = __repr__

    def __format__(self, spec: str) -> str:
        return repr(self)

    def __setattr__(self, name: str, value: Any) -> None:
        raise AttributeError("Secret is immutable")

    def __reduce__(self):
        # Never let pickle/copy/json-ish serializers carry the value out.
        raise TypeError("Secret cannot be serialized")


def reveal(value: "Secret | str | None") -> str:
    """The raw value of a Secret or plain string (provider_settings stores
    whichever it was handed); '' for None."""
    if isinstance(value, Secret):
        return value.reveal()
    return value or ""


def register_secret(value: str) -> None:
    if value and len(value) >= MIN_REDACT_LENGTH:
        with _lock:
            _registered.add(value)


def _dotenv_keys() -> set[str]:
    try:
        return {key for key, val in dotenv_values().items() if val}
    except Exception:
        return set()


def take_secret(*names: str) -> Secret:
    """The first non-empty value among ``names`` as a ``Secret``, with *every*
    listed variable removed from ``os.environ`` (an alias set to the same key
    must not survive just because the primary name won).

    Warns when a name is defined in backend/.env: unlike an exported shell
    variable, that file sits on disk where the agent's file tools can read it.
    """
    in_dotenv = _dotenv_keys()
    value = ""
    for name in names:
        candidate = os.environ.pop(name, "")
        if name in in_dotenv and candidate:
            logger.warning(
                "%s is set in backend/.env, which the agent's file tools can read. "
                "Export it in your shell instead and remove it from .env.",
                name,
            )
        value = value or candidate
    return Secret(value)


def register_env_secrets(environ: Mapping[str, str] | None = None) -> int:
    """Register (for output masking only) every credential-looking variable
    currently in the environment. Returns how many were registered."""
    count = 0
    for name, value in (environ if environ is not None else os.environ).items():
        if _SECRET_NAME.search(name) and len(value) >= MIN_REDACT_LENGTH:
            register_secret(value)
            count += 1
    return count


def registered_secrets() -> list[str]:
    with _lock:
        # Longest first, so a secret that contains another is masked whole.
        return sorted(_registered, key=len, reverse=True)


def redact(text: str) -> str:
    for secret in registered_secrets():
        if secret in text:
            text = text.replace(secret, REDACTED)
    return text


def redact_in_place(obj: Any) -> bool:
    """Mask registered secrets in every string inside a nested dict/list (a
    tool result's content blocks). Returns True if anything changed."""
    changed = False
    if isinstance(obj, dict):
        for key, value in obj.items():
            if isinstance(value, str):
                masked = redact(value)
                if masked != value:
                    obj[key] = masked
                    changed = True
            else:
                changed |= redact_in_place(value)
    elif isinstance(obj, list):
        for index, value in enumerate(obj):
            if isinstance(value, str):
                masked = redact(value)
                if masked != value:
                    obj[index] = masked
                    changed = True
            else:
                changed |= redact_in_place(value)
    return changed


def contains_secret(obj: Any) -> bool:
    """True if any registered secret appears verbatim in a nested structure
    (a tool call's input)."""
    secrets = registered_secrets()
    if not secrets:
        return False

    def walk(node: Any) -> bool:
        if isinstance(node, str):
            return any(secret in node for secret in secrets)
        if isinstance(node, dict):
            return any(walk(key) or walk(value) for key, value in node.items())
        if isinstance(node, Iterable) and not isinstance(node, (bytes, bytearray)):
            return any(walk(item) for item in node)
        return False

    return walk(obj)
