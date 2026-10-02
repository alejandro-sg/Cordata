---
name: sub-agent-team
description: Spin up a team of specialized Strands sub-agents for a task that genuinely benefits from multiple roles (e.g. research + writing + review, or independent parallel research).
---

Two tools, not `swarm` directly — both already point every sub-agent at the same
model provider you run on, so you never write `model_provider` or `model_settings`
yourself, and you should not try to. A spec may add `"tier": "fast" | "standard" |
"strong"` to pick a smaller or larger model for that specialist; leave it out unless the
sub-task is clearly trivial or unusually hard — a default is chosen for you. Default to `local_graph` when you're breaking a
task into distinct roles: it runs every role with no unmet dependency at the same time
(genuine concurrency, not a queue), so it finishes faster than `local_swarm` whenever
the roles don't actually need each other's output first. Only reach for `local_swarm`
when a step truly can't start before another one finishes.

- **`local_swarm`** — sub-agents hand off to each other one at a time, in sequence.
  Use when each step needs the previous one's output (research, then write, then
  review).
- **`local_graph`** — sub-agents with no unmet dependency run *concurrently*. Use when
  the sub-tasks are genuinely independent (e.g. researching three unrelated things at
  once) — `local_swarm` would run them one at a time for no reason.

Each agent spec is `name`, `system_prompt`, and optionally `tools` (a list of names from
your own toolset, e.g. `["http_request"]`, to restrict that specialist to exactly those).
Omit `tools` entirely and the specialist gets every generically-useful tool automatically
(currently: `http_request`, `file_read`, `calculator`, `current_time`, `think`,
`agent_memory`) — leave it out unless you actually need to restrict what this one
specialist can reach for. Pass `"tools": []` explicitly (not the same as omitting it) to
give a specialist none at all.

`shell`, `python_repl`, `file_write`, `editor`, and any generated skill are never
available to sub-agents in either tool, however you list them (or even if you omit
`tools` — they're excluded from the default set too) — both silently drop those names
before building the team, because sub-agents have no human-in-the-loop gate the way you
do. `local_swarm`, `local_graph`, and `skill_forge` are also excluded even from you
listing them explicitly — those are yours alone; a sub-agent recursively spinning up its
own team has no depth limit. If the task genuinely needs one of those, do that step
yourself after the team returns, instead of asking a sub-agent to do it.

`local_swarm` auto-injects a `handoff_to_agent` coordination tool into every sub-agent, so
they can hand off to each other autonomously — but only if each `system_prompt` actually
tells them when to use it. There is no `complete_swarm_task` tool — a sub-agent finishes
the swarm simply by responding with its final answer instead of handing off again; tell
the *last* agent in the chain to do exactly that. Per Strands' own multi-agent guidance, a
good sub-agent system prompt covers three things, not just the first:

1. **Specialty** — what this agent is for.
2. **Handoff condition** — the specific situation where it should hand off, and to whom.
3. **Tools** — what it has access to and is expected to use.

A prompt with only (1) leaves the swarm unable to coordinate — it can produce isolated
output per agent but won't hand off work in sequence.

Example — research, then write, then review:

```
local_swarm(
    task="<the overall goal>",
    agents=[
        {
            "name": "researcher",
            "system_prompt": (
                "You are a research specialist. Gather and verify facts relevant to the "
                "task using your tools; cite where each fact came from. Once you have "
                "enough to write from, hand off to 'writer' with your findings — don't "
                "attempt the writing yourself."
            ),
            "tools": ["http_request"],
        },
        {
            "name": "writer",
            "system_prompt": (
                "You are a writing specialist. Turn the researcher's findings into clear, "
                "concise prose. Once you have a complete draft, hand off to 'reviewer' for "
                "a pass before it's considered final — don't skip that step."
            ),
        },
        {
            "name": "reviewer",
            "system_prompt": (
                "You are a review specialist. Check the draft for accuracy against the "
                "researcher's cited facts and for clarity. If it's solid, respond with the "
                "final text as your answer — that ends the swarm. If it needs work, hand "
                "off back to 'writer' with specific, actionable feedback — not a rewrite "
                "yourself."
            ),
        },
    ],
)
```

Example — three independent research tasks in parallel, `local_graph`:

```
local_graph(
    task="<the overall goal>",
    agents=[
        {
            "name": "bitcoin_researcher",
            "system_prompt": "Research current Bitcoin price and trends. Respond with a short summary.",
            "tools": ["http_request"],
        },
        {
            "name": "ethereum_researcher",
            "system_prompt": "Research current Ethereum price and trends. Respond with a short summary.",
            "tools": ["http_request"],
        },
        {
            "name": "solana_researcher",
            "system_prompt": "Research current Solana price and trends. Respond with a short summary.",
            "tools": ["http_request"],
        },
    ],
)
```

None of the three has a `depends_on`, so all three run at the same time. Add
`"depends_on": ["bitcoin_researcher"]` to a later agent's spec to make it wait for that
one specifically — dependencies can be mixed with independence in the same call.

Only reach for either tool when the task actually has distinct roles that benefit from
separate context — a single agent with the right tools is simpler and faster for
anything that doesn't.
