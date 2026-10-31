"""Single source of truth for env-derived config, loaded once.

Previously three modules (agent_common.py, model_control_tool.py,
sub_agent_common.py) each read OMLX_BASE_URL/OMLX_MODEL_ID from os.getenv
independently, relying on agent_common.py's load_dotenv() call running
first. That ordering was fragile: main.py imports model_control_tool
*before* agent_common, so model_control_tool's os.getenv ran before .env
was loaded — any value set only via .env (not exported in the shell) was
silently ignored there while every other module picked it up correctly.
Loading .env here, at first import of this module by whichever caller gets
there first, removes the ordering dependency entirely.
"""

import os
import secrets

from dotenv import load_dotenv

from secret_guard import register_env_secrets, take_secret

load_dotenv()

OMLX_BASE_URL = os.getenv("OMLX_BASE_URL", "http://127.0.0.1:8000/v1")
OMLX_MODEL_ID = os.getenv("OMLX_MODEL_ID", "Ternary-Bonsai-27B-mlx-2bit")

# provider_settings.py: which model backend agent_common.build_model() talks
# to. "local" (default, oMLX via OMLX_BASE_URL/OMLX_MODEL_ID above) |
# "anthropic" | "openai_compatible". These four ANTHROPIC_*/CLOUD_OPENAI_*
# values are the *env-derived defaults* only — provider_settings.py's own
# set_provider_config() mutates the corresponding agent_common.ACTIVE_*
# globals at runtime (same "changed live, resets on restart unless also
# written to .env" shape as ACTIVE_MODEL_ID already has), so a value changed
# via the settings UI takes effect immediately without needing this module
# reloaded.
MODEL_PROVIDER = os.getenv("MODEL_PROVIDER", "local")
# MODEL_PROVIDER=ollama_cloud (ollama_cloud.py): cloud models served through the
# *local* Ollama daemon's OpenAI proxy — no API key (the daemon is signed in), no
# weights pulled. OLLAMA_BASE_URL defaults to http://localhost:11434/v1;
# OLLAMA_MODEL_ID is a registry alias (mistral-large-4 | minimax-m3.1-flash |
# gemma4:31b | kimi-k2.7-code | deepseek-v4.1-flash | glm-5.3-flash) for the orchestrator, empty = its role profile's model;
# OLLAMA_ROLE_<ROLE>=<alias>[:low|medium|high|max] overrides one role's model/effort.
OLLAMA_MODEL_ID = os.getenv("OLLAMA_MODEL_ID", "")
# Provider API keys are credentials the *agent* must never be able to read —
# see secret_guard.py. take_secret reads each from the environment once,
# wraps it in a Secret (prints as "Secret(****)"), and removes it from
# os.environ so `shell` children and environment dumps don't carry it. Source
# them from an exported shell variable, not backend/.env: that file is on disk
# where the agent's file tools can read it (take_secret warns if it finds one
# there), or the macOS Keychain (see secret_guard._keychain_get). OLLAMA_API_KEY is accepted as an alias for CLOUD_OPENAI_API_KEY so
# an existing `export OLLAMA_API_KEY=...` works against
# CLOUD_OPENAI_BASE_URL=https://ollama.com/v1 unchanged.
ANTHROPIC_API_KEY = take_secret(
    "ANTHROPIC_API_KEY",
    keychain=("cordata-anthropic-api-key", "sierralab-anthropic-api-key") if MODEL_PROVIDER == "anthropic" else None
)
ANTHROPIC_MODEL_ID = os.getenv("ANTHROPIC_MODEL_ID", "claude-sonnet-5")
CLOUD_OPENAI_BASE_URL = os.getenv("CLOUD_OPENAI_BASE_URL", "")
# With nothing exported, the macOS Keychain item "cordata-ollama-api-key" (or the pre-rename "sierralab-ollama-api-key") is used — but only when the cloud
# provider is the active one, so a local-model run never touches the keychain.
CLOUD_OPENAI_API_KEY = take_secret(
    "CLOUD_OPENAI_API_KEY", "OLLAMA_API_KEY",
    keychain=("cordata-ollama-api-key", "sierralab-ollama-api-key") if MODEL_PROVIDER == "openai_compatible" else None
)
# Any other credential-looking variable (HF_TOKEN, ...) stays in the
# environment — libraries read some lazily — but its value is masked in tool
# output (prompt_security.SecretRedactionHooks).
register_env_secrets()
CLOUD_OPENAI_MODEL_ID = os.getenv("CLOUD_OPENAI_MODEL_ID", "")
# 127.0.0.1, not "localhost" (same reasoning as OMLX_BASE_URL above): on
# this machine, "localhost" resolves to ::1 first and nothing answers
# there, so every qdrant-client call through the hostname hangs for 15s (3
# internal retries) before failing, while the IPv4 literal connects
# instantly — only matters when QDRANT_HOST isn't set (this deployment's own
# .env points QDRANT_HOST at a different machine on the LAN instead).
QDRANT_HOST = os.getenv("QDRANT_HOST", "127.0.0.1")
QDRANT_PORT = int(os.getenv("QDRANT_PORT", "6333"))

# agent_memory_tool.py: bounds and similarity thresholds for the curated
# memory playbook.
MEMORY_MAX_ACTIVE_ENTRIES = int(os.getenv("MEMORY_MAX_ACTIVE_ENTRIES", "300"))
MEMORY_DUPLICATE_THRESHOLD = float(os.getenv("MEMORY_DUPLICATE_THRESHOLD", "0.97"))
MEMORY_SUPERSEDE_THRESHOLD = float(os.getenv("MEMORY_SUPERSEDE_THRESHOLD", "0.90"))

# sandbox_functions_api.py: shared secret the Next.js proxy
# (frontend/app/api/sandbox/[...path]/route.ts) must present on every
# /sandbox/* request.
# main.py binds uvicorn to 0.0.0.0, not 127.0.0.1 — this deployment genuinely
# runs frontend and backend on different LAN machines (see
# frontend/.env.local.example's BACKEND_URL comment), so "don't expose this
# beyond localhost" was never actually true even for a single-machine setup:
# anything else on the same LAN could already reach runSnippet directly,
# with no gate at all beyond a client-side approval card.
#
# Auto-generated (not empty) when SANDBOX_API_TOKEN isn't set: a fresh
# `secrets.token_hex(32)` every process start, matching the reasoning
# odysseus-dev/odysseus uses for its own same-process loopback token —
# never persisted, never sent to a client directly. The frontend proxy
# routes don't need this value handed to them via env at all in that case;
# they fetch it themselves from GET /sandbox/_local-token, which
# _require_localhost below only answers to a caller on this same machine —
# see that function's own docstring for why that's still a real boundary
# even though the token it hands back is otherwise unauthenticated. This
# closes the "gap isn't silent, but it's still a gap" state the old empty-
# by-default value left every single-machine checkout in: with this, even a
# `curl` from another device on the LAN needs a token it has no way to
# obtain, not just a token it happens not to have been given. Set
# SANDBOX_API_TOKEN explicitly instead once frontend and backend really are
# on separate machines — _local-token's own localhost-only check means the
# auto-generated path can never reach across that hop on its own.
_env_sandbox_token = os.getenv("SANDBOX_API_TOKEN", "")
SANDBOX_API_TOKEN = _env_sandbox_token or secrets.token_hex(32)
SANDBOX_API_TOKEN_AUTO_GENERATED = not _env_sandbox_token

# prompt_security.py: optional local classifier (github.com/NandhaKishorM/
# laya) that scores untrusted tool output for prompt-injection risk before
# it reaches the model, on top of (not instead of) the existing marker wrap.
# Off by default: this backend's own Dockerfile is explicit that the
# container "only runs the agent loop + tool execution, not the model" —
# inference stays on whichever machine has the GPU/NPU (OMLX_BASE_URL).
# Laya is small (322M-421M params) but it's still a local model forward
# pass, so enabling it on the mini deployment is a deliberate per-
# environment opt-in via .env, not a default that reintroduces inference
# into an "always-on, no GPU expected" service.
LAYA_GUARD_ENABLED = os.getenv("LAYA_GUARD_ENABLED", "false").lower() == "true"
LAYA_GUARD_THRESHOLD = float(os.getenv("LAYA_GUARD_THRESHOLD", "0.75"))

# The Laya checkpoint every Laya feature here shares (the injection guard and
# laya_triage — one copy in memory). A Hugging Face repo id or a local directory;
# point LAYA_MODEL at the output of Laya's fine-tuning script (see
# docs/laya-triage.md) to swap in a model trained on your own routing decisions.
# LAYA_MODEL_SUBFOLDER picks one checkpoint out of a repo that bundles several
# ("multilingual" for the stock repo; leave it empty for a fine-tuned directory).
# A checkpoint registered as *live* by `laya_checkpoints.py` wins over the stock repo here, so activating one
# is the whole of "run on it" — while an explicit LAYA_MODEL in the environment still overrides both, which is
# what leaves an experiment possible without touching the registry. The resolution is in that module, which is
# stdlib-only precisely so this import is safe at startup.
import laya_checkpoints as _laya_checkpoints

LAYA_MODEL, LAYA_MODEL_SUBFOLDER = _laya_checkpoints.resolve()
LAYA_MODEL_SUBFOLDER = os.getenv("LAYA_MODEL_SUBFOLDER", LAYA_MODEL_SUBFOLDER)

# laya_triage.py — Laya as the orchestrator's fast "System 1" triage, with the
# LLM kept for generation and tool use. "route" (default) | "shadow" | "off".
#   route  — per user turn, Laya classifies the request in tens of ms. When its
#            confidence clears LAYA_TRIAGE_THRESHOLD, one short routing hint is
#            added to the orchestrator's system prompt *for that turn only*
#            (never persisted to history); below it the LLM decides exactly as
#            it did before. Its complexity estimate also picks the default model
#            tier for sub-agents (MODEL_TIER_* below). Always logs, like shadow.
#   shadow — log only (LAYA_TRIAGE_LOG): Laya's would-be route next to the tools
#            the orchestrator actually used. No hint, no tiering.
#   off    — nothing; Laya isn't loaded for triage.
# The log holds user prompts: local, gitignored, secret-masked. Read it with
# _laya_triage_report.py. The shipped Laya checkpoints are near chance before
# fine-tuning (see docs/laya-triage.md) — which is why route mode only *hints*
# above a confidence gate and never blocks or forces a tool.
LAYA_TRIAGE_MODE = os.getenv("LAYA_TRIAGE_MODE", "route").lower()
LAYA_TRIAGE_THRESHOLD = float(os.getenv("LAYA_TRIAGE_THRESHOLD", "0.85"))
# P(needs a team) at or above which the hint suggests the orchestrate tool,
# independently of the route. OFF by default (empty): zero-shot, Laya's `noul`
# primitive scored 0.90 on "Reply with exactly the word: pong" in a live run
# (docs/laya-triage.md), so acting on it before fine-tuning would nudge trivial
# prompts toward spawning a team. The team hint otherwise fires only from a
# *confident* team route. Enable (e.g. 0.8) once _laya_triage_report.py shows
# needs_team separating team turns from the rest on your data.
_team_threshold = os.getenv("LAYA_TRIAGE_TEAM_THRESHOLD", "").strip()
LAYA_TRIAGE_TEAM_THRESHOLD = float(_team_threshold) if _team_threshold else None
# Seconds a turn waits for Laya before proceeding without a hint (first call
# loads weights; the backend warms them in the background at startup).
LAYA_TRIAGE_TIMEOUT = float(os.getenv("LAYA_TRIAGE_TIMEOUT", "2.0"))
LAYA_TRIAGE_LOG = os.getenv("LAYA_TRIAGE_LOG", os.path.join(os.path.dirname(os.path.abspath(__file__)), "laya_triage.jsonl"))

# Sub-agent model tiers (sub_agent_common.py). Each is a model id on the ACTIVE
# provider — e.g. with MODEL_PROVIDER=openai_compatible against Ollama Cloud:
#   MODEL_TIER_FAST=gpt-oss:20b  MODEL_TIER_STANDARD=gpt-oss:120b  MODEL_TIER_STRONG=glm-5.3
# Empty (default) = that tier uses the active provider's own model, so nothing
# changes until you set them. A sub-agent spec may name a "tier"; otherwise
# Laya's complexity estimate for the turn picks one.
MODEL_TIER_FAST = os.getenv("MODEL_TIER_FAST", "")
MODEL_TIER_STANDARD = os.getenv("MODEL_TIER_STANDARD", "")
MODEL_TIER_STRONG = os.getenv("MODEL_TIER_STRONG", "")

# main.py: extra allowed CORS origins for the AG-UI frontend, beyond
# http://localhost:3000 (always allowed, so a fresh single-machine checkout
# works with no config). Comma-separated, e.g. "https://agent.example.com,
# https://agent.local" once you're hosting the frontend somewhere other than
# the machine running this backend.
EXTRA_CORS_ORIGINS = [origin.strip() for origin in os.getenv("EXTRA_CORS_ORIGINS", "").split(",") if origin.strip()]
