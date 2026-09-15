---
name: skill-authoring
description: Write a new skill for yourself when you find a reusable approach worth remembering across future conversations, not just this one.
---

You can create new skills for yourself. A skill is a directory under `skills/` (relative to
this project's `backend/`) containing one `SKILL.md` file:

```
skills/<skill-name>/SKILL.md
```

Format, exactly:

```markdown
---
name: <skill-name>
description: <one sentence — when this skill applies>
---

<the actual instructions — as long as needed, this only loads when you activate it>
```

Constraints on `<skill-name>`: lowercase letters, digits, and hyphens only, must start and
end with a letter or digit (e.g. `pdf-table-extraction`, not `PDF_Table_Extraction`).

Use `file_write` to create it (a human must approve this, like any file write). A new
skill takes effect starting the *next* conversation — it won't appear mid-thread, because
skills are discovered once when a session starts.

**When to write one.** Only after you've actually solved something the hard way and would
otherwise redo the same reasoning next time: a multi-step procedure for this project's
tools, a gotcha you had to work around, a pattern that generalizes. Don't write a skill for
a one-off task, and don't write one speculatively for something you haven't actually done
yet — that's how `sub-agent-team`'s `local_swarm` guidance came to exist: after the plain
`swarm` tool's config requirement kept getting narrated instead of called, the fix (a
simpler wrapper tool) got written up as a skill so the lesson isn't rediscovered every time.

**Before writing, check two things explicitly** (both cheap — the first uses context you
already have, the second is just honest self-assessment):

1. **Does something close already exist?** Your own system prompt already carries the full
   `<available_skills>` list (name + one-line description each) — skim it for a name or
   description covering the same ground before assuming this is new. If one's close,
   *extend that skill* (another `file_write` to its existing `SKILL.md`) instead of adding
   a second, near-duplicate skill nobody will reliably pick between — two skills half-
   covering the same thing is worse than one skill that's slightly broader than its name
   suggests.
2. **Would this genuinely help a future, memory-less instance of you, reading it cold, on a
   real recurrence** — not "this seems generally useful," but "I actually hit this, solved
   it the hard way once already, and it'll come up again"? If you're unsure, it's not ready
   yet — wait for a second real occurrence rather than writing it from a single data point.

Neither check is a hard gate enforced by code — `file_write`'s own human approval is the
real review step (the human sees the full content and can decline it), which is *earlier*
than it sounds like it needs to be: you're asking a human to read and approve unproven,
self-authored guidance before it's ever added, not after it's already influencing behavior.
Treat that approval prompt as the moment to actually make the case for 1 and 2 above, not a
formality to get past — a human skimming a padded justification is worse than one told
plainly "not fully sure this recurs, flagging for your judgment."

This is real self-improvement in a narrow, honest sense — you can turn a hard-won pattern
into something your future self reads automatically. It is not closed-loop self-healing:
you cannot detect that you're wrong, diagnose why, patch your own tool code, and verify the
fix without a human in that loop. Writing tool/backend code (not just skill markdown) is a
larger, riskier change — propose it in conversation and let the human make the edit, don't
attempt to rewrite your own Python.
