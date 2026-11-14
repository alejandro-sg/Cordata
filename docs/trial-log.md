# Daily-driver trial log

Six weeks from 2026-11-13. Decide from this file, not from how the tool feels. At week 6 (2026-12-25): **continue**,
**narrow** (ship the repo map, guard and criteria runner as Claude Code hooks and MCP servers, keep the orchestrator in
maintenance, stop the app), or **stop building**.

## How to log

One line per non-trivial task (over ten minutes), added when you start it and finished when you stop:

```text
2026-11-14 | started in: cordata-app dock (claude) | finished in: same | why switched: -
2026-11-14 | started in: cordata run | finished in: bare claude | why switched: step 3 failed, faster by hand
```

Friction entries (something Cordata made harder than it should be) go under **Friction** with an id (`F1`, `F2`, …).
Spend overruns and lockouts go under **Incidents**.

## Metrics

| Metric | Measure | Target at week 6 | Kill or pivot signal |
|---|---|---|---|
| Driver share | tasks started in Cordata (including Claude Code in its dock terminal) / all non-trivial tasks | 60% or more | under 25% |
| Escape rate | tasks started in Cordata and finished in bare Claude Code or `agy` | 15% or less | over 40% |
| Verified-run success | last 20 real `cordata run`s with all criteria green and no manual fix | 70% or more | under 50% |
| Review time | median minutes from run done to merge (baseline in week 1) | 30% lower | no change |
| Surprise spend | cap overruns, lockouts over 24 hours | zero | any repeat |
| Cost per active day | ledger dollars per active day against your plan fee | a known number | above plan fee: keep the subscription lane default |
| Maintenance ratio | commits traced to a friction entry / all commits | 70% or more | under 40% |
| Policy health | warnings, bans or new ToS statements, checked monthly | none | any warning |

## Tasks

_Add lines here, newest last._

## Friction

| id | date | what was harder than it should be |
|---|---|---|

## Incidents

| date | what happened | cost |
|---|---|---|
