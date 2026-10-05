"""``ollama_resource_manager`` — the Orchestrator's hands on local Ollama weights.

A Strands tool over the ``ollama`` CLI (``ollama_cli.py``): ``list``, ``ps``, ``pull``,
``rm``, ``stop`` — plus the two actions the memory-safety rule needs:

* ``status``           — the unified-memory budget and what is resident in it;
* ``ensure_headroom``  — free ``needed_gb`` for something heavy by stopping the least
  recently used idle models (``memory_budget``), *before* loading it.

The rule it exists for: the host has a 48 GB unified-memory ceiling and a 32B model
is ~19 GB. A heavy task must first make room, not discover on its own that there is
none. The system prompt says so (``MEMORY_GUIDANCE``) and — because a prompt is a
request, not a guarantee — the deterministic callers (``omlx_resource_tool`` ``load``,
``claude_code_tool``) run the same ``ensure_headroom`` themselves.

Approval, decided where every other tool's is (``acp_server._ApprovalHandler``):
``list`` / ``ps`` / ``status`` / ``stop`` / ``ensure_headroom`` never ask — they
only read, or free memory that reloads on demand; ``pull`` (a multi-gigabyte
download) and ``rm`` (deletes weights) always ask, even in ``auto`` mode.
"""

from __future__ import annotations

from typing import Any

from strands import tool

import harness_events
import memory_budget
import ollama_cli

#: Actions that change nothing on disk and cost nothing to undo — never asked about.
SAFE_ACTIONS = frozenset({"list", "ps", "status", "stop", "ensure_headroom"})
#: Actions the gate always asks about, whatever the permission mode.
ASKING_ACTIONS = frozenset({"pull", "rm"})
ACTIONS = tuple(sorted(SAFE_ACTIONS | ASKING_ACTIONS))

MEMORY_GUIDANCE = (
    "Local model memory: the host has a 48 GB unified-memory ceiling shared by every model, LanceDB and Laya. "
    "Before work that needs a large local model, call ollama_resource_manager(action='ensure_headroom', needed_gb=<size>) "
    "(and omlx_resource_manager to load/unload oMLX weights); never load a big model into memory that is already full."
)


def _result(ok: bool, text: str, data: dict[str, Any]) -> dict[str, Any]:
    return {"status": "success" if ok else "error", "content": [{"text": text}, {"json": data}]}


def _announce(event: str, payload: dict[str, Any]) -> None:
    harness_events.emit("memory", {"event": event, "runtime": "ollama", **payload})


@tool
def ollama_resource_manager(action: str, model: str = "", needed_gb: float = 0.0) -> dict:
    """Manage local Ollama models and the machine's unified-memory budget.

    Use this *before* a task that needs a large local model, to free memory first.

    Args:
        action: one of
            "status" — memory budget and what is resident now;
            "list" — models installed on disk (`ollama list`);
            "ps" — models loaded in memory (`ollama ps`);
            "stop" — unload a loaded model from memory (`ollama stop <model>`), keeps it on disk;
            "ensure_headroom" — free `needed_gb` of memory by stopping idle models, least recently used first;
            "pull" — download a model (`ollama pull`; always needs the user's approval);
            "rm" — delete a model from disk (`ollama rm`; always needs the user's approval).
        model: the model name for stop / pull / rm, e.g. "qwen3:32b".
        needed_gb: for ensure_headroom, how much memory the next model needs, in GB.
    """
    action = (action or "").strip().lower()
    if action not in ACTIONS:
        return _result(False, f"unknown action {action!r}; use one of {', '.join(ACTIONS)}", {"action": action})
    budget = memory_budget.default_budget()
    cli = budget.ollama

    try:
        if action == "status":
            snapshot = budget.snapshot()
            return _result(True, _describe(snapshot), snapshot.to_json())

        if action == "ensure_headroom":
            if needed_gb <= 0:
                return _result(False, "ensure_headroom needs needed_gb > 0 (the size of what you are about to load)", {"action": action})
            room = budget.ensure_headroom(needed_gb, reason="requested by the orchestrator")
            _announce("headroom", room.to_json())
            return _result(room.ok, room.summary(), room.to_json())

        if action in ("list", "ps"):
            result = cli.list() if action == "list" else cli.ps()
            return _result(result.ok, result.text or "(no output)", {"action": action, "exit_code": result.exit_code})

        name = ollama_cli.valid_model(model)
        if action == "stop":
            result = cli.stop(name)
            snapshot = budget.snapshot()
            _announce("stop", {"model": name, "ok": result.ok, "free_gb": round(snapshot.free_gb, 2)})
            text = (result.text or f"stopped {name}") + f"\nfree model budget now {snapshot.free_gb:.1f} GB"
            return _result(result.ok, text, {"action": action, "model": name, "exit_code": result.exit_code, "memory": snapshot.to_json()})

        if action == "pull":
            _announce("pull", {"model": name, "phase": "started"})
            with harness_events.Task(f"ollama pull {name}", source="memory") as task:
                result = cli.pull(name)
                task.update(message=result.text[:120])
            _announce("pull", {"model": name, "phase": "finished", "ok": result.ok})
            return _result(result.ok, result.text or f"pulled {name}", {"action": action, "model": name, "exit_code": result.exit_code, "seconds": result.seconds})

        result = cli.rm(name)  # action == "rm"
        _announce("rm", {"model": name, "ok": result.ok})
        return _result(result.ok, result.text or f"removed {name}", {"action": action, "model": name, "exit_code": result.exit_code})
    except ollama_cli.OllamaCliError as error:
        return _result(False, str(error), {"action": action, "model": model})


def _describe(snapshot: memory_budget.Snapshot) -> str:
    rows = "\n".join(f"  {r.runtime:6} {r.name}  {r.size_gb:.1f} GB" for r in snapshot.residents) or "  (no local model is loaded)"
    return (
        f"Model memory budget: {snapshot.used_gb:.1f} of {snapshot.budget_gb:.1f} GB used "
        f"({snapshot.ceiling_gb:.0f} GB ceiling − {snapshot.reserve_gb:.0f} GB reserved), {snapshot.free_gb:.1f} GB free.\n{rows}"
    )


def asks_for_approval(tool_input: Any) -> bool:
    """Does this call need a person, whatever the mode? (``pull`` and ``rm``.)"""
    return isinstance(tool_input, dict) and str(tool_input.get("action", "")).strip().lower() in ASKING_ACTIONS


def is_safe_call(tool_input: Any) -> bool:
    """Is this call one the gate may wave through without asking?"""
    return isinstance(tool_input, dict) and str(tool_input.get("action", "")).strip().lower() in SAFE_ACTIONS
