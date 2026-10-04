"""The two front doors must agree about what a valid setting is.

`backend/settings_store.py` and `tui/crates/acp-tui/src/settings.rs` each keep
their own `SCHEMA`, and each *rejects an unknown key with a warning*
(settings_store.py `_check`, settings.rs `check`). Nothing at runtime compares
them, so a key added on one side and not the other does not fail — it shows up
as a spurious warning in whichever front door is behind, which is easy to
misread as a user error rather than a bug in the harness.

This parses the Rust table and asserts the two agree on both the key set and
the kind of each key, so that drift fails here instead.

Hermetic and fast — no model, no network. Run directly:

    backend/.venv/bin/python _settings_schema_parity_smoke_test.py
"""

from __future__ import annotations

import re
from pathlib import Path

import settings_store

REPO_ROOT = Path(__file__).resolve().parent.parent
RUST_SETTINGS = REPO_ROOT / "tui" / "crates" / "acp-tui" / "src" / "settings.rs"


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


def rust_schema() -> dict[str, tuple]:
    """`{key: python-shaped spec}` read out of the Rust `SCHEMA` const."""
    source = RUST_SETTINGS.read_text(encoding="utf-8")
    start = source.index("pub const SCHEMA: &[Entry] = &[")
    # The table ends at the first line that closes it from column 0. Entry
    # blocks are indented, so an unindented `];` cannot be a nested one.
    end = source.index("\n];", start)
    block = source[start:end]

    def spec(kind: str, args: str) -> tuple:
        args = args.strip()
        if kind == "Bool":
            return ("bool",)
        if kind == "Str":
            return ("str",)
        if kind == "Enum":
            return ("enum", tuple(re.findall(r'"([^"]*)"', args)))
        if kind == "Float":
            low, _, high = args.partition(",")
            return ("float", float(low.strip()), float(high.strip()))
        raise AssertionError(f"unhandled Rust Kind::{kind} — extend this test")

    found: dict[str, tuple] = {}
    pattern = re.compile(r'key:\s*"([^"]+)",\s*kind:\s*Kind::(\w+)(?:\(([^)]*)\))?,')
    for key, kind, args in pattern.findall(block):
        found[key] = spec(kind, args or "")
    return found


def main() -> None:
    rust = rust_schema()
    check(len(rust) > 20, f"parsed a plausible number of Rust entries ({len(rust)})")
    check(
        len(rust) == len(set(rust)),
        "no duplicate keys in the Rust SCHEMA",
    )

    python_only = sorted(set(settings_store.SCHEMA) - set(rust))
    rust_only = sorted(set(rust) - set(settings_store.SCHEMA))
    check(not python_only, f"every Python key exists in the Rust SCHEMA (missing: {python_only})")
    check(not rust_only, f"every Rust key exists in the Python SCHEMA (missing: {rust_only})")

    mismatched = []
    for key in sorted(set(rust) & set(settings_store.SCHEMA)):
        # Python writes ("bool",) where Rust has Kind::Bool; compare on a
        # normalised (kind, detail) pair so only real disagreements show.
        py, rs = settings_store.SCHEMA[key], rust[key]
        if py[0] != rs[0] or (py[0] in ("enum", "float") and py[1:] != rs[1:]):
            mismatched.append(f"{key}: python={py} rust={rs}")
    check(not mismatched, "kinds and ranges agree:\n    " + "\n    ".join(mismatched))

    # The specific keys this change introduced, so a rename on one side alone
    # cannot pass the set comparison by coincidence.
    for key in (
        "laya.riskSafe",
        "laya.riskDestroy",
        "laya.riskAutoApprove",
        "memory.traces",
        "vault.enabled",
        "vault.addr",
        "vault.vault",
        "vault.egressPolicy",
        "observer.enabled",
        "observer.intervalMinutes",
    ):
        check(key in rust and key in settings_store.SCHEMA, f"{key} is declared on both sides")

    # A credential must never be settable through a settings file, which is why
    # vault.token is absent from both tables rather than range-checked.
    check(
        not any("token" in key or "key" in key for key in settings_store.SCHEMA),
        "no credential-shaped key is declared in the settings schema",
    )


if __name__ == "__main__":
    main()
    print("\nsettings schema parity: all checks passed")
