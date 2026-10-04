"""The observer's agent, in a process of its own.

Started by `daemon/observer.py` — never imported by the daemon itself. Two
reasons, and they are both structural rather than stylistic:

* **`agent_common` chdirs at import.** `supervisor.py`'s docstring states the
  invariant this file exists to keep: the daemon must never import
  `agent_common`, because it moves the whole process's working directory and the
  daemon serves several projects. The runner is a child, so the chdir is
  contained in it — and it is spawned with `cwd=<the project being observed>`,
  which is exactly what makes `file_read` resolve relative paths against the
  right project.
* **Zero-write has to be structural.** An observer that is *asked* not to write
  is one prompt injection away from writing. This one cannot: the agent is built
  with [`OBSERVER_TOOLS`][] and nothing else, the allowlist is asserted disjoint
  from `sub_agent_common`'s denylists at import (so adding `shell` here fails
  loudly rather than quietly widening the toolset), a hook cancels any call
  outside the allowlist even if a tool were hot-loaded into the registry later,
  and the briefing file is written by the *parent*, in daemon code, from the
  text this process prints. There is no code path from here to the filesystem.

Protocol: one JSON request on stdin, one JSON response on stdout, nothing else
on stdout (logs go to stderr — the parent parses stdout as data).

    {"project": "...", "evidence": {...}, "instruction": "..."}
    {"summary": "...", "proposals": [{"title": "...", "why": "...", "next": "..."}]}
"""

from __future__ import annotations

import json
import logging
import os
import sys
from typing import Any

# `sub_agent_common` imports lazily, so this is safe before any chdir:
# the denylists below are the reason the allowlist here is trustworthy.
import sub_agent_common

logger = logging.getLogger("sierralab.observer")

#: What the observer may hold. `file_read` to look at a file the evidence
#: mentioned; `think` to plan. No `shell` — the parent already ran the exact
#: read-only commands (a fixed `git` argv list) and hands the output over as
#: evidence, so the model never chooses a command. No `http_request`: an observer
#: that can fetch is an observer that can be told what to fetch.
OBSERVER_TOOLS = ("file_read", "think")

# The invariant that makes the line above a guarantee: if someone adds `shell`
# to OBSERVER_TOOLS, this fails at import in the child (and in the smoke test)
# instead of silently handing an autonomous loop write access.
_CLASH = set(OBSERVER_TOOLS) & (sub_agent_common.DENIED_TOOLS | sub_agent_common.ORCHESTRATOR_ONLY_TOOLS)
assert not _CLASH, f"OBSERVER_TOOLS overlaps a denied toolset: {sorted(_CLASH)}"

OBSERVER_PROMPT = (
    "You are a background observer for a software project. You are shown "
    "evidence collected from the project's git history and working tree, and "
    "you may call file_read to look at a specific file in it. You have no other "
    "tools: you cannot run commands, fetch anything, or change a single file — "
    "do not claim otherwise, and do not ask to.\n\n"
    "Your job is to say what a person coming back to this project should know, "
    "and what is worth doing next. Be concrete and short. Say nothing about a "
    "change you cannot see in the evidence; if the evidence is thin, say so "
    "instead of padding it.\n\n"
    "Answer with one JSON object and nothing else:\n"
    '{"summary": "4-8 sentences a person can act on", '
    '"proposals": [{"title": "one line", "why": "the evidence for it", '
    '"next": "the first concrete step"}]}\n'
    "Proposals are suggestions for the user to accept or ignore — you are not "
    "starting work, and nothing you write is applied."
)


def _tool_objects(names) -> list:
    from strands_tools import file_read, think

    available = {"file_read": file_read, "think": think}
    return [available[name] for name in names]


class _AllowlistOnly:
    """Cancels any tool call outside the allowlist.

    Belt and braces over building the agent with the allowlist in the first
    place: a tool that reached the registry another way (a plugin, a hot-load)
    still cannot run, because this checks the call rather than the roster.
    """

    def __init__(self, allowed) -> None:
        self.allowed = set(allowed)

    def register_hooks(self, registry) -> None:
        from strands.hooks import BeforeToolCallEvent

        registry.add_callback(BeforeToolCallEvent, self._check)

    def _check(self, event) -> None:
        name = event.tool_use.get("name")
        if name not in self.allowed:
            event.cancel_tool = f"Blocked: the observer holds only {sorted(self.allowed)}; it may not call {name}."


def build_observer_agent(*, model=None, tools: tuple[str, ...] = OBSERVER_TOOLS):
    """The read-only observer agent. `model` is injectable for the smoke test."""
    from strands import Agent

    from prompt_security import SecretRedactionHooks, UntrustedToolResultHooks

    if model is None:
        # Lazy: importing agent_common (inside this call) does the chdir the
        # module docstring describes, and it must happen in the child only.
        import sub_agent_common as sac

        model = sac.build_sub_agent_model(os.environ.get("SIERRALAB_OBSERVER_TIER") or None)
    return Agent(
        name="observer",
        model=model,
        system_prompt=OBSERVER_PROMPT,
        tools=_tool_objects(tools),
        hooks=[_AllowlistOnly(tools), UntrustedToolResultHooks(), SecretRedactionHooks()],
        callback_handler=None,
    )


def parse_answer(text: str) -> dict[str, Any]:
    """The model's JSON, or as much of it as can be trusted.

    Never raises: a cycle that produced prose instead of JSON still has a
    summary worth showing, and a formatting slip must not cost the whole
    observation. A `proposals` entry that is not an object with a title is
    dropped rather than rendered as a blank bullet.
    """
    payload: dict[str, Any] = {}
    start = text.find("{")
    end = text.rfind("}")
    if start >= 0 and end > start:
        try:
            candidate = json.loads(text[start : end + 1])
            if isinstance(candidate, dict):
                payload = candidate
        except ValueError:
            payload = {}
    summary = payload.get("summary")
    if not isinstance(summary, str) or not summary.strip():
        summary = text.strip()
    proposals = []
    for item in payload.get("proposals") or []:
        if not isinstance(item, dict):
            continue
        title = str(item.get("title") or "").strip()
        if not title:
            continue
        proposals.append(
            {
                "title": title[:200],
                "why": str(item.get("why") or "").strip()[:1000],
                "next": str(item.get("next") or "").strip()[:1000],
            }
        )
    return {"summary": summary, "proposals": proposals}


def observe(request: dict[str, Any], *, model=None) -> dict[str, Any]:
    """One observation: build the agent, hand it the evidence, parse the answer."""
    evidence = request.get("evidence") or {}
    project = request.get("project") or evidence.get("project") or "(unknown project)"
    agent = build_observer_agent(model=model)
    prompt = (
        f"Project: {project}\n\n"
        f"Evidence (collected by the daemon; nothing here was written by a model):\n"
        f"{json.dumps(evidence, indent=1, ensure_ascii=False)}\n\n"
        + (f"Also noted: {request['instruction']}\n\n" if request.get("instruction") else "")
        + "Answer with the JSON object described in your instructions."
    )
    result = agent(prompt)
    text = str(result)
    answer = parse_answer(text)
    answer["project"] = project
    return answer


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(level=os.environ.get("SIERRALAB_OBSERVER_LOG", "warning").upper(), stream=sys.stderr)
    raw = sys.stdin.readline()
    if not raw.strip():
        print(json.dumps({"error": "no request on stdin"}))
        return 2
    try:
        request = json.loads(raw)
    except ValueError as error:
        print(json.dumps({"error": f"unreadable request: {error}"}))
        return 2
    try:
        answer = observe(request)
    except Exception as error:  # a failed cycle is a quiet one, never a crash loop
        logger.exception("observer runner failed")
        print(json.dumps({"error": f"{type(error).__name__}: {error}"}))
        return 1
    print(json.dumps(answer, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
