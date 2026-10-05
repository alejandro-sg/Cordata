"""The unified-memory layer: the oMLX manager, the Ollama CLI wrapper, the budget that
decides who gets evicted, and the two Strands tools over them. Real HTTP against
``fake_omlx.FakeOmlx`` (state is real: load/unload change what ``status`` says); the
``ollama`` binary is a recording stub, so nothing is ever pulled, removed or stopped.

    backend/.venv/bin/python _memory_budget_smoke_test.py
"""

import os
import subprocess
import time

import provider_settings  # noqa: F401  (imported before agent_common, which chdirs — see main.py)

import memory_budget as mb
import ollama_cli
import ollama_resource_tool as ort
import omlx_manager
import omlx_resource_tool as oxt
from fake_omlx import FakeOmlx

GB = 1024**3
PRISM_2 = "prism-ml/Ternary-Bonsai-2-27B-mlx-2bit"
PRISM_1 = "prism-ml/Ternary-Bonsai-27B-mlx-2bit"
BIG = "Qwen2.5-Coder-32B-Instruct-4bit"


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


class StubOllama(ollama_cli.OllamaCli):
    """The real wrapper, with a recording runner instead of a subprocess and a
    scripted ``/api/ps``."""

    def __init__(self, running=()):
        self.calls: list[list[str]] = []
        self.envs: list[dict] = []
        self.resident = list(running)
        super().__init__(binary="/bin/echo", runner=self._run, ps_reader=lambda: list(self.resident))

    def _run(self, argv, **kwargs):
        self.calls.append(list(argv))
        self.envs.append(kwargs.get("env") or {})
        if argv[1] == "stop":
            self.resident = [m for m in self.resident if m.name != argv[2]]
        out = "pulling manifest\r\npulling abc 42%\r\nsuccess\n" if argv[1] == "pull" else f"ran {' '.join(argv[1:])}\n"
        return subprocess.CompletedProcess(argv, 0, out, "")


def running(name: str, gb: float, expires_in: float) -> ollama_cli.RunningModel:
    return ollama_cli.RunningModel(name, int(gb * GB), int(gb * GB), time.time() + expires_in)


def budget_for(fake: FakeOmlx, ollama: StubOllama | None = None, *, ceiling=48.0, reserve=10.0) -> mb.MemoryBudget:
    return mb.MemoryBudget(omlx_manager.OmlxManager(fake.base_url), ollama or StubOllama(), ceiling_gb=ceiling, reserve_gb=reserve)


# ------------------------------------------------------------------ oMLX manager


def test_manager() -> None:
    with FakeOmlx(pinned={"Qwen2.5-Coder-32B-Instruct-4bit"}) as fake:
        manager = omlx_manager.OmlxManager(fake.base_url)
        check(manager.healthy(), "health answers")
        status = manager.status()
        check(len(status.models) == 4 and not status.loaded, "every model starts inactive (0 loaded)")
        check(abs(status.ceiling_gb - 37.44) < 0.01, "the server's own pool ceiling is read")

        row = manager.resolve(PRISM_2)
        check(row.id == "Ternary-Bonsai-2-27B-mlx-2bit", "the registry's prism-ml/ org prefix resolves to the server's id")
        check(abs(row.size_gb - 8.41) < 0.01, f"estimated size is the spec's 8.41 GB ({row.size_gb:.2f})")

        loaded = manager.load(PRISM_2)
        check(loaded.loaded and manager.status().memory_gb > 8.0, "load makes the weights resident, verified against status")
        calls_before = fake.paths().count("POST /v1/models/Ternary-Bonsai-2-27B-mlx-2bit/load")
        manager.load(PRISM_2)
        check(fake.paths().count("POST /v1/models/Ternary-Bonsai-2-27B-mlx-2bit/load") == calls_before, "load is idempotent (no second request)")

        manager.load(BIG)
        dropped = manager.unload_all()
        check([m.id for m in dropped] == ["Ternary-Bonsai-2-27B-mlx-2bit"], "unload_all drops every unpinned model")
        check([m.id for m in manager.status().loaded] == [BIG], "…and leaves the pinned one resident")

        try:
            manager.resolve("nope/model")
        except omlx_manager.OmlxUnknownModel as error:
            check("Ternary-Bonsai" in str(error), "an unknown model names what the server does serve")
        else:
            raise AssertionError("FAIL: unknown model should raise")

    dead = omlx_manager.OmlxManager("http://127.0.0.1:9/v1")
    check(not dead.healthy(), "an unreachable server is unhealthy, not an exception")
    try:
        dead.status()
    except omlx_manager.OmlxUnreachable:
        check(True, "status raises OmlxUnreachable when nothing answers")

    os.environ["OMLX_PRISM_BASE_URL"] = "http://127.0.0.1:1/v1"
    try:
        check(omlx_manager.default_base_url() == "http://127.0.0.1:1/v1", "OMLX_PRISM_BASE_URL beats OMLX_BASE_URL")
    finally:
        del os.environ["OMLX_PRISM_BASE_URL"]


# ------------------------------------------------------------------- ollama cli


def test_cli() -> None:
    os.environ["ANTHROPIC_API_KEY"] = "sk-ant-should-not-leak"
    os.environ["OLLAMA_HOST"] = "127.0.0.1:11434"
    try:
        cli = StubOllama()
        cli.stop("qwen3:32b")
        check(cli.calls[-1] == ["/bin/echo", "stop", "qwen3:32b"], "argv, not a shell string")
        check("ANTHROPIC_API_KEY" not in cli.envs[-1] and "OLLAMA_HOST" in cli.envs[-1], "the child gets OLLAMA_* and no provider key")
        for bad in ("--help", "-rf", "a b", "x;rm -rf /", "$(id)", "", "../../etc/passwd"):
            try:
                cli.stop(bad)
            except ollama_cli.InvalidModelName:
                continue
            raise AssertionError(f"FAIL: {bad!r} should be rejected")
        check(True, "flag-shaped and shell-shaped model names are rejected")
        for good in ("qwen3:32b", "library/llama3:8b", "hf.co/org/model:Q4_K_M", "glm-5.3-flash:cloud"):
            ollama_cli.valid_model(good)
        check(True, "ordinary registry names pass")
        try:
            cli.run("serve")
        except ollama_cli.OllamaCliError:
            check(True, "only list/ps/pull/rm/stop can be run")
        result = cli.pull("qwen3:32b")
        check(result.stdout == "success", f"pull progress collapses to its final line ({result.stdout!r})")
    finally:
        del os.environ["ANTHROPIC_API_KEY"]
        del os.environ["OLLAMA_HOST"]


# ----------------------------------------------------------------------- budget


def test_budget() -> None:
    # 8.41 (Prism, recent) + 19.35 (Qwen 32B, old) resident; budget 38 → used 27.76, free 10.24.
    with FakeOmlx(loaded={BIG, "Ternary-Bonsai-2-27B-mlx-2bit"}) as fake:
        fake.loaded[BIG] = time.time() - 3600  # idle for an hour
        budget = budget_for(fake)
        snap = budget.snapshot()
        check(abs(snap.budget_gb - 38.0) < 1e-6 and abs(snap.used_gb - 27.76) < 0.02, f"budget = 48 − 10 reserve; used {snap.used_gb:.2f} GB")

        room = budget.ensure_headroom(5.0)
        check(room.ok and not room.freed, "enough room: nothing is stopped")

        room = budget.ensure_headroom(19.35)
        check(room.ok and [r.name for r in room.freed] == [BIG], f"needs 19.35 GB: the idle 32B goes first, the recent Prism stays ({room.summary()})")
        check(BIG not in fake.loaded and "Ternary-Bonsai-2-27B-mlx-2bit" in fake.loaded, "…and oMLX agrees")

    with FakeOmlx(loaded={BIG}) as fake:
        budget = budget_for(fake)
        room = budget.ensure_headroom(60.0)
        check(not room.ok and not room.freed and "exceeds the whole" in room.reason, "a request bigger than the whole budget evicts nothing")
        check(BIG in fake.loaded, "…the resident model is untouched")

    with FakeOmlx(loaded={BIG}, pinned={BIG}) as fake:
        room = budget_for(fake).ensure_headroom(30.0)
        check(not room.ok and BIG in fake.loaded and "pinned" in room.reason, "a pinned model is never evicted; the failure says so")

    # Ollama weights count against the same budget, and go through `ollama stop`.
    with FakeOmlx() as fake:
        cli = StubOllama([running("qwen3:32b", 19.0, 60), running("llama3:8b", 6.0, 3000)])
        budget = budget_for(fake, cli)
        room = budget.ensure_headroom(20.0)
        check(room.ok and [r.name for r in room.freed] == ["qwen3:32b"], f"the earliest-expiring (least recently used) Ollama model is stopped ({room.summary()})")
        check(cli.calls and cli.calls[0][1:] == ["stop", "qwen3:32b"], "…with `ollama stop <name>`")

    # oMLX's own pool is a second constraint: 30 GB loaded of a 37.44 pool leaves 7.4 < 8.41.
    with FakeOmlx(models={"a": 15.0, "b": 15.0, "Ternary-Bonsai-2-27B-mlx-2bit": 8.41}, loaded={"a", "b"}) as fake:
        fake.loaded["a"] = time.time() - 100
        budget = budget_for(fake, ceiling=60.0, reserve=5.0)  # global room is plentiful
        room = budget.ensure_headroom(8.41, runtime="omlx")
        check(room.ok and [r.name for r in room.freed] == ["a"], f"oMLX's pool ceiling is honoured even with global room ({room.summary()})")

    # Heavy task: every Prism model goes, then the budget is topped up.
    with FakeOmlx(loaded={"Ternary-Bonsai-2-27B-mlx-2bit", "Ternary-Bonsai-27B-mlx-2bit"}) as fake:
        budget = budget_for(fake)
        room = budget.release_for_heavy_task()
        check(not fake.loaded and len(room.freed) == 2 and room.ok, "release_for_heavy_task unloads every unpinned oMLX model")

    # load_omlx makes room first, then loads; and refuses when it cannot.
    with FakeOmlx(loaded={BIG, "Ternary-Bonsai-27B-mlx-2bit"}) as fake:
        fake.loaded[BIG] = time.time() - 500
        budget = budget_for(fake, ceiling=40.0, reserve=10.0)  # 30 GB budget; 27.65 used
        row, room = budget.load_omlx(PRISM_2)
        check(row.loaded and room is not None and BIG in [r.name for r in room.freed], "load_omlx stops an idle model to fit the new one")
        check(fake.events.index(f"unload {BIG}") < fake.events.index("load Ternary-Bonsai-2-27B-mlx-2bit"), "…and the unload happened BEFORE the load")

    with FakeOmlx(loaded={BIG}, pinned={BIG}) as fake:
        budget = budget_for(fake, ceiling=30.0, reserve=5.0)
        try:
            budget.load_omlx(PRISM_2)
        except mb.MemoryBudgetExceeded:
            check("load Ternary-Bonsai-2-27B-mlx-2bit" not in fake.events, "when it will not fit, load is never requested")
        else:
            raise AssertionError("FAIL: load should have been refused")

    # The emergency button.
    with FakeOmlx(loaded={"Ternary-Bonsai-2-27B-mlx-2bit", BIG}) as fake:
        cli = StubOllama([running("llama3:8b", 6.0, 100)])
        room = budget_for(fake, cli).unload_all()
        check(not fake.loaded and cli.calls[0][1:] == ["stop", "llama3:8b"] and room.ok, "unload_all stops oMLX and Ollama residents alike")


# ------------------------------------------------------------------------ tools


def tool_json(result: dict) -> dict:
    return next(block["json"] for block in result["content"] if "json" in block)


def test_tools() -> None:
    with FakeOmlx(loaded={BIG}) as fake:
        fake.loaded[BIG] = time.time() - 900
        cli = StubOllama([running("llama3:8b", 6.0, 100)])
        mb._DEFAULT = budget_for(fake, cli, ceiling=40.0, reserve=10.0)
        try:
            # --- omlx_resource_manager
            result = oxt.omlx_resource_manager.__wrapped__("status") if hasattr(oxt.omlx_resource_manager, "__wrapped__") else None
            direct = oxt.omlx_resource_manager._tool_func  # the undecorated function
            result = direct("status")
            check(result["status"] == "success" and "oMLX: 1 loaded" in result["content"][0]["text"], "omlx status is a ToolResult with text + json")
            result = direct("load", PRISM_2)
            check(result["status"] == "success" and "Ternary-Bonsai-2-27B-mlx-2bit" in fake.loaded, "load puts the Prism model in memory")
            check(BIG in tool_json(result)["freed"][0]["name"], "…after freeing the idle 32B to make room (30 GB budget)")
            result = direct("unload", PRISM_2)
            check(result["status"] == "success" and "Ternary-Bonsai-2-27B-mlx-2bit" not in fake.loaded, "unload takes it out again")
            result = direct("load", "nope")
            check(result["status"] == "error", "an unknown model is an error result, not an exception")
            result = direct("frobnicate")
            check(result["status"] == "error", "an unknown action is an error result")
            direct("load", PRISM_1)
            result = direct("unload_all")
            check(not fake.loaded or set(fake.loaded) == set(), "unload_all empties oMLX")

            # --- ollama_resource_manager
            ollama = ort.ollama_resource_manager._tool_func
            result = ollama("status")
            check(result["status"] == "success" and "free" in result["content"][0]["text"], "ollama status reports the budget")
            result = ollama("stop", "llama3:8b")
            check(result["status"] == "success" and cli.calls[-1][1:] == ["stop", "llama3:8b"], "stop runs `ollama stop`")
            result = ollama("ensure_headroom", needed_gb=5.0)
            check(result["status"] == "success", "ensure_headroom reports success when it fits")
            result = ollama("ensure_headroom", needed_gb=500.0)
            check(result["status"] == "error", "ensure_headroom reports failure when nothing could make it fit")
            result = ollama("stop", "--help")
            check(result["status"] == "error" and not any("--help" in call for call in cli.calls), "a flag-shaped name never reaches the CLI")
            result = ollama("list")
            check(result["status"] == "success" and cli.calls[-1][1] == "list", "list runs `ollama list`")

            check(ort.asks_for_approval({"action": "pull", "model": "x"}) and ort.asks_for_approval({"action": "RM"}), "pull and rm are the asking actions")
            check(all(ort.is_safe_call({"action": a}) for a in ("list", "ps", "status", "stop", "ensure_headroom")), "the rest never ask")
            check(not ort.is_safe_call({"action": "pull"}) and not ort.is_safe_call("x"), "a pull is not safe; a malformed call is not safe")
        finally:
            mb._DEFAULT = None


def main() -> None:
    test_manager()
    test_cli()
    test_budget()
    test_tools()
    print("\nmemory budget: all checks passed")


if __name__ == "__main__":
    main()
