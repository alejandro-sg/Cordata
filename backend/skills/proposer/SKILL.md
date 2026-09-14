---
name: proposer
description: Propose a new tool or a harness-file change proactively — when you notice a recurring pattern or a human asks for an improvement, not only after a tool call fails.
---

Use `proposer` when you notice something worth changing that isn't a single failed tool
call — a pattern that keeps costing extra turns, or a specific improvement a human asked
for. For one failed tool call, use `skill_forge` instead (see the `self-healing` skill) —
`proposer` is for proactive, not reactive, changes.

Call `proposer` with `observation` describing exactly what prompted this: what you
noticed, or what was asked, concretely. It runs a fixed sequence — analyst, writer, a
static guard, a regression-test gate against a sandboxed copy of `backend/`, and a critic
— and only returns a draft if all three checks after the writer passed. If any stage
refuses, it tells you why and nothing is drafted; don't retry the same observation more
than once or twice — if it keeps failing, stop and explain the situation to the human
instead of guessing at variants.

`proposer` never writes anything to disk itself — the same reason `skill_forge` doesn't
(see the `self-healing` skill): a sub-agent has no way to pause for approval, so drafting
is the only safe thing it can do. If it returns a draft, it looks like:

```
FILE: <path>
---
<full file content>
```

Read the draft yourself before acting on it. If you agree with it, call `file_write` (or
`editor` for an existing file) yourself, with exactly that path and exactly that content
— don't summarize, shorten, or "improve" what was drafted — as your very next tool call.
That real `file_write`/`editor` call is what actually pauses for human approval; nothing
before it does, including a passing critic.

Two files can never be a target, and `proposer` refuses in Python before ever drafting
anything against them: `skill_validation_hooks.py` and `proposer_tool.py` itself. If you
think one of those genuinely needs to change, say so to a human directly — don't try to
route it through `proposer`, it will refuse.

Don't call `proposer` more than once or twice for the same underlying observation. If a
second attempt also gets refused, that's a case for a human to decide, not you.
