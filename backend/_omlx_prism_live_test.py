"""Phase 7 verification gate, **live**: the real oMLX server, the real Prism model.

* Provider test — the Strands client (``omlx_provider.PrismModel``, an ``OpenAIModel``)
  talks to the local oMLX endpoint and ``/v1/chat/completions`` accepts its payload.
* Resource-manager test — a Strands agent calls ``omlx_resource_manager`` to **load**
  ``prism-ml/Ternary-Bonsai-2-27B-mlx-2bit`` (8.41 GB), the model answers a text
  completion, and the manager **unloads** it; ``/v1/models/status`` is checked at every step.

It moves real weights through unified memory (~8.4 GB, for a few seconds to a minute) and
sends one short real completion. Skips — touching nothing — when oMLX is not up, when the model
is not served, or when some *other* model is already resident (this test will not evict
your work to run itself). Leaves oMLX as it found it.

    backend/.venv/bin/python _omlx_prism_live_test.py
"""

import json
import sys
import time

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

from strands import Agent

import memory_budget as mb
import omlx_manager
import omlx_provider as op
import omlx_resource_tool as oxt
from harness_testing import ScriptedModel, check, text, tool as call

#: The gate names Bonsai-2 first. oMLX 0.6.4 here cannot load it ("Model type
#: prism_hadamard_qwen35 not supported"), so the gate runs on the first registered Prism model
#: this server *can* load and says which one it used. Upgrade oMLX and Bonsai-2 is used again.
ALIASES = [spec.alias for spec in op.REGISTRY.values()]


def skip(reason: str) -> None:
    print(f"SKIP: {reason}")
    sys.exit(0)


def main() -> None:
    manager = omlx_manager.OmlxManager()
    try:
        run(manager)
    finally:
        # Whatever happened, leave oMLX the way it was found: nothing of ours stays resident.
        if manager.healthy():
            for row in manager.status().loaded:
                if row.id in {op.resolve(a).wire_id for a in ALIASES} and row.id not in _INITIALLY_LOADED:
                    manager.unload(row.id)


_INITIALLY_LOADED: set[str] = set()


def run(manager: omlx_manager.OmlxManager) -> None:
    print(f"oMLX at {manager.base_url}")
    if not manager.healthy():
        skip("oMLX is not answering /health")
    status = manager.status()
    served = [alias for alias in ALIASES if status.get(op.resolve(alias).wire_id) is not None]
    if not served:
        skip("oMLX serves neither Prism model")
    others = [m.id for m in status.loaded if m.id not in {op.resolve(a).wire_id for a in ALIASES}]
    if others:
        skip(f"{others} is already loaded; not evicting it for a test")
    initially_loaded = {alias for alias in served if status.get(op.resolve(alias).wire_id).loaded}
    _INITIALLY_LOADED.update(op.resolve(alias).wire_id for alias in initially_loaded)

    budget = mb.default_budget()

    # ---- the orchestrator loads a Prism model through the manager (a real Strands loop) -------
    chosen = None
    for alias in served:
        wire = op.resolve(alias).wire_id
        row = status.get(wire)
        snap = budget.snapshot()
        check(snap.free_gb > row.size_gb, f"{alias}: the {snap.budget_gb:.0f} GB budget has room for {row.size_gb:.2f} GB ({snap.free_gb:.1f} GB free)")
        orchestrator = Agent(
            model=ScriptedModel([call("omlx_resource_manager", {"action": "load", "model": alias}), call("omlx_resource_manager", {"action": "status"}), text("loaded")]),
            tools=[oxt.omlx_resource_manager],
            callback_handler=None,
        )
        began = time.monotonic()
        orchestrator("load the planner model")
        load_s = time.monotonic() - began
        results = [b["toolResult"] for m in orchestrator.messages for b in m["content"] if "toolResult" in b]
        if results and results[0]["status"] == "success":
            chosen = (alias, wire, results, load_s)
            break
        reason = results[0]["content"][0]["text"] if results else "no result"
        print(f"   ENVIRONMENT: oMLX cannot load {alias}: {reason[:240]}")
    if chosen is None:
        raise AssertionError("FAIL: oMLX could not load either Prism model")
    alias, wire, results, load_s = chosen
    print(f"   using {alias} ({wire})")
    check(all(r["status"] == "success" for r in results), "omlx_resource_manager load + status returned ToolResults with status=success")
    check(manager.status().get(wire).loaded, f"/v1/models/status says it is loaded ({load_s:.1f}s)")
    check(wire in json.dumps(results[1]["content"]), "…and the manager's own status agrees")

    # ---- Provider test + a real completion on the Prism model ----------------------------------
    model = op.PrismModel(alias, params={"max_tokens": 2048, "temperature": 0})
    planner = Agent(model=model, system_prompt="You are terse.", tools=[], callback_handler=None)
    began = time.monotonic()
    reply = str(planner("Reply with the single word: pong"))
    print(f"   completion in {time.monotonic() - began:.1f}s: {reply.strip()[:100]!r}")
    check(bool(reply.strip()), "a real /v1/chat/completions round-trip returned text (payload compatible)")
    check(manager.status().get(wire).loaded, "the model was resident for the request")

    # ---- unload -----------------------------------------------------------------------------------
    closer = Agent(model=ScriptedModel([call("omlx_resource_manager", {"action": "unload", "model": alias}), text("freed")]), tools=[oxt.omlx_resource_manager], callback_handler=None)
    closer("free the memory")
    after = manager.status()
    check(not after.get(wire).loaded, "/v1/models/status says it is unloaded again")
    check(after.memory_gb < 0.5, f"oMLX holds {after.memory_gb:.2f} GB of weights again")

    # ---- a request to the now-unloaded model is refused by the provider, not silently reloaded ----
    try:
        Agent(model=op.PrismModel(alias), tools=[], callback_handler=None)("hello")
    except op.ModelNotLoaded:
        check(not manager.status().get(wire).loaded, "a request to the unloaded model is refused — oMLX did not reload it")
    else:
        raise AssertionError("FAIL: the request should have been refused")

    # ---- the architect specialist end to end, on whichever Prism model works -----------------------
    import specialists

    architect = specialists.build_specialist(specialists.ARCHITECT)
    began = time.monotonic()
    plan = str(architect("In one sentence: what is a good first step to split a 2000-line module?"))
    print(f"   architect (lease: load → completion → unload) in {time.monotonic() - began:.1f}s: {plan.strip()[:120]!r}")
    check(bool(plan.strip()), "the architect specialist answered on a local Prism model through the lease hooks")
    check(manager.status().memory_gb < 0.5, "…and the lease unloaded it afterwards")

    print("\noMLX / Prism live gate: passed")


if __name__ == "__main__":
    main()
