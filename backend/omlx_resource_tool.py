"""``omlx_resource_manager`` — load and unload oMLX weights on purpose.

oMLX leaves every model inactive ("0 loaded · 0.0 B") until something asks for it.
This tool is that ask, made deliberately and made safely:

* ``load``       — ``memory_budget.load_omlx``: make room first (stop the least recently
  used idle models), refuse if the model cannot fit, then load and verify against the
  server's own status. A Prism model is never loaded into memory that is full.
* ``unload``     — drop one model. ``unload_all`` drops every unpinned one.
* ``status``     — what is resident, how big, against which ceiling.

The Orchestrator's rule (spec, Phase 7): *load before delegating to a Prism model;
unload when the task completes, or before a heavy ClaudeCodeEngineer task*. The tool is
the LLM-visible handle for that; ``omlx_provider.PrismLeaseHooks`` (load before a Prism
agent runs, unload after) and ``claude_code_tool`` (``release_for_heavy_task``) make it
true without depending on the model remembering.

Never gated: it moves weights between disk and memory and changes nothing a person
would want to review (``acp_server._SAFE_RESOURCE_ACTIONS``).
"""

from __future__ import annotations

from typing import Any

from strands import tool

import harness_events
import memory_budget
import omlx_manager

ACTIONS = ("status", "load", "unload", "unload_all")


def _result(ok: bool, text: str, data: dict[str, Any]) -> dict[str, Any]:
    return {"status": "success" if ok else "error", "content": [{"text": text}, {"json": data}]}


def _announce(event: str, payload: dict[str, Any]) -> None:
    harness_events.emit("memory", {"event": event, "runtime": "omlx", **payload})


def _unreclaimed(manager: omlx_manager.OmlxManager, ids: list[str]) -> str:
    """The unload went through but the pool did not shrink (``OmlxManager.unload``): say so, with the one fix."""
    held = {model: manager.unreclaimed[model] for model in ids if manager.unreclaimed.get(model)}
    if not held:
        return ""
    gb = sum(held.values()) / omlx_manager.GB
    return f" — but oMLX still holds {gb:.1f} GB for {', '.join(held)}; restart the oMLX server to give it back to macOS"


def _describe(status: omlx_manager.OmlxStatus) -> str:
    loaded = status.loaded
    rows = "\n".join(f"  {row.id}  {row.size_gb:.1f} GB" for row in loaded) or "  (nothing loaded)"
    return f"oMLX: {len(loaded)} loaded · {status.memory_gb:.1f} GB of {status.ceiling_gb:.1f} GB pool\n{rows}"


@tool
def omlx_resource_manager(action: str, model: str = "") -> dict:
    """Load or unload a local oMLX model (the Prism models) in unified memory.

    Models start unloaded. Load one before delegating work to it; unload it when that
    work is done, or before any heavy task (the Claude Code engineer), so memory is free.

    Args:
        action: "status" (what is loaded), "load", "unload" (one model) or "unload_all".
        model: the model for load / unload, e.g. "prism-ml/Ternary-Bonsai-2-27B-mlx-2bit".
    """
    action = (action or "").strip().lower()
    if action not in ACTIONS:
        return _result(False, f"unknown action {action!r}; use one of {', '.join(ACTIONS)}", {"action": action})
    budget = memory_budget.default_budget()
    manager = budget.omlx
    try:
        if action == "status":
            status = manager.status()
            return _result(True, _describe(status), status.to_json())

        if action == "unload_all":
            dropped = manager.unload_all()
            status = manager.status()
            _announce("unload_all", {"models": [row.id for row in dropped], "memory_gb": round(status.memory_gb, 2)})
            names = ", ".join(row.id for row in dropped) or "nothing was loaded"
            held = _unreclaimed(manager, [row.id for row in dropped])
            return _result(True, f"unloaded: {names}{held}\n{_describe(status)}", {"action": action, "unloaded": [row.id for row in dropped], **status.to_json()})

        if not model.strip():
            return _result(False, f"{action} needs a model", {"action": action})

        if action == "load":
            row, room = budget.load_omlx(model.strip())
            status = manager.status()
            _announce("load", {"model": row.id, "size_gb": round(row.size_gb, 2), "memory_gb": round(status.memory_gb, 2), "freed": [r.name for r in (room.freed if room else [])]})
            freed = f" (stopped first: {', '.join(r.name for r in room.freed)})" if room and room.freed else ""
            return _result(True, f"loaded {row.id}{freed}\n{_describe(status)}", {"action": action, "model": row.id, "freed": [r.to_json() for r in (room.freed if room else [])], **status.to_json()})

        row = manager.unload(model.strip())  # action == "unload"
        status = manager.status()
        _announce("unload", {"model": row.id, "memory_gb": round(status.memory_gb, 2)})
        held = _unreclaimed(manager, [row.id])
        return _result(True, f"unloaded {row.id}{held}\n{_describe(status)}", {"action": action, "model": row.id, **status.to_json()})
    except memory_budget.MemoryBudgetExceeded as error:
        return _result(False, str(error), {"action": action, "model": model, "headroom": error.headroom.to_json() if error.headroom else None})
    except omlx_manager.OmlxError as error:
        return _result(False, str(error), {"action": action, "model": model})


def is_safe_call(tool_input: Any) -> bool:
    return isinstance(tool_input, dict) and str(tool_input.get("action", "")).strip().lower() in ACTIONS
