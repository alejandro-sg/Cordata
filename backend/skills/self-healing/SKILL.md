---
name: self-healing
description: When a tool call fails, diagnose and fix it through skill_forge instead of blindly retrying or giving up.
---

When a tool call you make fails (an error result, not just an unexpected answer), don't
just retry it as-is and don't just give up. Follow this sequence:

1. Call `think` first, on the actual error you got — not a guess. Consider whether the
   failure looks transient (a flaky network call, a timeout) or code-level (wrong args,
   a bug in how the tool works, a missing capability).

2. If it looks code-level, call `skill_forge` with the stack trace, the failing tool's
   name, and the arguments you attempted, all verbatim. Don't summarize or shorten the
   trace — `skill_forge` runs fresh sub-agents with no memory of this conversation, so
   whatever you don't pass in, they don't have.

3. `skill_forge` diagnoses and *drafts* a fix — it does not write anything to disk itself
   (a sub-agent has no way to pause for your approval, so it never gets a tool that would
   let it write ungated). Its result contains a `FILE: <path>` line, a `---` separator,
   and the full file content below that. Call `file_write` yourself, with exactly that
   path and that content — don't edit, shorten, or "clean up" what it drafted — as your
   very next tool call. That real `file_write` call is the one that actually pauses for
   human approval; nothing before it does.

4. Once your `file_write` call actually succeeds, the new tool registers itself on this
   conversation automatically — you don't need to wait for a restart, and you don't need
   to do anything else to make it callable. It just won't show up as an option in the
   very same model turn as the `file_write` call itself; call it on your next turn
   (right after, if that's what makes sense), not by trying to reference it before
   `file_write` has actually returned a result.

5. The first real call to a newly-written tool is gated a second time — a human has to
   approve it again, and it's dry-run checked, before it actually executes. If it gets
   rejected, you'll see a structured reason back instead of a result; read that reason
   and decide whether to call `skill_forge` again with the new information, or stop and
   explain the situation instead of retrying blindly.

6. Don't call `skill_forge` more than once or twice for the same original failure. If a
   second attempt also fails or gets rejected, stop and describe what happened instead of
   trying a third variant unsupervised — this is a case for a human to decide, not you.

This is different from `skill-authoring`: that skill is for writing yourself a markdown
guidance file by hand, when you've solved something the hard way. This skill is for
writing actual executable tool code, through `skill_forge`'s fixed debug -> write ->
validate pipeline, only in response to a real failure — never speculatively.
