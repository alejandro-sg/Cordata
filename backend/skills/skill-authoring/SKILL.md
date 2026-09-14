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

This is real self-improvement in a narrow, honest sense — you can turn a hard-won pattern
into something your future self reads automatically. It is not closed-loop self-healing:
you cannot detect that you're wrong, diagnose why, patch your own tool code, and verify the
fix without a human in that loop. Writing tool/backend code (not just skill markdown) is a
larger, riskier change — propose it in conversation and let the human make the edit, don't
attempt to rewrite your own Python.
