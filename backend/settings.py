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

from dotenv import load_dotenv

load_dotenv()

OMLX_BASE_URL = os.getenv("OMLX_BASE_URL", "http://127.0.0.1:8000/v1")
OMLX_MODEL_ID = os.getenv("OMLX_MODEL_ID", "Ternary-Bonsai-27B-mlx-2bit")
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
