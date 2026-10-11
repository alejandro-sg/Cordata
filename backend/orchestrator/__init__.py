"""The orchestrator: plan → execute → harden → audit across Claude Code, Antigravity and Ollama workers.

    plan_artifact.py   the plan contract (schema + the checks a schema cannot express)
    quota_ledger.py    per-plan envelope + rolling pool windows, soft/hard lines, provider signals
    handoff.py         the portable, secret-scrubbed packet one worker leaves for the next
    executor.py        EXECUTE: steps in checkpoints, safe-point handoffs
    audit.py           AUDIT: Tier 1 deterministic checks, Tier 2 residual model review
    machine.py         the durable state machine, bounded retries, resume
    dispatch.py        who does what: gates, then Laya + bandit ordering; offline replay
    config.py          the default candidate roster and settings
    fakes.py           in-process fake workers for tests and the eval suite

``docs/orchestrator-plan.md`` is the design; ``python -m orchestrator --help`` the command line.
"""
