"""Validates tools the self-healing loop (skill_forge_tool.py) writes to
backend/skills_generated/, before they ever run for real — and, separately,
hot-loads a newly-written one onto the live agent that just wrote it, so it
doesn't need the whole backend process to restart before it's callable.

Per docs/self-healing-architecture.md §2 step 4 ("Interception"): this is a
Strands hook, not a tool, so it runs on the agent's normal dispatch path —
`registry.add_callback(BeforeToolCallEvent, ...)` is the documented
`HookProvider` pattern (strandsagents.com, hooks concept page §2.1/§9), and
mutating `event.selected_tool` / `event.tool_use` on failure is the same
"Tool Interception" substitution shown in that page's own §5.2 cookbook
entry. `file_write` (the step that puts a file here in the first place) is
already one of main.py's `_APPROVAL_REQUIRED_TOOLS`, so a human has already
approved the source before this hook ever sees it — this is the second,
independent gate, not the first.

Every `BeforeToolCallEvent` fires through here, for every tool, not just
generated ones — `validate_generated_skill` is a no-op unless the selected
tool's name matches a file under skills_generated/. Same for
`hot_reload_generated_skill` on `AfterToolCallEvent`: a no-op unless the
tool that just ran was `file_write`/`editor` and it wrote inside
skills_generated/.

Also exports `find_banned_usage` and `GUARDRAIL_FILES` for proposer_tool.py's
static gate to import directly — see this file for why: a proposed harness
change must be checked by the same banned-imports/calls rule a generated
skill is, and must never be allowed to target this file itself.
"""

import ast
import importlib.util
from pathlib import Path

import strands_tools.python_repl as python_repl_module
from strands import tool
from strands.hooks import AfterToolCallEvent, BeforeToolCallEvent, HookProvider, HookRegistry
from strands.tools.decorator import DecoratedFunctionTool

SKILLS_GENERATED_DIR = Path(__file__).parent.resolve() / "skills_generated"

# Tools whose successful call might have just created a file — the only two
# with write access to skills_generated/ at all (see sub_agent_common.py's
# DENIED_TOOLS for the same two named for the same reason elsewhere).
_WRITE_TOOL_NAMES = {"file_write", "editor"}

# subprocess is banned wholesale (any alias); os.system and eval/exec are
# banned as specific calls, per docs/self-healing-architecture.md §2 step 4a.
# Not a general Python sandbox — a narrow check against the shell-escape
# patterns a generated tool has no legitimate reason to need.
#
# Widened by the Phase 11 audit (system_audit_report.md): a generated tool runs
# *in the agent's process*, so a raw socket, a spawned child with its own
# environment, or a foreign-function call reaches the network around the Agent
# Vault's proxy variables. The process-spawning and raw-network modules go
# wholesale; every os-level spawn goes by name, including `from os import …`
# and `getattr(os, "…")` spellings.
_BANNED_MODULES = {"subprocess", "socket", "ctypes", "multiprocessing", "pty", "pexpect", "_posixsubprocess"}
_BANNED_CALL_NAMES = {"eval", "exec", "__import__"}
_BANNED_OS_CALLS = {
    "system", "popen", "fork", "forkpty", "posix_spawn", "posix_spawnp",
    "execl", "execle", "execlp", "execlpe", "execv", "execve", "execvp", "execvpe",
    "spawnl", "spawnle", "spawnlp", "spawnlpe", "spawnv", "spawnve", "spawnvp", "spawnvpe",
}
_BANNED_ATTR_CALLS = {("os", name) for name in _BANNED_OS_CALLS} | {
    ("asyncio", "create_subprocess_exec"),
    ("asyncio", "create_subprocess_shell"),
    ("importlib", "import_module"),
}

_SYNTHETIC_LITERALS = {
    "str": '"synthetic-test-value"',
    "int": "1",
    "float": "1.0",
    "bool": "True",
    "list": "[]",
    "dict": "{}",
}

# Files no drafted change may ever target — checked against a proposed
# write's own destination path, not against what the change's *content*
# claims to do. proposer_tool.py's _run_static_gate imports and checks this
# directly, so nothing that tool drafts can talk its way past it: the
# refusal is Python this module owns, not a rule the model is merely asked
# to follow. Keep this set here (not in proposer_tool.py) so there is one
# place, not two, that says what's off-limits.
GUARDRAIL_FILES = {"skill_validation_hooks.py", "proposer_tool.py"}


def find_banned_usage(tree: ast.AST) -> str | None:
    """AST scan for the shell-escape patterns generated/proposed code has no
    legitimate reason to need — shared by SkillValidationHooks (checking an
    already-written skills_generated/*.py file) and proposer_tool.py's
    static gate (checking drafted-but-not-yet-written content), so both
    apply the exact same rule instead of two copies drifting apart.
    """
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                if alias.name.split(".")[0] in _BANNED_MODULES:
                    return f"imports banned module '{alias.name}'"
        elif isinstance(node, ast.ImportFrom):
            if node.module and node.module.split(".")[0] in _BANNED_MODULES:
                return f"imports banned module '{node.module}'"
            for alias in node.names:
                if node.module and (node.module, alias.name) in _BANNED_ATTR_CALLS:
                    return f"imports banned '{node.module}.{alias.name}'"
        elif isinstance(node, ast.Call):
            func = node.func
            if isinstance(func, ast.Name) and func.id in _BANNED_CALL_NAMES:
                return f"calls banned builtin '{func.id}'"
            if isinstance(func, ast.Attribute) and isinstance(func.value, ast.Name):
                if (func.value.id, func.attr) in _BANNED_ATTR_CALLS:
                    return f"calls banned '{func.value.id}.{func.attr}'"
            if isinstance(func, ast.Name) and func.id == "getattr" and len(node.args) >= 2:
                target, name = node.args[0], node.args[1]
                if isinstance(target, ast.Name) and isinstance(name, ast.Constant) and (target.id, name.value) in _BANNED_ATTR_CALLS:
                    return f"reaches banned '{target.id}.{name.value}' through getattr"
    return None


@tool
def skill_rejected(reason: str) -> str:
    """Stand-in for a generated tool that failed validation.

    SkillValidationHooks swaps this in as the selected tool instead of
    running untrusted code, so the model sees a structured rejection reason
    on its next turn instead of a silent failure or, worse, an executed
    exploit.

    Args:
        reason: Why the generated tool was rejected.
    """
    return f"Generated tool rejected before execution: {reason}"


class SkillValidationHooks(HookProvider):
    """Gate on any tool loaded from backend/skills_generated/.

    Tracks which generated tools have already passed validation this
    process's lifetime (`_trusted`) so a tool isn't re-parsed and re-dry-run
    on every single call once a human has approved it and it has passed —
    per §2 step 5's "promoted to trusted" note. Trust is in-memory only and
    resets on backend restart, which is intentional: skills_generated/ is an
    audit trail, not an unconditional allowlist.
    """

    def __init__(self) -> None:
        self._trusted: set[str] = set()

    def register_hooks(self, registry: HookRegistry) -> None:
        registry.add_callback(BeforeToolCallEvent, self.validate_generated_skill)
        registry.add_callback(AfterToolCallEvent, self.hot_reload_generated_skill)

    def hot_reload_generated_skill(self, event: AfterToolCallEvent) -> None:
        """Register a just-written skills_generated/*.py tool on the live
        agent immediately, instead of leaving it uncallable until the whole
        backend process restarts (agent_common.discover_generated_tools()
        otherwise only ever runs once, at import time).

        Fires only after a genuinely successful file_write/editor call
        (event.result is a dict with status="success" — an Exception, or a
        status="error" result, means nothing was actually written). Only
        registers it on *this* agent: for the AG-UI/web front door that
        means the rest of this same thread's conversation, since
        ag_ui_strands caches one live Agent per thread and reuses it across
        turns (confirmed reading its source: `self._agents_by_thread`) — a
        *different*, already-open thread, or a brand-new one, still needs a
        restart, because ag_ui_strands derives every thread's agent by
        deep-copying the one template Agent built once at process start,
        not by re-scanning skills_generated/ per thread. ACP doesn't have
        that limitation: agent_common.build_agent_for_session() re-scans
        the directory on every call, so every new ACP session picks up
        anything written since the process started, restart or not — see
        that function's own comment.

        IMPORTANT gating caveat, not a bug but worth being explicit about: a
        tool hot-loaded this way still gets validate_generated_skill's
        AST-scan + sandboxed dry-run on its first real call (that hook is
        unconditional on any tool name matching a skills_generated/ file,
        independent of anything below) — but it does NOT get the
        interrupt-based human-approval pause shell/python_repl/file_write
        and every *restart-registered* generated tool get, because that
        gate is driven by a name list (main.py's _APPROVAL_REQUIRED_TOOLS /
        acp_server.py's APPROVAL_REQUIRED_TOOLS) computed once, statically,
        before this tool's name existed. After the next restart the tool is
        part of the normal static list and gets full parity (both gates).
        Extending the live interrupt set at runtime to close this gap too
        would need verified changes in main.py/acp_server.py this session
        didn't have room to do carefully — deliberately left as a known,
        documented gap rather than a guessed-at fix to security-relevant
        code.

        Failure anywhere here (bad path, file doesn't parse, no @tool
        function inside it) just means the tool isn't hot-loaded — the
        existing restart-based path still works exactly as before.
        """
        if event.tool_use.get("name") not in _WRITE_TOOL_NAMES:
            return
        result = event.result
        if not isinstance(result, dict) or result.get("status") != "success":
            return

        raw_path = (event.tool_use.get("input") or {}).get("path")
        if not raw_path:
            return

        try:
            written = Path(raw_path).expanduser().resolve()
        except (OSError, ValueError):
            return
        if not written.is_relative_to(SKILLS_GENERATED_DIR) or written.suffix != ".py":
            return

        tool_name = written.stem
        if tool_name in event.agent.tool_registry.registry:
            return  # already registered this process — a rewrite of an existing generated tool, not a new one

        try:
            spec = importlib.util.spec_from_file_location(f"skills_generated.{tool_name}", written)
            if spec is None or spec.loader is None:
                return
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
        except Exception:
            return  # a broken generated file just doesn't get hot-loaded, same as discover_generated_tools()'s own "continue" on import failure

        new_tools = [attr for attr in vars(module).values() if isinstance(attr, DecoratedFunctionTool)]
        if new_tools:
            event.agent.tool_registry.process_tools(new_tools)

    def validate_generated_skill(self, event: BeforeToolCallEvent) -> None:
        name = event.tool_use.get("name")
        if not name or name in self._trusted:
            return

        path = SKILLS_GENERATED_DIR / f"{name}.py"
        if not path.exists():
            return  # not a generated tool — built-ins pass through untouched

        source = path.read_text()
        try:
            tree = ast.parse(source, filename=str(path))
        except SyntaxError as exc:
            self._reject(event, name, f"file does not parse as Python: {exc}")
            return

        banned_reason = self._find_banned_usage(tree)
        if banned_reason:
            self._reject(event, name, banned_reason)
            return

        func = self._find_tool_function(tree, name)
        decoration_reason = self._check_tool_decoration(func, name)
        if decoration_reason:
            self._reject(event, name, decoration_reason)
            return

        # Static checks passed. A failed dry-run does NOT reject the tool
        # outright (§2 step 4e) — it's left unmodified and allowed to run
        # for real; if the dry run's prediction was right, the real call
        # produces its own AfterToolCallEvent error, which the
        # orchestrator's bounded retry logic is what's meant to handle it,
        # not this hook.
        dry_run_failure = self._dry_run(path, name, func)
        if dry_run_failure is None:
            self._trusted.add(name)

    def _reject(self, event: BeforeToolCallEvent, name: str, reason: str) -> None:
        event.selected_tool = skill_rejected
        event.tool_use["name"] = "skill_rejected"
        event.tool_use["input"] = {"reason": f"Generated tool '{name}' failed validation: {reason}"}

    def _find_banned_usage(self, tree: ast.AST) -> str | None:
        return find_banned_usage(tree)

    def _find_tool_function(self, tree: ast.AST, name: str) -> ast.FunctionDef | None:
        for node in ast.walk(tree):
            if isinstance(node, ast.FunctionDef) and node.name == name:
                return node
        return None

    def _check_tool_decoration(self, func: ast.FunctionDef | None, name: str) -> str | None:
        if func is None:
            return f"no function named '{name}' found in the generated file"
        is_decorated = any(
            (isinstance(d, ast.Name) and d.id == "tool") or (isinstance(d, ast.Attribute) and d.attr == "tool")
            for d in func.decorator_list
        )
        if not is_decorated:
            return f"function '{name}' is not @tool-decorated"
        docstring = ast.get_docstring(func)
        if not docstring or not docstring.strip():
            return f"function '{name}' has no docstring"
        return None

    def _dry_run(self, path: Path, name: str, func: ast.FunctionDef) -> str | None:
        """Sandboxed dry-run against synthetic args, inside python_repl.

        Calls the already-HITL-gated python_repl tool function directly (the
        same pattern the old local_swarm tool used to call the raw `swarm`
        function) — this is a programmatic sandbox invocation from inside
        our own hook code, not a tool call the agent itself makes, so it
        does not re-trigger the interrupt_on_call approval a real agent-
        initiated python_repl call would.
        """
        call_args = self._synthetic_kwargs_source(func)
        script = (
            "import importlib.util\n"
            f"spec = importlib.util.spec_from_file_location({f'_dry_run_{name}'!r}, {str(path)!r})\n"
            "module = importlib.util.module_from_spec(spec)\n"
            "spec.loader.exec_module(module)\n"
            f"fn = getattr(module, {name!r})\n"
            "try:\n"
            f"    fn({call_args})\n"
            "    print('SKILL_FORGE_DRY_RUN: OK')\n"
            "except Exception as exc:\n"
            "    print(f'SKILL_FORGE_DRY_RUN: FAIL {exc!r}')\n"
        )
        result = python_repl_module.python_repl(
            tool={"toolUseId": f"skill-forge-dry-run-{name}", "name": "python_repl", "input": {"code": script, "interactive": False}}
        )
        output = "".join(block.get("text", "") for block in result.get("content", []))
        if "SKILL_FORGE_DRY_RUN: OK" in output:
            return None
        return output.strip() or "dry run produced no output"

    def _synthetic_kwargs_source(self, func: ast.FunctionDef) -> str:
        args = func.args.args
        n_required = len(args) - len(func.args.defaults)
        parts = []
        for i, arg in enumerate(args[:n_required]):
            annotation = ast.unparse(arg.annotation) if arg.annotation else "str"
            literal = _SYNTHETIC_LITERALS.get(annotation, '"synthetic-test-value"')
            parts.append(f"{arg.arg}={literal}")
        return ", ".join(parts)
