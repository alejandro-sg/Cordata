# Deck research (2026-11-09)

What this records: pages captured with craft-anything, one REA run, and a read of harness2's source. Each claim says
where it came from. Nothing here is a benchmark of those products.

## Method and limits

- **craft-anything** (`craft-anything-cli --deep --allow-run web-walkthrough-record`): 1440x900 screenshot and
  accessibility capture of the public home pages of conductor.build, zed.dev, cursor.com and warp.dev, taken
  2026-11-09. Marketing pages only: the product screenshots on them are what the vendors chose to show, not the
  products' real behaviour.
- **REA** (`analyze_javascript_application` on a clone of google-research/harness2, commit 6868817): found 0 JavaScript
  files, 0 modules, 0 findings; it inventoried 4 JSON files, one of which (`dependencies/ASSETS.json`) lists the
  vendored third-party runners (`jb_runner`, `lab_runner`, `wb_runner`, opencode patches). REA targets binaries,
  bundles and web pages; harness2 is ~16k lines of Python, so REA says nothing about its logic. The patterns below
  come from reading `harness2/core/*.py` directly.

## What the four products put in front of the user

| Product | Seen on the page |
|---|---|
| Conductor | Workspace list with avatars and ages; tool calls folded to "17 tool calls, 3 messages"; Changes / Checks / Review tabs; a "Ready to merge" chip on the PR |
| Zed | Thread list with branch, age and diff stats (`+23 −2`); tool calls as single muted lines |
| Cursor | "READY FOR REVIEW 5": finished agents listed with diff stats and a one-line outcome; live preview of the running app |
| Warp | A grid of dots, one per run, live ("8 x 28 · live"); evals and benchmarks as a product claim |

## What Cordata's deck took from it (built, tested)

- Outcome line per thread: the first sentence of the agent's last reply (`flight::outcome`).
- "ready to review" badge: changed files, no open gate, no collision, no failed check (`flight::ready`).
- Folded tool calls, but as a strip you can read: one bar per action, hover for the action (`flight::steps`).

## Gaps found, not built

1. **Resume checks guards, not budgets or workers.** Closed for the safety guards: `cordata-run resume` refuses when
   settings now turn one off (`machine::config_drift`) unless `--allow-weaker-guards`. harness2
   (`core/runmark.py`) fingerprints the whole harness; Cordata records other changed settings as a warning only.
2. **No fleet view past ~10 threads.** Lanes are one row each. Warp's dot grid shows 200+ runs at once; a compact mode
   (one dot per thread, colour by state) would keep the deck useful at scale.
3. **Budget identity is not checked.** harness2 (`core/ledger.py`) asserts that spent rollouts equal the planned budget
   and raises `BudgetError` otherwise. Cordata's ledger records spend but nothing asserts that it matches the plan.
4. **Unknown is not zero.** harness2's judge returns `None` on failure and excludes it from the denominator
   (`core/judge.py`). Worth checking Cordata's check/receipt counts for the same trap.
5. **Ground view has no GPUI test**, and the lane detail panel has only been exercised by the headless render bench,
   not by a click-through (the shell here has no accessibility permission to click the window).
