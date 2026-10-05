"""The evaluation harness: does the harness actually work, and did a change make it better?

Everything the harness might learn — a route policy, a prompt, a skill, a bandit's arm, a fine-tuned
Laya — is only an improvement if a number says so, on tasks it was not tuned on. This package is that
number's source:

* ``task``     — a task is data: a small repo, a prompt, a verifier that must fail before the work and
  pass after it, and an *oracle* solution that proves the task is solvable (``validate`` runs every one);
* ``catalog``  — the tasks themselves: bug fixes, features, refactors, test-writing (scored by whether
  the tests kill mutants), config edits, multi-file changes, answer-only questions, and **attacks** — a
  scripted compromised model tries the dangerous thing and the task passes only if the harness stopped it;
* ``cassette`` — record a live model once, replay it deterministically forever (also what makes a
  test that drives a model stop flaking on its wording);
* ``runner``   — drives the real ACP server, scores each task (verifier + the turn's outcome record) and
  aggregates, with a stable **dev / held-out** split so tuning on one and reporting the other is honest.

    backend/.venv/bin/python -m evals list | validate | run | compare
"""
