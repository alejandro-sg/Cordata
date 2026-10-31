"""Grounded, three-stage research on the zero-download Ollama Cloud roster — before a plan touches code.

    backend/.venv/bin/python -m orchestrator.cloud_research --goal "…" --out ~/.cordata/orchestrator/<run>/ [paths…]

1. **Scout** (``minimax-m3.1-flash``, 1M context): the files under study are packed whole, with line numbers, into one
   prompt; the model returns a research brief whose every finding cites ``path:line`` and a verbatim quote.
2. **Ground + synthesise** (``research.verify`` then ``gemma4:31b``): citations that do not match the tree are dropped
   deterministically — no model decides what is true — and the survivors are turned into dependency-ordered plan steps
   with a ``file_scope`` and checkable acceptance criteria.
3. **Critique** (``mistral-large-4``, another vendor family): the synthesised plan is audited for failure modes the
   first two missed.

Everything goes through ``ollama_cloud.OllamaCloudModel`` (``EndpointGuard``: only ``POST /v1/chat/completions``), so
nothing is pulled. The three outputs and a combined ``research.md`` land in ``--out``.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from orchestrator import research

SCOUT, SYNTH, CRITIC = "minimax-m3.1-flash", "gemma4:31b", "mistral-large-4"
#: A pack larger than this is cut (whole files dropped from the end), so a stray glob cannot spend a fortune.
MAX_PACK_CHARS = 1_200_000

Ask = Callable[[str, str, str], str]  # (alias, effort, prompt) -> reply text


@dataclass
class Result:
    brief: dict[str, Any]
    dropped: list[str]
    plan: dict[str, Any]
    critique: str
    timings: dict[str, float] = field(default_factory=dict)


def pack(root: Path, paths: list[str]) -> str:
    """Each file once, every line numbered (``  42| code``) so a citation can be checked against what was shown."""
    parts: list[str] = []
    total = 0
    for rel in paths:
        file = (root / rel).resolve()
        if not file.is_file() or root.resolve() not in file.parents:
            continue
        lines = file.read_text(encoding="utf-8", errors="replace").splitlines()
        body = "\n".join(f"{n:5}| {line}" for n, line in enumerate(lines, 1))
        chunk = f"=== FILE {file.relative_to(root.resolve())} ({len(lines)} lines) ===\n{body}\n"
        if total + len(chunk) > MAX_PACK_CHARS:
            break
        parts.append(chunk)
        total += len(chunk)
    return "\n".join(parts)


def extract_json(text: str) -> dict[str, Any]:
    """The first JSON object in a reply (fenced or bare); ``{}`` when there is none."""
    fenced = re.search(r"```(?:json)?\s*(\{.*?\})\s*```", text, re.S)
    candidates = [fenced.group(1)] if fenced else []
    start, end = text.find("{"), text.rfind("}")
    if start != -1 and end > start:
        candidates.append(text[start : end + 1])
    for candidate in candidates:
        try:
            value = json.loads(candidate)
            if isinstance(value, dict):
                return value
        except json.JSONDecodeError:
            continue
    return {}


def scout_prompt(goal: str, packed: str) -> str:
    return (
        f"{research.RESEARCH_BRIEF}\n\nGoal: {goal}\n\n"
        "Map every integration seam the goal touches. For each finding cite evidence of kind 'file' with ref "
        "'relative/path:LINE' (the numbers shown at the left of each line below) and a quote copied verbatim from that "
        "line (without the number prefix). 12–30 findings. Reply with ONE JSON object matching this schema, nothing else:\n"
        f"{json.dumps(research.RESEARCH_SCHEMA['properties'])}\n\n{packed}"
    )


def synth_prompt(goal: str, brief_md: str) -> str:
    return (
        f"Goal: {goal}\n\nVerified research brief (every citation below was checked against the tree):\n{brief_md}\n\n"
        "Synthesize a dependency-ordered implementation plan. Reply with ONE JSON object: "
        '{"steps": [{"id": "S1", "title": str, "depends_on": [ids], "file_scope": [paths], '
        '"acceptance_criteria": [deterministic checks: a test name, a command and its expected exit, a grep]}], '
        '"risks": [str]}. Use only paths that appear in the brief or new files next to them.'
    )


def critic_prompt(goal: str, plan: dict[str, Any], brief_md: str) -> str:
    return (
        f"Goal: {goal}\n\nYou are a principal engineer from a different team auditing this plan before any code is "
        "written. Be concrete and terse. For each risk: the failure scenario, which step it hits, and the change to the "
        "plan that prevents it. Look specifically for: copy-on-write / merge conflicts between parallel steps, bandit "
        "cold-start variance, UI re-render storms on theme change, MCP/plugin trust boundaries, scope escapes, and "
        "acceptance criteria a model could game.\n\n"
        f"Plan:\n{json.dumps(plan, indent=1)}\n\nBrief:\n{brief_md}"
    )


def default_ask(alias: str, effort: str, prompt: str) -> str:
    from strands import Agent

    import ollama_cloud

    agent = Agent(model=ollama_cloud.OllamaCloudModel(alias, reasoning_effort=effort, params={"max_tokens": 32768}), callback_handler=None)
    return str(agent(prompt))


def run(goal: str, root: Path, paths: list[str], ask: Ask = default_ask, log: Callable[[str], None] = print) -> Result:
    timings: dict[str, float] = {}
    packed = pack(root, paths)
    log(f"stage 1 · {SCOUT}: {len(packed):,} chars packed from {len(paths)} paths")
    started = time.monotonic()
    brief = extract_json(ask(SCOUT, "high", scout_prompt(goal, packed)))
    timings["scout"] = time.monotonic() - started
    verified = research.verify(brief, root)
    survivors = verified.brief.get("findings", []) if isinstance(verified.brief, dict) else []
    log(f"stage 2 · verify: {len(survivors)} findings kept, {len(verified.dropped)} dropped{' (REFUSED: ' + '; '.join(verified.reasons) + ')' if verified.refused else ''}")
    brief_md = research.render(verified.brief)
    started = time.monotonic()
    plan = extract_json(ask(SYNTH, "high", synth_prompt(goal, brief_md)))
    timings["synthesize"] = time.monotonic() - started
    log(f"stage 2 · {SYNTH}: {len(plan.get('steps', []))} plan steps")
    started = time.monotonic()
    critique = ask(CRITIC, "high", critic_prompt(goal, plan, brief_md))
    timings["critique"] = time.monotonic() - started
    log(f"stage 3 · {CRITIC}: {len(critique):,} chars of critique")
    return Result(verified.brief, list(verified.dropped), plan, critique, timings)


def write(result: Result, goal: str, out: Path) -> Path:
    out.mkdir(parents=True, exist_ok=True)
    (out / "brief.json").write_text(json.dumps(result.brief, indent=1))
    (out / "plan.json").write_text(json.dumps(result.plan, indent=1))
    (out / "critique.md").write_text(result.critique)
    steps = "\n".join(
        f"- **{s.get('id')}** {s.get('title')} — after {', '.join(s.get('depends_on') or []) or 'nothing'}; scope "
        f"`{', '.join(s.get('file_scope') or [])}`; accept: {'; '.join(s.get('acceptance_criteria') or [])}"
        for s in result.plan.get("steps", [])
    )
    doc = (
        f"# Grounded research\n\nGoal: {goal}\n\nModels: scout `{SCOUT}`, synthesis `{SYNTH}`, critique `{CRITIC}` "
        f"(all `:cloud`, zero download). Timings: {', '.join(f'{k} {v:.0f}s' for k, v in result.timings.items())}.\n\n"
        f"## Stage 1–2: verified brief ({len(result.brief.get('findings', []))} findings kept, {len(result.dropped)} dropped)\n\n"
        f"{research.render(result.brief)}\n\nDropped (citation did not match the tree): {', '.join(result.dropped) or 'none'}\n\n"
        f"## Stage 2: plan\n\n{steps or '(no steps returned)'}\n\n## Stage 3: cross-family critique\n\n{result.critique}\n"
    )
    path = out / "research.md"
    path.write_text(doc)
    return path


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--goal", required=True)
    parser.add_argument("--root", default=".")
    parser.add_argument("--out", required=True)
    parser.add_argument("paths", nargs="+")
    args = parser.parse_args(argv)
    root = Path(args.root).expanduser().resolve()
    result = run(args.goal, root, args.paths)
    path = write(result, args.goal, Path(args.out).expanduser())
    print(path)
    return 0 if result.plan.get("steps") else 1


if __name__ == "__main__":
    sys.exit(main())
