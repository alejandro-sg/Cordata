# Laya triage — a fast decision layer around the orchestrator

[Laya](https://github.com/NandhaKishorM/laya) is a non-autoregressive "System 1"
model: typed answers (`choice`, `noul` = P(yes), `score`) with confidence, in one
forward pass (~30–150 ms warm), no text generation and no tool calls. The LLM is
the opposite: slow, generative, tool-calling. This backend uses each where it is
strong.

| layer | who | does |
|---|---|---|
| orchestrator control plane | **Laya** (`backend/laya_triage.py`) | per user turn: what kind of work is this, how hard is it |
| orchestrator | **LLM** (the active provider) | generates replies, decides and calls tools, delegates (`orchestrate`, specialists) |
| sub-agents / specialists | **LLM**, one model **tier** each | do the delegated work (architect, debugger, digest) |
| content guard | **Laya** (`prompt_security.py`) | advisory injection score on untrusted tool output |

Both Laya features share **one** loaded checkpoint (`LAYA_MODEL`), warmed in a
background thread at startup.

## What `route` mode does (default)

`LAYA_TRIAGE_MODE=route`, per user turn:

1. Laya classifies the request in a worker thread, with a timeout
   (`LAYA_TRIAGE_TIMEOUT`, default 2 s; the first call can be slower while weights load).
2. **Routing hint.** If the route's confidence is ≥ `LAYA_TRIAGE_THRESHOLD` (0.85) and
   the route has a clear tool affinity — `files_and_code`, `web_research`, `memory`,
   `team` — **one short advisory line** is appended to the orchestrator's system
   prompt *for that turn only*. It is restored afterwards and is never in the
   conversation messages or the persisted session. The text is a fixed string per
   Laya enum answer; nothing from the user's message is interpolated, so a prompt
   cannot steer the agent through it. `chat` and `other_tools` never produce a hint
   (don't discourage tool use on a guess).
3. **Model tier for sub-agents.** Laya's complexity estimate, if it is at least
   moderately sure (confidence ≥ 0.6), is the default tier — `fast` / `standard` /
   `strong` — for the sub-agents the orchestrator spawns that turn. A sub-agent spec's
   own `"tier"` always wins.
4. **Below the gate, on timeout, on error: nothing changes.** The LLM decides exactly
   as it did before Laya existed.

It never blocks a tool and never forces one. That is deliberate — see "Why advisory".

`shadow` mode logs only (no hint, no tier). `off` does nothing and loads nothing.

### Watching it live

- **In `acp-tui`:** nothing to turn on. A dim `◆ laya` line appears under each prompt ~70 ms
  after you send it — route and confidence, and whether a hint was sent or why not — and a second
  line when the turn ends says what the LLM actually did and whether it agreed. `/laya off` hides
  it. The backend pushes these as an ACP extension notification (`_sierralab/laya`, emitted by
  `LayaTriageHooks(notify=…)`); clients that don't know it ignore it.
- **Any front door** (the web UI, Zed, scripts): `make laya-watch` follows the log and prints
  one line per turn as it ends. It lags the TUI line by one turn, because the log is written when
  a turn finishes.

### The log

Every mode but `off` appends one JSON line per turn to `LAYA_TRIAGE_LOG`
(`backend/laya_triage.jsonl`, gitignored — it holds user prompts, secret-masked):
Laya's decision beside the route the LLM *actually* took (derived from the tools it
called), the hint applied, how long the turn waited, and the tier each sub-agent got.

```bash
backend/.venv/bin/python backend/_laya_triage_report.py            # agreement by confidence
```

It prints agreement with the orchestrator, the majority-class baseline, per-route
precision/recall, the most common disagreements, and a confidence-gate table
(coverage / agreement / how many would be misrouted at each threshold). "Agreement"
is with what the LLM did, not with ground truth — the LLM is the policy being
measured and makes mistakes too.

## Sub-agents use the same provider, tiers, and key

Sub-agents (the specialists, the digest) are built by
`sub_agent_common.build_sub_agent_model`, which goes through the same
`agent_common.build_model` as the orchestrator. So with `MODEL_PROVIDER=openai_compatible`
(e.g. Ollama Cloud) sub-agents run on the cloud too, with the **same API key — no second
key is needed** (they are the same process and the key never enters their tools,
prompts or environment; see `THREAT_MODEL.md`, "Provider API keys"). Before this they
were hardwired to local oMLX.

Tiers are model ids on the active provider, set in the environment; an unset tier
means "the provider's own model", so nothing changes until you configure them:

```bash
export MODEL_TIER_FAST=gpt-oss:20b MODEL_TIER_STANDARD=gpt-oss:120b MODEL_TIER_STRONG=glm-5.3
```

Mind your provider's concurrency limit: the orchestrator plus a running specialist are two
simultaneous requests on one account. A second key on the same account would not raise
that limit.

## Why advisory — and what the live runs showed

Laya's README is explicit that the shipped checkpoints are **near chance zero-shot** on
typed decisions (0.36 against a 0.46 majority-class baseline) and only become useful
after fine-tuning on your own decisions. A live run here confirmed it:

- It got most obvious prompts right (a `file_read` request → `files_and_code`, 0.98).
- It routed a request that used a team tool to `memory` at 0.67 — below the gate, so
  the LLM decided alone, which is the gate doing its job.
- Its `needs_team` (`noul`) scored **0.90 on "Reply with exactly the word: pong"**. So
  `needs_team` alone does **not** produce a team hint by default; only a *confident team
  route* does. Enable the independent trigger with `LAYA_TRIAGE_TEAM_THRESHOLD=0.8` only
  after the report shows `needs_team` separating team turns from the rest on your data.

Hence: hint above a confidence gate, never block, always log. Tune
`LAYA_TRIAGE_THRESHOLD` from the report's gate table, not from this document.

The same numbers bound what can be *learned* on top of Laya: `bandit.py`'s `route` arm and
`laya_risk`'s complexity reading are only as good as the checkpoint behind them, and the
shipped one is near chance, so those arms are weak features until a fine-tuned checkpoint
(from the loop below) clears the gate. The bandit's reward is outcomes-based, so it is not
blocked by this — it just has less to work with on the Laya-derived points than on `engine`
and `ask`.

## Improving it: collect → export → fine-tune → swap

`scripts/laya-finetune.sh` runs the four steps below (`export` / `train` / `register` /
`gate`); this section is what each one does. `make laya-export`, `make laya-eval`.

1. Run normally (or with `acp-stress` scenarios) so the log accumulates real turns.
2. **Export.** `backend/.venv/bin/python backend/trajectory.py export --validate` writes
   every row shape in one directory (`~/.sierralab/trajectories/<project>/`, override
   `--out`): `laya.{train,eval}.jsonl` — `state`, `questions`, `gold` as JSON-string
   columns, byte-for-byte the schema `_laya_triage_export.py` writes and Laya's training
   script reads (a `--validate` pass feeds every question through Laya's own
   `build_sequence` with the loaded tokenizer, the call its training script makes) — plus
   `bandit.*`, `sft.*` and `dpo.*` for the trainer that replaces the bandit's tabular
   scorer and for the small local models. See "The other three row shapes" below.
   `_laya_triage_export.py --out DIR` still works and writes just the Laya pair.

   Labels are what the LLM did, one-hot — a behaviour-cloning target, mistakes included,
   so review before training. Every row is split train / eval by a hash of its own
   request, so the same request is never in both.
3. Fine-tune with Laya's script (`notebooks/laya_finetune_typed_decisions_mps.py` in its
   repo — Apple Silicon; a Kaggle 2×T4 notebook also exists). Its dataset loader calls
   `load_dataset("LocalLLaMA/typed-decisions", ...)`; replace that call with
   `datasets.load_dataset("json", data_files={"train": "laya_data/train.jsonl"})["train"]`.
   Laya's own run used ~30k questions and took hours on 2×T4; a few hundred rows will
   mostly memorise, so treat early runs as a way to exercise the loop.
4. **Register and gate.** `backend/.venv/bin/python backend/laya_checkpoints.py register
   <id> <checkpoint dir>` records it under `~/.sierralab/laya/checkpoints.json` as a
   *candidate*. `make laya-eval` (default `--mode shadow`) then puts it in force for an
   eval run (`LAYA_MODEL=<dir>`) and measures it the same way a staged skill is measured:
   nothing that passed without it may fail with it, something must improve, and the same
   must hold on held-out tasks (`promote.py`; `docs/evals.md`). Only a pass may go live —
   `activate` refuses a checkpoint whose last verdict did not promote, and `--force` is
   the deliberate override for one measured elsewhere. A live checkpoint wins over the
   stock repo for the next backend start; an explicit `LAYA_MODEL` still overrides both.

5. **Look at the traffic again.** Re-run the report on fresh turns and compare the
   confidence-gate table with the stock model's before raising the threshold or routing
   anything harder.

Fine-tuning itself still runs off this machine: step 3 is Laya's script and hours on a
GPU or an Apple Silicon box, and `scripts/laya-finetune.sh train` prints what to run
rather than pretending to run it. Steps 1–2 and the format validation are tested here;
`LAYA_MODEL` loading goes through `laya.load`, which accepts a local path.

### The other three row shapes

`trajectory.py` also writes what the rest of the learning loop needs, from the same
ledger (`docs/evals.md`):

| file | row | for |
|---|---|---|
| `bandit.*.jsonl` | `(context, point, arm, propensity, reward)` | the model that replaces the tabular scorer behind `bandit.choose` when there is enough data. Only choices the bandit recorded a decision for are exported — the propensity comes from that audit row, and a made-up one teaches the wrong thing about how the data was collected |
| `sft.*.jsonl` | `(request, the tools the turn ran, reward)` | imitation data for the small local models |
| `dpo.*.jsonl` | two turns in the same coarse context, better against worse | a preference pair; only comparable contexts, and only when they really differ |

### What a checkpoint can and cannot change

Its **weights** — which route it leans toward, how confident it is. Not what the harness
permits: Laya Guard's floor, `laya_risk`'s thresholds and the approval gate are read from
code that does not consult the registry, and a better checkpoint can only re-rank options
that were already on the table. `docs/evals.md` says the same thing about every learned
artifact, and `THREAT_MODEL.md` about the ones that reach a turn.

## Settings

| variable | default | meaning |
|---|---|---|
| `LAYA_TRIAGE_MODE` | `route` | `route` / `shadow` / `off` |
| `LAYA_TRIAGE_THRESHOLD` | `0.85` | route confidence needed for a hint |
| `LAYA_TRIAGE_TEAM_THRESHOLD` | *(off)* | P(needs team) that hints a team on its own |
| `LAYA_TRIAGE_TIMEOUT` | `2.0` | seconds a turn waits for Laya before going without a hint |
| `LAYA_TRIAGE_LOG` | `backend/laya_triage.jsonl` | the log |
| `LAYA_MODEL` / `LAYA_MODEL_SUBFOLDER` | `convaiinnovations/laya` / `multilingual` | checkpoint (repo id or local dir). Unset, a checkpoint registered *live* by `laya_checkpoints.py` wins; set, it overrides everything |
| `MODEL_TIER_FAST` / `_STANDARD` / `_STRONG` | *(unset)* | sub-agent model ids on the active provider |
| `LAYA_GUARD_ENABLED` / `LAYA_GUARD_THRESHOLD` | `false` / `0.75` | the separate injection guard on untrusted tool output |

## Costs and caveats

- **Memory.** One Laya checkpoint (~650 MB) per backend process. `acp-stress` starts a
  process per session, so a large `-c` multiplies that; for pure harness stress runs set
  `LAYA_TRIAGE_MODE=off LAYA_GUARD_ENABLED=false`.
- **Latency.** Warm calls were 60–230 ms in live runs; the very first turn after startup
  waited ~1.5 s while weights finished loading.
- **Privacy.** The log holds your prompts (masked for secrets). Delete it freely.
- **Not a security boundary.** The hint is advisory and the classifier is
  fallible; nothing here changes what the approval gates allow.
