---
name: build
description: |
  Plan-then-execute implementation against SPEC.md. Native single-thread
  loop, no sub-agents. On test or build failure, auto-invokes the backprop
  skill before retrying — a failed verification always considers whether
  a new §V invariant would prevent recurrence. Triggers when the user asks
  to build, implement, execute the spec, or tackle a specific §T task
  (`build §T.3`, `build --next`, `implement next task`, `run the build`).
  Expects SPEC.md to exist; if not, defers to the spec skill.
---

# build — implement spec

Single-thread native plan→execute. You are main Claude. No swarm.

## LOAD

1. Read `SPEC.md`. If missing → tell user to invoke the spec skill first. Stop.
2. Read `FORMAT.md` once if not loaded.
3. Read §R if present — external facts the build must honor, ⊥ re-derive or contradict.
4. Parse invocation args:
   - `§T.n` → that task only
   - `--next` → lowest-numbered row with status `.` or `~`
   - `--all` or empty → every `.` row in §T order

High blast radius (shared module, auth, data, money, public §I)? Run `/review` first. Trivial & reversible? Skip planning ceremony, just do step EXECUTE.

## PLAN & EXECUTION CONTRACT

Instead of open-ended planning, structure the proposal strictly across three parts:

1. **Describe the task(s)**:
   - Specific tasks broken down in execution order.
   - Files to create or edit.
   - Cite every §I interface touched; must preserve interface shape.
2. **Describe the Guardrails that surround the task(s)**:
   - Cite every §V invariant that applies; plan must respect all without exception.
   - Non-goals and blast radius bounds (shared modules, auth, credentials, fail-closed rules).
   - Invariants that MUST NOT be regressed or modified.
3. **Describe what Done looks like**:
   - **Verification contract**: name the EXACT test(s) / acceptance criteria that prove each §V touched.
   - Name the verification command (test, build, lint) — this is the external oracle. Green = done; ⊥ "looks done".

Present contract via `present_plan`. Wait for user OK unless auto mode.

## EXECUTE

Per task in order:

1. Flip §T.n status cell `.` → `~`. Just write to SPEC.md.
2. Edit code per plan.
3. Run verification command.
4. **Pass** → flip `~` → `x`. Next task.
5. **Fail** → invoke backprop skill. Do NOT retry blindly.

## FAIL → BACKPROP

On test/build failure:

1. Read failure output.
2. Ask: is failure (a) my code bug, (b) spec wrong, or (c) unspecified edge case?
3. If (a) → fix code, re-run. No spec change.
4. If (b) or (c) → invoke spec skill with `bug: <cause>` first, let it update §V and §B, then resume build against updated spec.

Rule: never silently fix root-cause without considering backprop. §B is the memory that stops recurrence.

## WRITE POLICY

- Only flip §T status. No other SPEC.md edits from build.
- Other spec edits → invoke spec skill.
- Commit after each §T completes. Message: `T<n>: <goal line>` + §V cites.

## VERIFICATION

Task `x` only if:
- Verification command (the oracle) exits 0.
- Every §V touched has its named test from the verification contract, and it passes.
- No §V invariant regressed (run full test suite at end).

## NON-GOALS

- No sub-agents. No parallel workers. Main thread only.
- No progress dashboards. `cat SPEC.md | grep §T` is the dashboard.
- No speculative work beyond chosen task scope.
