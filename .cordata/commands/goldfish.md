---
description: Run a stateless Goldfish review on git diff with zero conversational bias
---
Perform a stateless "Goldfish" review of the changes ($ARGUMENTS, default: working tree diff `git diff HEAD`).

Assume ZERO previous conversational context, explanations, or rationalizations. Inspect only the code diff and live files with fresh eyes.

Audit strictly against:
1. **Workspace Invariants** ([`AGENTS.md`](../../AGENTS.md)): zero plaintext secrets, hermetic smoke tests, no untracked large file reads, sandboxing boundaries.
2. **Failure Modes & Edge Cases**: unexpected None/null, broken error handling, race conditions, unhandled exceptions.
3. **Architecture Drift**: unintentional changes to public interfaces, settings keys, on-disk formats (`state.db`, session files, `control.ndjson`) or the shared redaction fixture (`proto/fixtures/redact_cases.json`).

Output format:
- **Verdict**: PASS / REVISE
- **Findings**: List of concrete issues with `file:line` citations.
- **Evidence/Reproduction**: Exact deterministic command to reproduce or verify each finding.
