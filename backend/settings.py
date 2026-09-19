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
ANTHROPIC_API_KEY = os.getenv("ANTHROPIC_API_KEY", "")
ANTHROPIC_MODEL_ID = os.getenv("ANTHROPIC_MODEL_ID", "claude-sonnet-5")
CLOUD_OPENAI_BASE_URL = os.getenv("CLOUD_OPENAI_BASE_URL", "")
CLOUD_OPENAI_API_KEY = os.getenv("CLOUD_OPENAI_API_KEY", "")
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
# (frontend/app/api/sandbox/[...path]/route.ts, frontend/app/api/games/
# [...path]/route.ts) must present on every /sandbox/* and /games/* request.
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

# main.py: extra allowed CORS origins for the AG-UI frontend, beyond
# http://localhost:3000 (always allowed, so a fresh single-machine checkout
# works with no config). Comma-separated, e.g. "https://agent.example.com,
# https://agent.local" once you're hosting the frontend somewhere other than
# the machine running this backend.
EXTRA_CORS_ORIGINS = [origin.strip() for origin in os.getenv("EXTRA_CORS_ORIGINS", "").split(",") if origin.strip()]
