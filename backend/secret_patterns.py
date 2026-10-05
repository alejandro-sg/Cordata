"""Pattern-based secret masking, for text that is about to be *shown to a person*.

``secret_guard.redact`` masks the credentials this process *knows* — the ones it loaded
(``take_secret``) or registered from its environment. That is exactly right inside the
agent process, and exactly insufficient at the edge of a different one: the daemon never
loads a provider key, so for it ``redact`` is a no-op, and the diff of a ``.env`` the
model was about to write carries a key nobody registered anywhere.

This module masks what a credential *looks like*, with no registry:

* well-known token formats (``sk-…``, ``sk-ant-…``, ``ghp_…``, ``github_pat_…``, ``xox?-…``,
  ``AKIA…``, ``AIza…``, ``hf_…``, three-part JWTs);
* ``Authorization:`` / ``Bearer`` values;
* ``NAME=value`` / ``NAME: value`` where NAME contains key/token/secret/password/credential
  and the value *looks* like one (≥16 chars with letters and digits — so ``max_tokens = 4096``
  and ``token_count = compute_budget`` are left readable, and a diff stays reviewable);
* PEM private-key blocks.

It is a second line of defence, deliberately conservative about *false positives* on code
(a reviewer must be able to read the diff) and complete on the formats that actually leak. The
Rust client carries the same table (``lab/crates/harness-client/src/redact.rs``) and masks again
before drawing; ``_secret_patterns_smoke_test.py`` holds the two to the same fixtures.

Never raises; returns ``(masked text, number of spans masked)``.
"""

from __future__ import annotations

import re

import secret_guard

REDACTED = "[REDACTED]"

_NAME = r"[A-Za-z0-9_.-]*(?:api[_-]?key|apikey|token|secret|passw(?:or)?d|credential|private[_-]?key|auth)[A-Za-z0-9_.-]*"

#: (pattern, replacement). Order matters: blocks first, then the labelled forms, then bare formats.
PATTERNS: tuple[tuple[re.Pattern[str], str], ...] = (
    (re.compile(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\Z)", re.DOTALL), "[REDACTED PRIVATE KEY]"),
    (re.compile(r"(?i)\b(authorization\s*[:=]\s*(?:bearer|basic|token)\s+)[A-Za-z0-9._~+/=-]{8,}"), r"\1" + REDACTED),
    (re.compile(r"(?i)\b(bearer\s+)(?=[A-Za-z0-9._~+/=-]*\d)[A-Za-z0-9._~+/=-]{16,}"), r"\1" + REDACTED),
    (
        re.compile(rf"(?i)\b({_NAME})(\s*[=:]\s*)(['\"]?)(?=[A-Za-z0-9/+_=.-]*\d)(?=[A-Za-z0-9/+_=.-]*[A-Za-z])([A-Za-z0-9/+_=.-]{{16,}})\3"),
        rf"\1\2\3{REDACTED}\3",
    ),
    (re.compile(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b"), REDACTED),
    (
        re.compile(
            r"\b(?:sk-ant-[A-Za-z0-9_-]{16,}|sk-[A-Za-z0-9_-]{20,}|ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}|ghs_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}"
            r"|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{30,}|hf_[A-Za-z0-9]{20,})\b"
        ),
        REDACTED,
    ),
)


def scrub(text: str) -> tuple[str, int]:
    """``text`` with every credential-shaped span (and every registered secret) masked."""
    if not text:
        return text or "", 0
    try:
        count = 0
        masked = secret_guard.redact(text)
        if masked != text:
            count += sum(text.count(secret) for secret in secret_guard.registered_secrets() if secret in text)
        for pattern, replacement in PATTERNS:
            masked, n = pattern.subn(replacement, masked)
            count += n
        return masked, count
    except Exception:  # masking must never be the thing that fails a request
        return text, 0
