#!/usr/bin/env python3
"""Refuses to let a real credential into git.

    scripts/check-secrets.py              # every tracked and not-ignored file (what `make secrets` and CI run)
    scripts/check-secrets.py --staged     # only the lines being committed (what the pre-commit hook runs)
    scripts/check-secrets.py --history    # every blob reachable from any ref

It looks for provider-shaped tokens (Anthropic, OpenAI-style `sk-`, GitHub, AWS, Google, Slack, Hugging Face, JWTs,
private-key blocks). The repository's tests and the redaction fixtures contain deliberately fake ones, so a match is
let through when it is plainly synthetic: an alphabet run, a repeated character, or a word like "fake" or "planted".
Anything else fails, with file and line but never the value. Standard library only.
"""

from __future__ import annotations

import collections
import math
import re
import subprocess
import sys

PATTERNS = {
    "Anthropic key": r"sk-ant-[A-Za-z0-9_\-]{10,}",
    "OpenAI-style key": r"\bsk-[A-Za-z0-9_\-]{20,}",
    "GitHub token": r"\b(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,})",
    "AWS access key id": r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
    "Google API key": r"\bAIza[0-9A-Za-z_\-]{35}\b",
    "Slack token": r"\bxox[abprs]-[A-Za-z0-9\-]{10,}",
    "Hugging Face token": r"\bhf_[A-Za-z0-9]{30,}\b",
    "JWT": r"\beyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
    "Discord bot token": r"\b[MN][A-Za-z\d]{23,27}\.[\w\-]{6}\.[\w\-]{27,}\b",
    "credentials in a URL": r"://[^/\s:@]+:[^/\s:@]{8,}@(?!localhost|127\.0\.0\.1)[A-Za-z0-9.\-]+",
}
COMPILED = {name: re.compile(rx) for name, rx in PATTERNS.items()}

FAKE_WORDS = re.compile(
    r"(?i)(example|sample|placeholder|dummy|fake|test|planted|leak|secret|should|redact|canary|your[_-]|changeme|"
    r"xxxx|aaaa|1234|abcd|0000|<[^>]+>|\$\{|\.\.\.)"
)
PREFIX = re.compile(r"^(sk-ant-|sk-|gh[pousr]_|github_pat_|xox[abprs]-|hf_|AKIA|ASIA|AIza|eyJ)")
SKIP_PATHS = re.compile(r"(^|/)(Cargo\.lock|package-lock\.json|uv\.lock|.*\.min\.js)$")


def entropy(text: str) -> float:
    counts = collections.Counter(text)
    return -sum(n / len(text) * math.log2(n / len(text)) for n in counts.values())


def looks_synthetic(token: str) -> bool:
    """True for the fakes tests use: they spell the alphabet, repeat a character, or say what they are."""
    if FAKE_WORDS.search(token):
        return True
    body = PREFIX.sub("", token)
    if len(body) < 8:
        return True
    if token.startswith("sk-ant-") and len(body) < 40:
        return True  # real Anthropic keys run past 90 characters; a short one was typed by hand for a test
    if token.startswith("eyJ") and ".eyJzdWIiOiIxMjM0NTY3ODkwIn" in token:
        return True  # the jwt.io sample ({"sub":"1234567890"}), in every JWT tutorial
    pairs = list(zip(body, body[1:]))
    if sum(1 for a, b in pairs if ord(b) - ord(a) == 1) / max(1, len(pairs)) >= 0.5:
        return True  # abcdefghij…, 0123456789
    return len(set(body)) <= 6 or entropy(body) < 3.0


PEM = re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----(?:\\n|\\r|\s)*([A-Za-z0-9+/=]{40,})")


def findings_in(text: str):
    for match in PEM.finditer(text):  # a real block has a base64 body; a mention of the header in prose or a regex does not
        if not looks_synthetic(match.group(1)):
            yield text.count("\n", 0, match.start()) + 1, "private key"
    for number, line in enumerate(text.splitlines(), 1):
        if len(line) > 4000:
            continue
        for name, rx in COMPILED.items():
            for match in rx.finditer(line):
                if not looks_synthetic(match.group(0)):
                    yield number, name


def git(*args: str, text: bool = True) -> str:
    return subprocess.run(["git", *args], capture_output=True, text=text, check=False).stdout


def scan_tree() -> list[str]:
    bad = []
    for path in git("ls-files", "-co", "--exclude-standard", "-z").split("\0"):
        if not path or SKIP_PATHS.search(path):
            continue
        try:
            data = open(path, "rb").read()
        except OSError:
            continue
        if b"\0" in data[:2000]:
            continue
        bad += [f"{path}:{n}: {name}" for n, name in findings_in(data.decode("utf-8", "replace"))]
    return bad


def scan_staged() -> list[str]:
    bad = []
    for path in git("diff", "--cached", "--name-only", "--diff-filter=ACMR", "-z").split("\0"):
        if not path or SKIP_PATHS.search(path):
            continue
        added = [line[1:] for line in git("diff", "--cached", "-U0", "--", path).splitlines() if line.startswith("+") and not line.startswith("+++")]
        bad += [f"{path}: {name} (in the staged change)" for _, name in findings_in("\n".join(added))]
    staged_env = [p for p in git("diff", "--cached", "--name-only", "--diff-filter=ACMR").splitlines() if re.search(r"(^|/)\.env($|\.(?!example$))|\.(pem|p12|pfx|key)$|id_(rsa|ed25519)", p)]
    return bad + [f"{p}: this kind of file holds credentials; it must not be committed" for p in staged_env]


def scan_history() -> list[str]:
    names = {}
    for line in git("rev-list", "--objects", "--all").splitlines():
        sha, _, name = line.partition(" ")
        names[sha] = name
    bad = []
    process = subprocess.Popen(["git", "cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    for sha, name in names.items():
        process.stdin.write(f"{sha}\n".encode())
        process.stdin.flush()
        header = process.stdout.readline().split()
        data = process.stdout.read(int(header[2]))
        process.stdout.read(1)
        if header[1] != b"blob" or len(data) > 2_000_000 or b"\0" in data[:2000] or SKIP_PATHS.search(name):
            continue
        bad += [f"{name or sha}@{sha[:8]}:{n}: {kind}" for n, kind in findings_in(data.decode("utf-8", "replace"))]
    process.stdin.close()
    return bad


def main() -> int:
    mode = "staged" if "--staged" in sys.argv else "history" if "--history" in sys.argv else "tree"
    bad = {"staged": scan_staged, "history": scan_history, "tree": scan_tree}[mode]()
    if bad:
        print("possible credentials found (values not shown):", file=sys.stderr)
        for item in bad:
            print(f"  {item}", file=sys.stderr)
        print("Move the value to the macOS Keychain (make key) or an environment variable. If it is a test fake, make it plainly fake "
              "(abcdefghij…, or say 'fake' in it).", file=sys.stderr)
        return 1
    print(f"no credentials found ({mode})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
