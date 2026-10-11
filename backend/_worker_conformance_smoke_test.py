"""Phase 1 of the orchestrator (docs/orchestrator-plan.md): the ``Worker`` interface and its three adapters.

Every adapter — ``ClaudeCodeWorker``, ``AgyWorker``, ``OllamaWorker`` — runs the same conformance checks
(``workers/conformance.py``) against a scripted stand-in for its provider: the fake ``claude`` / ``agy`` CLIs
(``fixtures/fake_cli_worker.py``, spawned for real through the scrubbed Seatbelt sandbox, with a local socket standing
in for the vault proxy) and a scenario-playing Strands model for Ollama. Streaming contract, structured output, the
file scope (kernel-enforced for the CLIs), budget cutoff, provider rate limits, crashes, cancel, timeout, resume, and
the secret boundary (vault egress, scrubbed environment, fenced and masked handoff data).

Plus the parts the fakes cannot show: the argv each CLI is given per role, the vault refusal, and the dialect parsers
fed the event streams recorded from the real CLIs on this machine.

    backend/.venv/bin/python _worker_conformance_smoke_test.py
"""

from __future__ import annotations

import asyncio
import json
import socket
import sys
import tempfile
from pathlib import Path

import provider_settings  # noqa: F401  (imported before anything that imports agent_common, which chdirs)

import scrubbed_sandbox
import vault_proxy
from harness_testing import check
from workers import TaskPacket, build_worker, collect, conformance, scope
from workers.agy_worker import AgyStream, AgyWorker
from workers.claude_worker import ClaudeCodeWorker, ClaudeStream


def listener() -> tuple[str, socket.socket]:
    server = socket.socket()
    server.bind(("127.0.0.1", 0))
    server.listen(16)
    return f"http://127.0.0.1:{server.getsockname()[1]}", server


def test_conformance(vault, addr: str) -> None:
    targets = [conformance.ollama_target()]
    if scrubbed_sandbox.seatbelt_works():
        targets = [conformance.cli_target("claude_code", lambda: vault, addr), conformance.cli_target("antigravity", lambda: vault, addr), *targets]
    else:
        # The CLI adapters refuse to run unconfined (and their scope is the kernel's); off macOS only Ollama's checks run.
        print("skip: Seatbelt unavailable here — the claude_code and antigravity adapters are checked on macOS")
    failures = 0
    for target in targets:
        print(f"\n=== conformance: {target.name} ===")
        for label, ok, detail in asyncio.run(conformance.run_all(target)):
            print(f"{'ok' if ok else 'FAIL'} [{target.name}] {label}" + ("" if ok else f" — {detail}"))
            failures += 0 if ok else 1
    check(failures == 0, f"every adapter passes every conformance check ({failures} failures)")


def test_argv() -> None:
    print("\n=== argv per role ===")
    claude = ClaudeCodeWorker(binary="/bin/echo")
    plan = TaskPacket(task_id="a", role="plan", prompt="p", cwd="/tmp", json_schema={"type": "object"}, budget_usd=0.5, model="sonnet")
    argv = claude.build_argv("claude", plan, None)
    check(argv[argv.index("--permission-mode") + 1] == "plan", "claude plan role runs in plan permission mode")
    check("Edit" not in argv[argv.index("--allowedTools") + 1].split(","), "claude plan role has no edit tool")
    check("--json-schema" in argv and "--max-budget-usd" in argv and argv[argv.index("--max-budget-usd") + 1] == "0.5", "claude enforces the schema and the dollar cap natively")
    check(argv[argv.index("--setting-sources") + 1] == "project" and "--disable-slash-commands" in argv and "--strict-mcp-config" in argv, "claude runs without the user's plugins, hooks, skills or MCP servers")
    execute = TaskPacket(task_id="b", role="execute", prompt="p", cwd="/tmp", file_scope=["src/a.py"])
    argv = claude.build_argv("claude", execute, "sess-9")
    check(argv[argv.index("--permission-mode") + 1] == "acceptEdits" and "Edit" in argv[argv.index("--allowedTools") + 1], "claude execute role may edit")
    check(argv[argv.index("--resume") + 1] == "sess-9", "claude resume passes the session id")

    agy = AgyWorker(binary="/bin/echo")
    argv = agy.build_argv("agy", plan, None)
    check("--mode" not in argv, "agy plan role runs in the default (review) mode — headless denies edits and commands")
    check(argv[-1] == "-p=" and "--input-format" in argv, "agy reads its prompt from stdin, never argv")
    argv = agy.build_argv("agy", TaskPacket(task_id="c", role="harden", prompt="p", cwd="/tmp", file_scope=["x"], model="claude-sonnet-5-5-high", effort="high"), "conv-1")
    check(argv[argv.index("--mode") + 1] == "accept-edits", "agy harden role accepts edits")
    check("--effort" not in argv, "agy does not repeat an effort the model id already carries")
    check(argv[argv.index("--conversation") + 1] == "conv-1", "agy resume passes the conversation id")
    stdin = json.loads(agy.build_stdin(plan, "hello"))
    check(stdin == {"event": "user", "message": {"content": "hello"}}, "agy stdin message shape (observed)")


def test_vault_refusal() -> None:
    print("\n=== vault ===")
    project = conformance.make_project(Path(tempfile.mkdtemp()))
    for label, status in (("off", vault_proxy.VaultStatus(enabled=False)), ("none", None), ("down", vault_proxy.VaultStatus(enabled=True, ok=False, addr="http://127.0.0.1:9", reason="the vault is unreachable"))):
        for family in ("claude_code", "antigravity"):
            worker = build_worker(family, binary="/bin/echo", vault=lambda s=status: s)
            _, result = asyncio.run(collect(worker.run(TaskPacket(task_id="v", role="plan", prompt="x", cwd=str(project)))))
            check(result.status == "failed" and "Refused" in (result.error_message or ""), f"{family} refuses to run with the vault {label}")


def test_recorded_dialects() -> None:
    """Event streams recorded from the real CLIs on this machine (trimmed), fed to the parsers."""
    print("\n=== recorded dialects ===")
    task = TaskPacket(task_id="r", role="plan", prompt="p", cwd="/tmp")
    agy_lines = [
        {"event": "init", "conversation_id": "bc7d", "init": {"model": "gemini-3.6-flash-low", "tools": ["view_file"], "permission_mode": "request-review"}},
        {"event": "step_update", "step_update": {"conversation_id": "bc7d", "step_index": 0, "state": "DONE", "step_type": "user_input"}},
        {"event": "step_update", "step_update": {"conversation_id": "bc7d", "step_index": 1, "state": "ACTIVE", "step_type": "agent_response", "text_delta": "PONG"}},
        {"event": "step_update", "step_update": {"conversation_id": "bc7d", "step_index": 1, "state": "DONE", "step_type": "agent_response", "text_delta": "\n", "usage": {"input_tokens": 15631, "output_tokens": 2, "thinking_tokens": 0, "cache_read_tokens": 0, "total_tokens": 15633}}},
        {"event": "result", "result": {"conversation_id": "bc7d", "status": "SUCCESS", "response": "PONG\n", "num_turns": 1, "usage": {"input_tokens": 15631, "output_tokens": 2, "thinking_tokens": 0, "cache_read_tokens": 0}}},
    ]
    parser = AgyStream(task)
    for line in agy_lines:
        parser.apply(line)
    check(parser.session_handle == "bc7d" and parser.model == "gemini-3.6-flash-low" and parser.ok and parser.text.strip() == "PONG", "agy: conversation, model, answer")
    check((parser.tokens_in, parser.tokens_out) == (15631, 2), "agy: tokens from step and result usage, not double-counted")
    denied = AgyStream(task)
    denied.apply({"event": "result", "result": {"status": "SUCCESS", "response": "", "denied_actions": [{"action": "command", "display_name": "RunCommand"}]}})
    check(denied.denied == ["command:RunCommand"], "agy: denied actions are reported")
    error = AgyStream(task)
    error.apply({"event": "result", "result": {"status": "ERROR", "error": "invalid model selection (--model \"x\")"}})
    check(error.ok is False and not error.rate_limited, "agy: an error result is a failure, not a rate limit")

    claude = ClaudeStream(task)
    for line in (
        {"type": "system", "subtype": "init", "session_id": "af9f", "model": "claude-haiku-5-5"},
        {"type": "assistant", "message": {"id": "m1", "content": [{"type": "text", "text": "PONG"}], "usage": {"input_tokens": 2, "output_tokens": 5, "cache_creation_input_tokens": 28974}}},
        {"type": "assistant", "message": {"id": "m1", "content": [{"type": "tool_use", "id": "tu1", "name": "StructuredOutput", "input": {"word": "PONG"}}], "usage": {"input_tokens": 2, "output_tokens": 5, "cache_creation_input_tokens": 28974}}},
        {"type": "result", "subtype": "error_max_budget_usd", "is_error": True, "total_cost_usd": 0.11595, "terminal_reason": "budget_exhausted", "errors": ["Reached maximum budget ($0.1)"], "session_id": "af9f",
         "modelUsage": {"claude-sonnet-5-5": {"inputTokens": 2, "outputTokens": 5, "cacheReadInputTokens": 0, "cacheCreationInputTokens": 28974, "costUSD": 0.11595}}},
    ):
        claude.apply(line)
    check(claude.structured == {"word": "PONG"}, "claude: the StructuredOutput tool call is the answer")
    check(claude.budget_exhausted and claude.ok is False and abs(claude.cost_usd - 0.11595) < 1e-9, "claude: a budget stop is recognised with its cost")
    check(claude.tokens_in == 28976 and claude.tokens_out == 5 and claude.requests == 1, "claude: one message's usage is counted once")
    window = ClaudeStream(task)
    events = window.apply({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "resetsAt": 1791688200, "rateLimitType": "five_hour", "overageStatus": "rejected",
                                                                            "unifiedWindows": {"five_hour": {"utilization": 0.46, "resetsAt": 1791688200}, "seven_day": {"utilization": 0.23, "resetsAt": 1791824400}}}})
    check(window.quota_info["windows"] == {"five_hour": {"utilization": 0.46, "resets_at": 1791688200}, "seven_day": {"utilization": 0.23, "resets_at": 1791824400}} and not window.rate_limited and events[0].event_type == "quota",
          "claude: the provider's own window utilisation is read (recorded live)")
    window.apply({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "resetsAt": 1791688200}})
    check(window.rate_limited, "claude: a rejected window is a rate limit")
    agy_struct = AgyStream(task)
    agy_struct.apply({"event": "result", "result": {"status": "SUCCESS", "response": "x", "structured_output": {"verdict": "accept", "items": [], "toolAction": "Finishing task", "toolSummary": "Finish task"}}})
    check(agy_struct.structured == {"verdict": "accept", "items": []}, "agy: the finish tool's envelope fields are dropped from the answer (recorded live)")


def test_scope_rules() -> None:
    print("\n=== scope ===")
    check(scope.matches("src/a.py", ["src/"]) and scope.matches("src/x/b.py", ["src/**"]) and not scope.matches("srcx/a.py", ["src/"]), "directory scopes")
    check(scope.matches("a.py", ["a.py"]) and scope.matches("a.py.tmp1", ["a.py"]) and not scope.matches("a.pyc/x", ["a.py"]), "file scopes allow the editor's temp sibling")
    check(scope.matches("pkg/m.py", ["pkg/*.py"]) and not scope.matches("pkg/m.txt", ["pkg/*.py"]), "glob scopes")
    check(scope.valid("/etc/passwd", "/tmp") and scope.valid("../x", "/tmp") and scope.valid(".env", "/tmp") and scope.valid("src/a.py", "/tmp") is None, "absolute, escaping and secrets entries are refused")
    profile = scope.scoped_profile("/tmp", ["src/a.py"])
    lines = profile.splitlines()
    deny_root = next(i for i, l in enumerate(lines) if l.startswith("(deny file-write* (subpath"))
    dotenv = next(i for i, l in enumerate(lines) if ".env" in l and l.startswith("(deny"))
    check(lines[1] == "(allow default)" and deny_root < dotenv, "the scope rules come before the secret denials (last match wins)")


def main() -> int:
    addr, server = listener()
    vault = vault_proxy.VaultStatus(enabled=True, ok=True, credentialed=False, addr=addr, reason="test vault")
    try:
        test_argv()
        test_scope_rules()
        test_recorded_dialects()
        test_vault_refusal()
        test_conformance(vault, addr)
    finally:
        server.close()
    print("\nworker conformance: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
