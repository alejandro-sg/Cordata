"""One filter for every piece of prose the harness lets a learned artifact put in front of the model.

A learned skill (`learned.py`) and a prompt overlay (`prompts.py`) are both text that reaches the model as if the
harness had written it. The patterns below are what no such text may contain: a URL, a download piped into a
shell, a destructive or privileged command, an instruction to ignore instructions or skip approval, a credential
path. ``learned`` used to hold these privately; they live here now so the prompt path cannot drift from the skill
path, and so a rule added for one is enforced for both.

This is a **first filter, not the defence.** The defence is that a learned artifact is staged, never live, until
the promotion gate (`promote.py`) has run it through the eval suite, and that a person can read it in full before
it goes live. This runs so that a clearly hostile draft never even reaches that stage.

Stdlib plus ``secret_patterns``; no model, no Strands.
"""

from __future__ import annotations

import re
from typing import NamedTuple

#: ``(pattern, why)`` — the phrase the report gives a person is ``it {why}``.
BANNED: list[tuple[str, str]] = [
    (r"https?://", "contains a URL"),
    (r"\b(curl|wget)\b[^\n]*\|\s*(ba|z)?sh\b", "pipes a download into a shell"),
    (r"\brm\s+-[a-z]*r[a-z]*f?\b|\bsudo\b|\bchmod\s+[0-7]*7\b|\bmkfs\b|\bdd\s+if=", "names a destructive or privileged command"),
    (r"ignore\s+(all\s+|any\s+|the\s+|your\s+)?(previous|prior|above|earlier|system)?\s*(instructions|rules|guidelines)", "tells the model to ignore its instructions"),
    (r"disregard|override\s+(the\s+)?(system|safety|policy)", "tells the model to disregard something"),
    (r"without\s+(asking|approval|confirmation|permission|telling)|skip\s+(the\s+)?(approval|confirmation|review|prompt|check)", "tells the model to skip asking"),
    (r"auto-?\s?approv|\bbypass|\bcircumvent|disable\s+(the\s+)?(guard|safety|approval|sandbox|laya|checks?|hooks?|vault)", "tells the model to bypass a safeguard"),
    (r"(do\s*n[o']t|never)\s+(tell|inform|mention\s+to|show)\s+(the\s+)?user|keep\s+(this|it)\s+(secret|hidden)", "tells the model to hide something from the user"),
    (r"exfiltrat|\.ssh\b|keychain|\bid_rsa|~/\.aws|\bcredentials?\s+file|\.netrc|/etc/(passwd|shadow)|\bprivate\s+key", "points at credentials"),
    (r"<\s*/?\s*(system|assistant|tool|instructions?)\b|\[\s*(system|inst)\s*\]", "imitates a prompt delimiter"),
]

#: Control characters and bidirectional overrides: invisible reordering of text a person is asked to approve.
CONTROL_CHARS = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f‪-‮⁦-⁩]")


class Problems(NamedTuple):
    problems: list[str]
    scrubbed: bool


def _banned_re() -> list[tuple[re.Pattern[str], str]]:
    return [(re.compile(p, re.IGNORECASE), why) for p, why in BANNED]


_BANNED_RE = _banned_re()


def inspect(text: str) -> Problems:
    """Every reason ``text`` may not go live, as a person would read them. Empty means it may be staged."""
    problems: list[str] = []
    for pattern, why in _BANNED_RE:
        if pattern.search(text):
            problems.append(f"it {why}")
    try:
        import secret_patterns

        scrubbed = bool(secret_patterns.scrub(text)[1])
    except Exception:
        scrubbed = False
    if scrubbed:
        problems.append("it contains a credential-shaped string")
    if CONTROL_CHARS.search(text):
        problems.append("it contains control or bidirectional-override characters")
    return Problems(problems, scrubbed)
