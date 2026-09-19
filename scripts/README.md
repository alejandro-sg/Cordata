# Dev environment scripts

macOS-oriented (this project targets Apple Silicon for the oMLX model server),
but the shell scripts themselves are plain bash and should work on Linux too.

| Script | Does |
|---|---|
| `setup.sh` | One-time: backend venv + pinned deps, frontend deps, `.env`/`.env.local` from their `.example` templates. Safe to re-run. |
| `dev-up.sh` | Starts Qdrant (container, auto-detects docker/podman), backend, frontend. Checks your oMLX server is reachable and warns (doesn't start it — that's external). `--no-qdrant` to skip it. |
| `dev-down.sh` | Stops backend + frontend. Leaves Qdrant running (`--with-qdrant` to stop it too). |
| `dev-restart.sh` | Stops and restarts backend + frontend only. |
| `dev-status.sh` | Prints what's running and whether each is actually responding. |
| `deploy-mini.sh` | Deploys both halves to the mini (ssh alias `sierralab-mini`): backend, then frontend. |
| `deploy-mini-backend.sh` | Syncs `backend/` to the mini and rebuilds its container. Leaves an already-customized remote `docker-compose.yml` (`OMLX_BASE_URL`, `QDRANT_HOST`, `SANDBOX_API_TOKEN`) alone; state dirs (sessions/workspace/repl_state/skills_generated/mcp_servers.json) persist across redeploys. |
| `deploy-mini-frontend.sh` | Syncs `frontend/` to the mini and rebuilds its container. Leaves an already-customized remote `docker-compose.yml` (`BACKEND_URL`, `SANDBOX_API_TOKEN`) alone. |

Also available as `make setup` / `make up` / `make down` / `make restart` / `make status` / `make deploy-mini` / `make deploy-mini-backend` / `make deploy-mini-frontend` from the repo root.

State lives in `.dev/` at the repo root (gitignored): one `<name>.pid` + `<name>.log` per process. `tail -f .dev/backend.log` / `.dev/frontend.log` to watch either live.
