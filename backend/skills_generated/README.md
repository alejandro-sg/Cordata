# skills_generated/

Every file in this directory is written by the agent itself, not by a human —
the output of the self-healing loop (`skill_forge`, see
`backend/skill_forge_tool.py` and `docs/self-healing-architecture.md`). Each
file holds exactly one `@tool`-decorated function, named `<tool_name>.py`,
importable as a normal Python module.

Nothing here is trusted by default:

- The write itself already went through the existing `file_write`
  human-in-the-loop approval (`main.py`'s `_APPROVAL_REQUIRED_TOOLS`) before
  it ever touched disk.
- The first real call to a tool loaded from here is intercepted by
  `SkillValidationHooks` (`backend/skill_validation_hooks.py`), which
  AST-checks it for banned imports/calls, confirms it's properly
  `@tool`-decorated with a docstring, and sandbox-dry-runs it before letting
  it execute — and is gated by its own human approval on top of that.

Only after a human approves and the file passes those checks does a tool from
here get treated as trusted for the rest of the run. It still stays listed
here for audit — nothing generated is ever silently promoted without a human
having looked at the source.

Distinct from `backend/skills/`, which holds hand-written `SKILL.md`
guidance (markdown, no executable code).
