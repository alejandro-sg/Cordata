"""Layered user settings and permission rules — the cordata-agents analogue of
Claude Code's ``settings.json`` / ``CLAUDE.md`` (and Kilo's ``kilo.jsonc``).

Three JSON files, lowest precedence first (a higher one wins on a conflict):

    user     ~/.cordata/settings.json            (``CORDATA_HOME`` moves the dir)
    project  <cwd>/.cordata/settings.json        (commit it; shared with the team)
    local    <cwd>/.cordata/settings.local.json  (this machine only; gitignored)

Objects merge key by key; ``permissions.allow/ask/deny`` lists *accumulate* across
files (so a project can add a deny without erasing the user's own rules); every
other value is replaced. The acp-tui client reads and writes the same files
(``app/crates/cordata-view/src/settings.rs``) — ``docs/settings.md`` is the contract,
and both implementations are tested against ``tests/fixtures/settings``.

What does NOT go in here: credentials. API keys live in the environment or the
macOS Keychain (see ``secret_guard.py``); a settings value that looks like a
secret, or a key named like one, is refused. The model *provider* is deployment
configuration (env / ``.env``), not a preference.

Trust. A cloned repository can ship ``.cordata/settings*.json``. Its ``deny``
and ``ask`` rules only restrict, so they always apply; its ``allow`` rules and a
more permissive default mode would let a stranger's repo switch off the approval
prompts, so they apply only once the folder is listed in the *user* file's
``trustedProjects`` (``/trust`` in the TUI, or choosing "Always allow" on an
approval prompt, which is an explicit decision about this folder).

Permission rules (``permissions.allow`` / ``ask`` / ``deny``), Claude-style:

    shell                  every shell call
    shell(git status)      exactly that command
    shell(git *)           ``*`` matches any text
    file_write(src/*)      a path (relative ones are taken from the project dir)
    http_request(https://docs.python.org/*)

``deny`` beats ``ask`` beats ``allow``, and ``deny`` holds in every mode,
``auto`` included. A shell command is split on ``;  &&  ||  |  &`` and newlines:
a *deny* pattern matches if any part matches; an *allow* pattern with ``*`` has to
match every part, and a command with a substitution (``$( )``, backticks) or a
redirect is never auto-allowed by a wildcard. These rules are a convenience on top
of the approval prompt, not a sandbox.
"""

from __future__ import annotations

import json
import os
import re
import tempfile
import threading
from dataclasses import dataclass, field
from pathlib import Path

SCOPES = ("user", "project", "local")  # lowest precedence first
MODES = ("plan", "build", "edit", "auto")
TEMPERATURES = ("focused", "balanced", "creative")
LAYA_MODES = ("off", "shadow", "route")
COLLAPSE = ("collapsed", "expanded")
RULE_KINDS = ("allow", "ask", "deny")

# dotted key -> ("enum", choices) | ("bool",) | ("str",) | ("float", lo, hi)
SCHEMA: dict[str, tuple] = {
    "permissions.defaultMode": ("enum", MODES),
    # Adaptive mode (mode_advisor.py): a request that wants a plan first (a design question, "plan this", a large
    # cross-cutting change) starts the turn in plan mode and ends with a plan card. It only ever tightens; leaving plan is
    # the person's answer to the card. Not trust-gated for that reason: a repository cannot loosen anything with it.
    "modes.adaptive": ("bool",),
    "model": ("str",),
    "temperature": ("enum", TEMPERATURES),
    "laya.mode": ("enum", LAYA_MODES),
    "laya.threshold": ("float", 0.0, 1.0),
    "tiers.fast": ("str",),
    "tiers.standard": ("str",),
    "tiers.strong": ("str",),
    "laya.dispatch": ("enum", ("direct", "hint")),
    "laya.teamDispatch": ("enum", ("direct", "hint")),
    "laya.minConfidence": ("float", 0.0, 1.0),
    "laya.onAbstain": ("enum", ("escalate", "llm")),
    "selfHeal.enabled": ("bool",),
    "selfHeal.maxAttempts": ("float", 1.0, 10.0),
    "selfHeal.verify": ("enum", ("sandbox", "workspace", "off")),
    "selfHeal.autoApply": ("bool",),
    "selfHeal.memory": ("bool",),
    "selfHeal.autoResume": ("bool",),
    # Learning (reflect.py, learned.py, promote.py). Reflection drafts a lesson into staging after a turn that cost
    # something; nothing staged reaches a model until it has beaten the baseline on the eval suite
    # (`learning.promotion`: off = never automatically, shadow = evaluate and record verdicts only, live = promote on a pass).
    "learning.reflect": ("bool",),
    "learning.promotion": ("enum", ("off", "shadow", "live")),
    "learning.evalTasks": ("float", 3.0, 60.0),
    # Prompt/skill optimization (optimize.py, prompts.py): draft a sharper variant of a live lesson or prompt
    # overlay and stage it as a candidate for the same gate. Off by default; trust-gated so a cloned repo cannot
    # start rewriting what is in front of the model.
    "learning.optimizePrompts": ("bool",),
    # The contextual bandit (bandit.py): learns which of the harness's own discrete choices — the route action,
    # the model tier, the reasoning effort, Claude Code vs local tools, whether to ask — tends to turn out better
    # in a context like this one. Off by default, and **additive only**: a learned choice may add an ask or
    # re-rank options already on the table, never remove a prompt or lower a Guard score. `learning.bandit` is
    # trust-gated so a cloned repo cannot turn a learned policy loose on itself.
    "learning.bandit": ("enum", ("off", "shadow", "live")),
    "learning.banditExplore": ("float", 0.0, 0.5),
    "learning.banditMinPulls": ("float", 1.0, 100.0),
    "learning.banditLambda": ("float", 0.0, 1.0),
    "learning.banditMu": ("float", 0.0, 1.0),
    # The orchestrator (backend/orchestrator/, docs/orchestrator-plan.md): who may work (`workers`, a comma list of
    # claude_code · antigravity · ollama), how dispatch learns (`dispatch`: off = always strongest, shadow = learn and
    # log but act as off, live = act on what was learned), the per-plan envelope (`maxUsd`, `maxTokens`), the quota
    # pools (`pools`, a JSON list — orchestrator/quota_ledger.py), and whether harden and the Tier-2 audit run.
    # Trust-gated, every one: they decide what is spent, by whom, and how much checking happens.
    "orchestrator.workers": ("str",),
    "orchestrator.dispatch": ("enum", ("off", "shadow", "live")),
    "orchestrator.maxUsd": ("float", 0.0, 100.0),
    "orchestrator.maxTokens": ("float", 0.0, 100000000.0),
    "orchestrator.pools": ("str",),
    "orchestrator.harden": ("bool",),
    "orchestrator.tier2": ("bool",),
    "orchestrator.lambda": ("float", 0.0, 2.0),
    "orchestrator.mu": ("float", 0.0, 2.0),
    "lsp.enabled": ("bool",),
    # Large files (code_nav.py): a whole-file read of a file over this many lines comes back as an outline of its
    # classes and functions plus how to read a range or one symbol, instead of the file. Trust-gated like the other
    # context levers: a repository must not be able to switch the diet off for itself.
    "context.outlineLines": ("float", 100.0, 20000.0),
    # Preflight (preflight.py): an edit is checked *before* it is put to a person or applied — syntax, the project's
    # linter, the language server's errors, optionally a test command in a throwaway copy. A check the edit makes
    # newly fail sends the errors back to the agent instead of asking (`maxRetries` per file per turn); when the
    # agent runs out, the turn's own edits are rolled back to its first checkpoint (`rollback`). Trust-gated: a
    # repository must not be able to switch verification off, and `tests` runs code.
    "preflight.enabled": ("bool",),
    "preflight.maxRetries": ("float", 0.0, 10.0),
    "preflight.tests": ("str",),
    "preflight.rollback": ("bool",),
    # Reasoning effort that follows the work (adaptive_effort.py): the orchestrator's reasoning_effort on Ollama
    # Cloud is chosen per model call from the workspace's size and Laya's reading of the turn's complexity. Off
    # keeps the role's fixed level.
    "reasoning.adaptive": ("bool",),
    # Rejection memory (rejection_memory.py): a change a person rejects is reflected on, once, by a tool-less model
    # call, and the rule it states ("do not …") is put in front of later turns in this project as a constraint.
    "learning.rejections": ("bool",),
    # Destructiveness scoring (laya_risk.py). The thresholds are the 0.2 / 0.8
    # the directive names. `riskAutoApprove` is reserved: nothing consults it
    # (laya_risk.auto_approvable has no caller, deliberately — see its docstring),
    # so setting it changes nothing. It stays in the schema so an existing
    # settings file that names it is not rejected.
    "laya.riskSafe": ("float", 0.0, 1.0),
    "laya.riskDestroy": ("float", 0.0, 1.0),
    "laya.riskAutoApprove": ("bool",),
    # Laya Guard (laya_guard.py): a destructiveness score for every tool call by every agent, not
    # just the turn's one reading. `rules` is deterministic only; `full` adds Laya's own reading,
    # which can only raise a score. A call at or above its agent's gate asks, in every mode.
    "laya.callGuard": ("enum", ("off", "rules", "full")),
    # Checkpoints (checkpoints.py): a snapshot of the project before every tool call that may change
    # files, so `auto` mode can be undone. On by default; trust-gated so a cloned repo cannot turn it off.
    "checkpoints.enabled": ("bool",),
    "checkpoints.maxFileMb": ("float", 0.5, 512.0),
    # Append-only trace store (trace_store.py). Off by default, so a checkout
    # without lancedb installed behaves exactly as it did before.
    "memory.traces": ("bool",),
    # Knowledge graph (kg/). Off by default, like the trace corpus, and gated the
    # same way: a checkout that has not opted in is unchanged by this module
    # existing. `kg.sink` picks the backend; both write the same JSONL mirror.
    "kg.enabled": ("bool",),
    "kg.sink": ("enum", ("lance", "cognee")),
    # Zero-trust credential proxy (vault_proxy.py). `vault.token` is deliberately
    # absent: it is a secret and lives in the Keychain via secret_guard, never in
    # a settings file, which is why _SECRET_KEY above is not a sufficient guard
    # on its own — the key simply must not exist here.
    "vault.enabled": ("bool",),
    "vault.addr": ("str",),
    "vault.vault": ("str",),
    "vault.egressPolicy": ("enum", ("deny", "proxy")),
    # The menu-bar deck's screen context (lab/crates/harness-menubar). Read only by
    # the menubar, and only from the *user* file — a project file is ignored there even
    # when trusted — so a cloned repo can never grant itself a window. Registered here so
    # the agent and the TUI recognise the keys rather than warn about them. Lists of
    # bundle ids, never names or patterns; the never-read list is not a setting at all.
    "capture.enabled": ("bool",),
    "capture.allowlist": ("bundle_ids",),
    "capture.denylist": ("bundle_ids",),
    "capture.screenshot": ("bool",),
    "capture.screenshotCloud": ("bool",),
    # The model a screenshot turn runs on (a vision model the session's provider serves). Unset:
    # screenshots are previewed but never sent, because no model is assumed to see.
    "capture.visionModel": ("str",),
    # Proactive observer (daemon/observer.py). Off by default — it is the only
    # thing in this schema that can act with no user turn behind it.
    "observer.enabled": ("bool",),
    "observer.intervalMinutes": ("float", 1.0, 1440.0),
    # The Meta-Harness (claude_code_tool.py, omlx_provider.py). The Claude Code CLI wrapper is
    # on by default but still asks for approval per call, and never egresses except through the
    # vault — there is deliberately no setting that lets it. `omlx.prism` sends the architect and
    # the local context digest to the Prism models on oMLX (when oMLX answers); off keeps them
    # on the orchestrator's provider.
    "claudeCode.enabled": ("bool",),
    "claudeCode.maxBudgetUsd": ("float", 0.1, 100.0),
    # What a Claude Code CLI run uses, beside its dollar cap: the model alias ("" = whatever the CLI defaults
    # to) and the effort level ("default" = the CLI's own). Both default to a cheaper setting than the CLI's —
    # see claude_code_tool.DEFAULT_MODEL / DEFAULT_EFFORT — because these runs spend the person's Claude Code usage.
    "claudeCode.model": ("str",),
    "claudeCode.effort": ("enum", ("default", "low", "medium", "high", "xhigh", "max")),
    # Token economy (context_diet.py): what every model call re-sends. `coldTools` hides rarely-used tools
    # until a turn asks for them (the agent can always load one with `enable_tools`); `toolOutputChars` caps one
    # tool result; `agedOutputChars` shortens tool output older than the newest few messages (0 turns it off).
    "context.coldTools": ("bool",),
    "context.toolOutputChars": ("float", 2000.0, 200000.0),
    "context.agedOutputChars": ("float", 0.0, 20000.0),
    # A result over `toolOutputChars` is set aside whole and read back by slice (context_offload.py, Strands'
    # ContextOffloader) instead of being cut; `offload: false` cuts it, head and tail. `maxSteps` / `maxTurnUsage` (tokens) are
    # what one turn may spend before it pauses (Strands' per-invocation `limits`); 0 turns a cap off.
    "context.offload": ("bool",),
    # What `web_fetch` returns: `agentic` (an analyst model answers your prompt about the page — the page never enters the
    # conversation) or `markdown` (the page itself, as markdown).
    "context.webFetch": ("enum", ("agentic", "markdown")),
    "context.maxSteps": ("float", 0.0, 500.0),
    # Who writes the summary when a long chat is compacted: the fast model tier (MODEL_TIER_FAST) when one is set, or the
    # session's own model (`main`). With no fast tier configured both are the same.
    "context.summaryModel": ("enum", ("fast", "main")),
    # Generation and compaction (the app's Settings → Models). `model.thinking` is Anthropic extended thinking: off,
    # auto (the model decides — adaptive thinking), always (a budget every call). `model.maxTokens` caps one reply.
    # `context.autoCompact` is when Strands' proactive compression summarises older turns (unset: 70%). All three are
    # trust-gated: a cloned repository must not raise what a turn may spend.
    "model.thinking": ("enum", ("off", "auto", "always")),
    "model.maxTokens": ("float", 1024.0, 128000.0),
    "context.autoCompact": ("enum", ("80", "90", "off")),
    "context.maxTurnUsage": ("float", 0.0, 50000000.0),
    # Skills (skill_search.py): list only the few that fit the request — embedding search, Laya deciding — instead of
    # all 49 on every call. `skillSearch: false` lists them all; `skillsListed` is how many are listed per turn.
    "context.skillSearch": ("bool",),
    "context.skillsListed": ("float", 1.0, 12.0),
    "omlx.prism": ("bool",),
    "tui.thinking": ("enum", COLLAPSE),
    "tui.toolOutput": ("enum", COLLAPSE),
    "tui.showLaya": ("bool",),
    # The native app's own preferences (docs/app.md §6.4). User file only (``USER_ONLY_PREFIXES``): how one person's
    # window looks and behaves is not a repository's to decide.
    "app.theme": ("enum", ("night", "high-contrast", "system")),
    "app.density": ("enum", ("comfortable", "compact")),
    "app.notifications": ("enum", ("all", "approvals", "off")),
    "app.restoreWindows": ("bool",),
    "app.showReasoning": ("bool",),
}
LIST_KEYS = tuple(f"permissions.{kind}" for kind in RULE_KINDS)
USER_ONLY_KEYS = ("trustedProjects",)
#: Whole groups read only from the user file. Mirrored in `app/crates/cordata-view/src/settings.rs` (`USER_ONLY_PREFIXES`).
USER_ONLY_PREFIXES = ("app.",)


def user_only(key: str) -> bool:
    return key in USER_ONLY_KEYS or key.startswith(USER_ONLY_PREFIXES)

#: Keys a repository must not be able to set for itself. Like `defaultMode:
#: "auto"` and `selfHeal.autoApply`, they decide how much the harness protects
#: the user, and `.cordata/settings.json` is a file a cloned project brings
#: along: `vault.enabled: false` would turn off the egress proxy the user chose,
#: `vault.egressPolicy: "proxy"` would turn its fail-closed refusal into
#: fail-open, `vault.addr` would point every tool subprocess's proxy variables at
#: a host the project names, `kg.sink` decides where graph rows are sent, and
#: `checkpoints.enabled: false` would remove the undo that makes `auto` mode safe to leave on. Set
#: from the user file, or from a project the user has `/trust`ed, they apply.
#: Mirrored in `app/crates/cordata-view/src/settings.rs` (`TRUST_GATED_KEYS`).
TRUST_GATED_KEYS = frozenset(
    {"vault.enabled", "vault.addr", "vault.vault", "vault.egressPolicy", "kg.sink", "claudeCode.maxBudgetUsd", "claudeCode.model", "claudeCode.effort", "context.coldTools", "context.toolOutputChars", "context.agedOutputChars", "context.offload", "context.webFetch", "context.maxSteps", "context.maxTurnUsage", "context.summaryModel", "model.thinking", "model.maxTokens", "context.autoCompact", "context.skillSearch", "context.skillsListed", "checkpoints.enabled", "checkpoints.maxFileMb", "learning.promotion", "learning.optimizePrompts", "learning.bandit",
     # Laya's own lines: `callGuard: off` or a raised `riskDestroy` from a cloned repo would quietly remove the
     # per-call asks that make `auto` mode bounded, and `laya.mode: off` the turn-level arming.
     "laya.mode", "laya.callGuard", "laya.riskSafe", "laya.riskDestroy",
     "context.outlineLines", "preflight.enabled", "preflight.maxRetries", "preflight.tests", "preflight.rollback", "learning.rejections",
     # What the menu bar may read off the screen. The menubar is stricter still (user file only).
     "capture.enabled", "capture.allowlist", "capture.denylist", "capture.screenshot", "capture.screenshotCloud", "capture.visionModel",
     # What the orchestrator may spend, on whom, and how much it checks.
     "orchestrator.workers", "orchestrator.dispatch", "orchestrator.maxUsd", "orchestrator.maxTokens", "orchestrator.pools",
     "orchestrator.harden", "orchestrator.tier2", "orchestrator.lambda", "orchestrator.mu"}
)

#: A bundle id as `capture.allowlist` accepts it: reverse-DNS labels, nothing that could be a path, a
#: pattern or a shell fragment. Mirrors `harness_capture::policy::acceptable` and the TUI's check.
_BUNDLE_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]*(\.[A-Za-z0-9][A-Za-z0-9_-]*)+$")

_SECRET_KEY = re.compile(r"(api[_-]?key|token|secret|password|passwd|credential)", re.IGNORECASE)
_SECRET_VALUE = re.compile(r"(sk-[A-Za-z0-9]{16,}|[A-Za-z0-9]{32,}\.[A-Za-z0-9_-]{8,}|ghp_[A-Za-z0-9]{20,}|xox[bp]-[A-Za-z0-9-]{10,})")

# Always on, listed by /permissions as "built-in": reading a stored credential out of
# the macOS Keychain from the agent's shell. secret_guard masks the value in tool
# output anyway; this refuses the call outright.
BUILTIN_DENY = (
    "shell(security *find-generic-password*)",
    "shell(security *find-internet-password*)",
    "shell(security *dump-keychain*)",
)


def user_dir() -> Path:
    env_dir = os.environ.get("CORDATA_HOME")
    if env_dir:
        return Path(env_dir)
    return Path.home() / ".cordata"


def settings_path(scope: str, cwd: str | os.PathLike) -> Path:
    if scope == "user":
        return user_dir() / "settings.json"
    p_cwd = Path(cwd)
    base = p_cwd / ".cordata"
    return base / ("settings.json" if scope == "project" else "settings.local.json")


# ----------------------------------------------------------------- reading


@dataclass
class Rule:
    kind: str  # allow | ask | deny
    text: str
    scope: str  # user | project | local | built-in
    active: bool = True  # False: written down but not applied (untrusted project)

    @property
    def tool(self) -> str:
        return self.text.partition("(")[0].strip()

    @property
    def pattern(self) -> str | None:
        if "(" not in self.text or not self.text.endswith(")"):
            return None
        return self.text.partition("(")[2][:-1]


@dataclass
class Settings:
    values: dict = field(default_factory=dict)  # merged, validated, dotted keys -> value
    sources: dict = field(default_factory=dict)  # dotted key -> scope that set it
    rules: list = field(default_factory=list)
    warnings: list = field(default_factory=list)
    trusted: bool = False
    paths: dict = field(default_factory=dict)

    def get(self, key: str, default=None):
        return self.values.get(key, default)

    def rules_for(self, kind: str) -> list[Rule]:
        return [rule for rule in self.rules if rule.kind == kind and rule.active]


def _read_json(path: Path, warnings: list) -> dict:
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return {}
    except OSError as error:
        warnings.append(f"{path}: cannot read ({error.strerror or error})")
        return {}
    if not text.strip():
        return {}
    try:
        data = json.loads(text)
    except ValueError as error:
        warnings.append(f"{path}: not valid JSON, ignored ({error})")
        return {}
    if not isinstance(data, dict):
        warnings.append(f"{path}: top level must be an object, ignored")
        return {}
    return data


def _flatten(data: dict, prefix: str = "") -> dict:
    flat = {}
    for key, value in data.items():
        dotted = f"{prefix}{key}"
        if isinstance(value, dict) and dotted not in ("trustedProjects",):
            flat.update(_flatten(value, dotted + "."))
        else:
            flat[dotted] = value
    return flat


def _check(key: str, value, where: str) -> tuple[bool, str | None]:
    """(ok, warning) for one setting value against SCHEMA."""
    if _SECRET_KEY.search(key.rpartition(".")[2]):
        return False, f"{where}: '{key}' looks like a credential — refused (keep keys in the environment or Keychain)"
    if isinstance(value, str) and _SECRET_VALUE.search(value):
        return False, f"{where}: the value of '{key}' looks like a secret — refused"
    spec = SCHEMA.get(key)
    if spec is None:
        return False, f"{where}: unknown setting '{key}' ignored"
    kind = spec[0]
    if kind == "enum" and value not in spec[1]:
        return False, f"{where}: '{key}' must be one of {', '.join(spec[1])} (got {value!r})"
    if kind == "bool" and not isinstance(value, bool):
        return False, f"{where}: '{key}' must be true or false"
    if kind == "str" and not isinstance(value, str):
        return False, f"{where}: '{key}' must be a string"
    if kind == "float":
        if isinstance(value, bool) or not isinstance(value, (int, float)) or not spec[1] <= value <= spec[2]:
            return False, f"{where}: '{key}' must be a number between {spec[1]} and {spec[2]}"
    if kind == "bundle_ids" and not (isinstance(value, list) and all(isinstance(item, str) and _BUNDLE_ID.match(item) for item in value)):
        return False, f"{where}: '{key}' must be a list of bundle ids, e.g. [\"com.apple.Safari\"]"
    return True, None


def _trusted(user_raw: dict, cwd: str) -> bool:
    trusted = user_raw.get("trustedProjects")
    if not isinstance(trusted, list):
        return False
    here = os.path.realpath(cwd)
    return any(isinstance(item, str) and os.path.realpath(os.path.expanduser(item)) == here for item in trusted)


def load(cwd: str | os.PathLike) -> Settings:
    """Merge the three files for ``cwd``. Never raises: a bad file or value is
    skipped and reported in ``warnings``."""
    cwd = str(cwd)
    result = Settings()
    raw = {}
    for scope in SCOPES:
        path = settings_path(scope, cwd)
        result.paths[scope] = str(path)
        raw[scope] = _read_json(path, result.warnings)
    result.trusted = _trusted(raw["user"], cwd)

    for scope in SCOPES:
        where = result.paths[scope]
        flat = _flatten(raw[scope])
        # fixed order (allow, ask, deny; each in file order) so every reader agrees
        ordered = [(k, v) for k, v in flat.items() if k not in LIST_KEYS]
        ordered += [(k, flat[k]) for k in LIST_KEYS if k in flat]
        for key, value in ordered:
            if user_only(key):
                if scope != "user":
                    result.warnings.append(f"{where}: '{key}' is only read from the user file — ignored")
                    continue
                if key in USER_ONLY_KEYS:
                    continue  # trustedProjects is read by `_trusted`, not as a value
            if key in LIST_KEYS:
                kind = key.rpartition(".")[2]
                if not isinstance(value, list) or not all(isinstance(item, str) and item.strip() for item in value):
                    result.warnings.append(f"{where}: '{key}' must be a list of rule strings")
                    continue
                for text in value:
                    if _SECRET_VALUE.search(text):
                        result.warnings.append(f"{where}: a rule in '{key}' looks like it holds a secret — refused")
                        continue
                    # allow rules from a repo only count once the folder is trusted
                    active = kind != "allow" or scope == "user" or result.trusted
                    result.rules.append(Rule(kind, text.strip(), scope, active))
                    if not active:
                        result.warnings.append(
                            f"{where}: allow rule '{text.strip()}' ignored — folder not trusted (run /trust to enable)"
                        )
                continue
            ok, warning = _check(key, value, where)
            if not ok:
                result.warnings.append(warning)
                continue
            if key == "permissions.defaultMode" and value in ("auto", "edit") and scope != "user" and not result.trusted:
                result.warnings.append(f"{where}: defaultMode '{value}' ignored — folder not trusted (run /trust to enable)")
                continue
            # Pre-approving the self-heal loop's patches loosens the approval gate,
            # so like 'auto' a repository can't switch it on for itself.
            if key == "selfHeal.autoApply" and value is True and scope != "user" and not result.trusted:
                result.warnings.append(f"{where}: selfHeal.autoApply ignored — folder not trusted (run /trust to enable)")
                continue
            if key in TRUST_GATED_KEYS and scope != "user" and not result.trusted:
                result.warnings.append(f"{where}: '{key}' ignored — folder not trusted (run /trust to enable)")
                continue
            result.values[key] = value
            result.sources[key] = scope
    for text in BUILTIN_DENY:
        result.rules.append(Rule("deny", text, "built-in"))
    return result


_cache: dict[str, tuple[tuple, Settings]] = {}
_cache_lock = threading.Lock()


def current(cwd: str | os.PathLike) -> Settings:
    """``load`` with a cache keyed on the three files' mtimes, so asking before
    every tool call costs three stat() calls and picks up edits made mid-session
    (``/permissions``, ``/config`` or an editor)."""
    cwd = str(cwd)
    stamp = []
    for scope in SCOPES:
        try:
            info = settings_path(scope, cwd).stat()
            stamp.append((info.st_mtime_ns, info.st_size))
        except OSError:
            stamp.append(None)
    key = tuple(stamp)
    with _cache_lock:
        hit = _cache.get(cwd)
        if hit and hit[0] == key:
            return hit[1]
    fresh = load(cwd)
    with _cache_lock:
        _cache[cwd] = (key, fresh)
    return fresh


# ----------------------------------------------------------------- writing


def _write_json(path: Path, data: dict, private: bool) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    handle, temp = tempfile.mkstemp(dir=path.parent, prefix=".settings-", suffix=".tmp")
    try:
        with os.fdopen(handle, "w", encoding="utf-8") as out:
            json.dump(data, out, indent=2, ensure_ascii=False)
            out.write("\n")
        if private:
            os.chmod(temp, 0o600)
        os.replace(temp, path)
    except BaseException:
        try:
            os.unlink(temp)
        except OSError:
            pass
        raise


def _editable(scope: str, cwd) -> tuple[Path, dict]:
    if scope not in SCOPES:
        raise ValueError(f"scope must be one of {', '.join(SCOPES)}")
    path = settings_path(scope, cwd)
    warnings: list = []
    data = _read_json(path, warnings)
    if warnings and path.exists() and path.stat().st_size:
        # never overwrite a file we could not understand
        raise ValueError(warnings[0])
    return path, data


def _after_write(scope: str, path: Path) -> None:
    if scope == "local":
        ignore = path.parent / ".gitignore"
        if not ignore.exists():
            ignore.write_text("settings.local.json\nCORDATA.local.md\n", encoding="utf-8")


def set_value(scope: str, key: str, value, cwd) -> Path:
    """Set one scalar setting (or clear it with ``None``) in one file."""
    ok, warning = (True, None) if value is None else _check(key, value, scope)
    if not ok:
        raise ValueError(warning)
    if key not in SCHEMA:
        raise ValueError(f"unknown setting '{key}'")
    if user_only(key) and scope != "user":
        raise ValueError(f"'{key}' is only read from the user file")
    path, data = _editable(scope, cwd)
    parts = key.split(".")
    node = data
    for part in parts[:-1]:
        node = node.setdefault(part, {})
        if not isinstance(node, dict):
            raise ValueError(f"{path}: '{part}' is not an object")
    if value is None:
        node.pop(parts[-1], None)
    else:
        node[parts[-1]] = value
    _write_json(path, data, private=scope == "user")
    _after_write(scope, path)
    return path


def add_rule(kind: str, rule: str, scope: str, cwd) -> Path:
    if kind not in RULE_KINDS:
        raise ValueError(f"rule kind must be one of {', '.join(RULE_KINDS)}")
    rule = rule.strip()
    if not rule or _SECRET_VALUE.search(rule):
        raise ValueError("empty rule, or it looks like it holds a secret")
    path, data = _editable(scope, cwd)
    rules = data.setdefault("permissions", {}).setdefault(kind, [])
    if rule not in rules:
        rules.append(rule)
    _write_json(path, data, private=scope == "user")
    _after_write(scope, path)
    return path


def remove_rule(rule: str, cwd) -> list[Path]:
    """Remove ``rule`` (any kind) from every file that holds it."""
    changed = []
    for scope in SCOPES:
        path, data = _editable(scope, cwd)
        permissions = data.get("permissions")
        if not isinstance(permissions, dict):
            continue
        touched = False
        for kind in RULE_KINDS:
            items = permissions.get(kind)
            if isinstance(items, list) and rule in items:
                items.remove(rule)
                touched = True
        if touched:
            _write_json(path, data, private=scope == "user")
            changed.append(path)
    return changed


def trust_project(cwd) -> Path:
    path, data = _editable("user", cwd)
    here = os.path.realpath(str(cwd))
    trusted = data.setdefault("trustedProjects", [])
    if not isinstance(trusted, list):
        raise ValueError(f"{path}: 'trustedProjects' must be a list")
    if here not in trusted:
        trusted.append(here)
    _write_json(path, data, private=True)
    return path


def untrust_project(path) -> Path | None:
    """Take ``path`` out of the user's ``trustedProjects`` (by realpath). Revoking only ever lowers what a folder may
    do, so it needs no confirmation. Returns the file written, or None when the folder was not trusted."""
    file, data = _editable("user", path)
    here = os.path.realpath(str(path))
    trusted = data.get("trustedProjects")
    if not isinstance(trusted, list) or not any(os.path.realpath(str(p)) == here for p in trusted if isinstance(p, str)):
        return None
    data["trustedProjects"] = [p for p in trusted if not (isinstance(p, str) and os.path.realpath(p) == here)]
    _write_json(file, data, private=True)
    return file


def trusted_projects(cwd) -> list[str]:
    """Every folder in the user's ``trustedProjects``."""
    _, data = _editable("user", cwd)
    trusted = data.get("trustedProjects")
    return [p for p in trusted if isinstance(p, str)] if isinstance(trusted, list) else []


# ------------------------------------------------------------ instructions

INSTRUCTION_CAP = 16_000


def instruction_files(cwd: str | os.PathLike) -> list[tuple[str, Path]]:
    """(scope, path) of the guidance files that exist: the user's own, the
    project's (``CORDATA.md``, else ``AGENTS.md`` / ``CLAUDE.md`` so a repo set
    up for another tool still works), and this machine's local one."""
    cwd = Path(cwd)
    found: list[tuple[str, Path]] = []
    candidates = [
        ("user", [user_dir() / "CORDATA.md"]),
        ("project", [
            cwd / "CORDATA.md",
            cwd / ".cordata" / "CORDATA.md",
            cwd / "AGENTS.md",
            cwd / "CLAUDE.md",
        ]),
        ("local", [cwd / "CORDATA.local.md"]),
    ]
    for scope, options in candidates:
        for path in options:
            if path.is_file():
                found.append((scope, path))
                break
    return found


def load_instructions(cwd: str | os.PathLike) -> str:
    """The guidance files as one block for the system prompt ('' if none)."""
    sections = []
    budget = INSTRUCTION_CAP
    for scope, path in instruction_files(cwd):
        try:
            text = path.read_text(encoding="utf-8", errors="replace").strip()
        except OSError:
            continue
        if not text or budget <= 0:
            continue
        if len(text) > budget:
            text = text[:budget] + "\n[… truncated]"
        budget -= len(text)
        sections.append(f"## {scope}: {path}\n{text}")
    if not sections:
        return ""
    return "# Instructions from the user's own files\nFollow these for this project:\n\n" + "\n\n".join(sections)


# --------------------------------------------------------- permission rules

_SPECIFIER = {
    "shell": "command",
    "python_repl": "code",
    "file_read": "path",
    "file_write": "path",
    "editor": "path",
    "http_request": "url",
    "web_fetch": "url",
}
_PATH_TOOLS = ("file_read", "file_write", "editor")


def specifier(tool: str, tool_input) -> str | None:
    field_name = _SPECIFIER.get(tool)
    if field_name is None or not isinstance(tool_input, dict):
        return None
    value = tool_input.get(field_name)
    return value if isinstance(value, str) else None


def split_shell(command: str) -> tuple[list[str], bool]:
    """Split a command line on unquoted ``; && || | &`` and newlines.

    Returns (parts, simple). ``simple`` is False when the line has a command
    substitution, a process substitution, or a redirect other than to /dev/null or
    ``2>&1`` — things a wildcard allow rule must not vouch for."""
    command = re.sub(r"\d*>&\d+|\d*>>?\s*/dev/null|&>\s*/dev/null", " ", command)
    parts: list[str] = []
    buffer: list[str] = []
    simple = True
    quote = ""
    index = 0
    while index < len(command):
        char = command[index]
        pair = command[index : index + 2]
        if char == "\\" and quote != "'":
            buffer.append(command[index : index + 2])
            index += 2
            continue
        if quote:
            if char == quote:
                quote = ""
            elif quote == '"' and (pair == "$(" or char == "`"):
                simple = False
            buffer.append(char)
            index += 1
            continue
        if char in "'\"":
            quote = char
            buffer.append(char)
        elif pair in ("$(", "<(", ">(") or char == "`":
            simple = False
            buffer.append(char)
        elif char in "<>":
            simple = False
            buffer.append(char)
        elif pair in ("&&", "||"):
            parts.append("".join(buffer))
            buffer = []
            index += 2
            continue
        elif char in ";|&\n":
            parts.append("".join(buffer))
            buffer = []
        else:
            buffer.append(char)
        index += 1
    parts.append("".join(buffer))
    return [part.strip() for part in parts if part.strip()], simple and not quote


def _glob(pattern: str) -> re.Pattern:
    return re.compile("".join(".*" if part == "*" else re.escape(part) for part in re.split(r"(\*)", pattern)), re.DOTALL)


def _match(pattern: str, text: str) -> bool:
    if _glob(pattern).fullmatch(text):
        return True
    # "git status *" also covers a bare "git status"
    return pattern.endswith(" *") and text == pattern[:-2]


def _normalise_path(value: str, cwd: str) -> str:
    value = os.path.expanduser(value)
    return os.path.normpath(value if os.path.isabs(value) else os.path.join(cwd, value))


def rule_matches(rule: Rule, tool: str, tool_input, cwd: str, *, allow: bool = False) -> bool:
    """Does ``rule`` apply to this call? ``allow`` selects the stricter reading
    used for allow rules (see the module docstring on shell commands)."""
    if rule.tool != tool:
        return False
    pattern = rule.pattern
    if pattern is None:
        return True
    subject = specifier(tool, tool_input)
    if subject is None:
        return False
    if tool in _PATH_TOOLS:
        base = pattern.lstrip()
        if not base.startswith(("*", "~", "/")):
            pattern = os.path.join(cwd, base)
        else:
            pattern = os.path.expanduser(base)
        return _match(pattern, _normalise_path(subject, cwd))
    if tool != "shell":
        return _match(pattern, subject.strip())
    command = subject.strip()
    parts, simple = split_shell(command)
    wildcard = "*" in pattern
    if not allow:  # deny / ask: any part, or the whole line
        return _match(pattern, command) or any(_match(pattern, part) for part in parts)
    if not wildcard:
        return pattern == command or (len(parts) == 1 and pattern == parts[0])
    return simple and bool(parts) and all(_match(pattern, part) for part in parts)


@dataclass
class Verdict:
    action: str  # deny | ask | allow
    rule: Rule

    def explain(self) -> str:
        return f"{self.rule.kind} rule {self.rule.text!r} ({self.rule.scope})"


def evaluate(settings: Settings, tool: str, tool_input, cwd: str) -> Verdict | None:
    """First matching verdict, ``deny`` before ``ask`` before ``allow``; None if
    no rule says anything (the mode's default applies)."""
    for kind in ("deny", "ask", "allow"):
        rules = settings.rules_for(kind)
        if kind == "allow":
            # allow needs *every* shell part covered, possibly by different rules
            for rule in rules:
                if rule_matches(rule, tool, tool_input, cwd, allow=True):
                    return Verdict(kind, rule)
            if tool == "shell":
                covered = _shell_covered_by_allow(rules, tool_input)
                if covered:
                    return Verdict(kind, covered)
        else:
            for rule in rules:
                if rule_matches(rule, tool, tool_input, cwd):
                    return Verdict(kind, rule)
    return None


def _shell_covered_by_allow(rules: list[Rule], tool_input) -> Rule | None:
    command = specifier("shell", tool_input)
    if command is None:
        return None
    parts, simple = split_shell(command.strip())
    if not simple or len(parts) < 2:
        return None
    patterns = [rule for rule in rules if rule.tool == "shell" and rule.pattern is not None]
    first = None
    for part in parts:
        hit = next((rule for rule in patterns if _match(rule.pattern, part)), None)
        if hit is None:
            return None
        first = first or hit
    return first


def suggest_rule(tool: str, tool_input) -> str | None:
    """The narrowest rule that would have allowed this very call, offered as
    "Always allow …" — or None when there is no sensible narrow form."""
    if tool == "python_repl":
        return None
    subject = specifier(tool, tool_input)
    if tool in _SPECIFIER:
        if not subject or "\n" in subject or "*" in subject or ")" in subject:
            return None
        return f"{tool}({subject.strip()})"
    return tool


if __name__ == "__main__":  # python settings_store.py [cwd]  — what the backend will use
    import sys

    target = sys.argv[1] if len(sys.argv) > 1 else os.getcwd()
    loaded = load(target)
    print(json.dumps(
        {
            "trusted": loaded.trusted,
            "paths": loaded.paths,
            "values": loaded.values,
            "sources": loaded.sources,
            "rules": [[r.kind, r.text, r.scope, r.active] for r in loaded.rules],
            "warnings": loaded.warnings,
        },
        indent=2,
    ))
