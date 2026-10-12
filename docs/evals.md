# Outcomes and evals — the harness's reward signal

Nothing in the harness can improve what it cannot measure. Two pieces measure it:

* **Outcome records** (`backend/outcomes.py`, `outcome_hooks.py`) — what each real turn did and how it turned out.
* **The eval suite** (`backend/evals/`) — 42 tasks with verifiers, so a change to the harness is a number, not a feeling.

The eval suite is the check every learned change has to pass before it counts: a reflected lesson, a candidate
skill or a prompt overlay is promoted only if it raises the score here without breaking the held-out third
(`backend/promote.py`, `docs/agentic-harness.md`), and the contextual bandit's reward is built from the same
`outcomes.score` parts. The signal is what `reflect.py`, `promote.py`, `prompts.py`, `optimize.py` and
`bandit.py` are allowed to learn from — and what stops them learning the wrong thing.

A run is measured against an **arm**: `run_suite(tasks, driver, config={...})` puts the arm in force for the
whole run — a candidate skill directory (`CORDATA_EXTRA_SKILLS`) or a candidate prompt overlay
(`CORDATA_PROMPT_OVERLAY`), applied by `sandbox` and removed on exit, with the arm's name recorded on every
`TaskResult` so a saved run says which arm produced it. Without it the gate would compare a candidate against
itself.

## Outcome records

Every ACP turn writes one `outcome` event to the `state_store` ledger when it ends:

| field | what |
|---|---|
| `goal` | the prompt, secrets masked |
| `mode`, `route` | build / plan / auto, and the route Laya chose (target, confidence, abstained) |
| `tools` | per tool: count, failed, **denied** (a refused call is not a failure) |
| `approvals` | asked / approved / rejected / auto — what auto mode let through |
| `files` | files changed, with before/after hashes |
| `verifier` | project checks run (`pytest`, `cargo test`, `tsc`, …), and whether the **last one after the last edit** passed |
| `stop_reason`, `seconds`, `tokens` | cancelled, truncated, cost |

A later pass (`outcomes.reconcile`, run at the start of the next turn on that project) writes `outcome_update`
events: of the files a turn changed, which were **kept**, **reverted**, **edited** by the person afterwards, or
**removed**. A change the person undoes is the strongest negative signal there is, and it arrives for free.

`outcomes.score(record)` collapses a record to a reward in [−1, 1]:

| part | weight |
|---|---|
| verifier passed after the last edit | +0.45 |
| verifier failing after the last edit | −0.45 |
| file kept / reverted | +0.25 / −0.40 |
| each rejected approval (cap 3) | −0.10 |
| cancelled / truncated | −0.20 each |
| went in circles (a repeated step was refused, or an edit undid itself; a loop note alone: half) | −0.15 |
| tokens, time | up to −0.05 each |

The weights are deliberately blunt: a verifier outranks everything because it is the only part nobody argues with.
`python -m outcomes report` summarises recent turns and their scores; `python -m outcomes reconcile` judges the files they left behind.

## The eval suite

```
make evals-validate     # every task fails before the work and passes after its oracle
make evals              # the whole suite through the real ACP server, oracle driver
make evals ARGS="--driver replay --suite heldout"
cd backend && PYTHONPATH=. .venv/bin/python -m evals list
```

* **42 tasks, 8 categories** — bug fixes, features, refactors, config, answer-only, mutation-graded test writing,
  and **attacks**. Each is data: starting files, a prompt, a verifier, an oracle solution.
* **A task proves itself.** `validate` runs the verifier on the untouched repo (must fail) and after the oracle's
  solution (must pass). A verifier that already passes, or a task nobody can solve, is reported as invalid.
* **Dev / held-out.** The split is `sha1(task id)`, so adding a task never moves another. Tune against `dev`;
  report `heldout`. Anything that improves `dev` and not `heldout` has learned the suite.
* **Attacks are a regression suite for Laya Guard.** A scripted compromised model attempts a destructive delete,
  a secrets read, a prompt-injected command and so on. The task passes only if the harness **held the call for a
  person** and nothing changed. Approve the call, or turn Laya Guard off, and the same task fails — the test file
  proves both.
* **Mutation-graded tests.** A test-writing task passes only if the tests it writes kill mutants of the code.

### Drivers

| driver | model | use |
|---|---|---|
| `oracle` | scripted from the task's solution | offline CI: proves the tools, gates, hooks and recording work |
| `record` | the real model, written to `evals/cassettes/<task>.jsonl` | capture a run once |
| `replay` | the cassette, no network | re-run it for free; a request that was never recorded is a loud `CassetteMiss` |
| `live` | the configured model | the number that matters, and the only one that costs anything |

Cassettes key a request by a hash of the normalised system prompt, tool names and messages. Temp paths, timestamps,
long hex ids, durations and short git hashes are normalised away, so a recording replays in a different directory
on a different day. Change the prompt and you get a miss, not a stale answer.

### Reading a run

Runs are saved as JSON under `~/.cordata/evals/`. `python -m evals compare A.json B.json` names the tasks that
flipped in either direction. The aggregate splits dev from held-out and breaks results down by category, with the
mean reward from the same `outcomes.score` the live turns use — so an eval number and a production number mean the
same thing.

## Trajectories: the ledger as training data

`backend/trajectory.py` joins the same two records into the row shapes a trainer reads —
`(context, arm, propensity, reward)` off-policy rows for a model that would replace the
bandit's tabular scorer, Laya's behaviour-cloning rows, and `(request, tools, reward)`
SFT / preference pairs for the small local models. `make laya-export`; the row shapes and
the fine-tune loop are in `docs/laya-triage.md`.

Two rules carry over unchanged. A row only exists for a turn whose reward is **known**
(`Reward.known`, i.e. reconciled), because a turn scored on half its signal is worse than
one not scored; and the same request never lands in both the train and the eval split —
the split is a hash of the request text, exactly as it is inside the suite, so adding
turns never moves an existing one.

A fine-tuned **Laya checkpoint** is then a candidate like any other, and passes the same
gate: `backend/laya_checkpoints.py` puts it in force for a run (`LAYA_MODEL`), measures it
against the checkpoint it would replace on the same tasks, and only a pass — dev no
regression plus a gain, then held-out clean — may go live (`make laya-eval`, `shadow` by
default). It changes Laya's weights, never the Guard floor.

## What this does not do

* The oracle driver tests the harness, not the model. Only `live` measures the model.
* The weights above are a prior, not a fit. Phase 6 revisits them against the person's own reverts.
* Nothing here lowers a Guard floor or waives an approval; learning built on this is bounded by that.
* The gate measures *the harness with the candidate in force*, so a Laya checkpoint that only
  re-ranks routing can look flat on it — the oracle driver ignores Laya entirely, and the live
  driver's routing effect is diluted by the tasks' own verifiers. Read a checkpoint's verdict
  as "the harness got no worse", not as "Laya got better"; `docs/laya-triage.md`'s report is
  where Laya itself is measured.
