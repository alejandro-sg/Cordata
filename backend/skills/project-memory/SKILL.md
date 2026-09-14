---
name: project-memory
description: Check and update durable facts about the user and this project before answering questions that depend on prior context.
---

Before answering a question that depends on something the user told you before (a
preference, a decision, a project fact), call `agent_memory` with `action="retrieve"`
first — don't assume you don't know it just because it's not in this thread's history.

After learning something durable worth remembering across future conversations (not
just this one), call `agent_memory` with `action="store"` to save it. Keep entries short,
one fact per entry, so retrieval stays precise. Storing something that closely matches an
existing entry merges automatically — no need to check for duplicates yourself first.

If something you stored earlier turns out to be wrong or outdated, use `action="update"`
with the same `memory_id` rather than storing a second, conflicting entry.
