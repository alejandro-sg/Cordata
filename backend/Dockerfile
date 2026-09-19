# Multi-stage build for the mini-hosted deployment (see deploy/mini/backend/).
# Inference stays on whichever machine has the GPU/NPU — OMLX_BASE_URL is
# supplied at container runtime via docker-compose (same reasoning as the
# frontend's BACKEND_URL), never baked in here. This container only runs the
# agent loop + tool execution, not the model.
#
# Runs as root, deliberately: the app's own security boundary is
# SANDBOX_API_TOKEN + the LAN itself (see settings.py's own comment on
# main.py binding 0.0.0.0), not container-user isolation, and root avoids
# fighting bind-mounted volume ownership for sessions/workspace/repl_state.

FROM python:3.12-slim
WORKDIR /app

# build-essential: some pinned deps (sentence-transformers's own transitive
# requirements) don't always ship prebuilt aarch64 wheels on PyPI; installed
# once at image-build time, not present in the final layer's PATH usage but
# kept (not stripped to a second stage) since a `pip install` from inside
# the running container (debugging) would otherwise silently lack it.
RUN apt-get update && apt-get install -y --no-install-recommends build-essential \
 && rm -rf /var/lib/apt/lists/*

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY . .

EXPOSE 8001
# Not `python main.py` — that path's `reload=True`/`reload_dirs` (see
# main.py's own comment) is a local-dev convenience with nothing to watch
# in a built image; the plain uvicorn CLI runs the same app without it.
CMD ["uvicorn", "main:app", "--host", "0.0.0.0", "--port", "8001"]
