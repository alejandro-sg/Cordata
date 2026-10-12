# Dev environment scripts

macOS-oriented (this project targets Apple Silicon for the oMLX model server),
but the shell scripts themselves are plain bash and should work on Linux too.

| Script | Does |
|---|---|
| `setup.sh` | One-time: backend venv + pinned deps, `backend/.env` from its template, a release build of the terminal client; `--web` adds the web UI's dependencies and `frontend/.env.local`. Safe to re-run. |
| `dev-up.sh` | Starts Qdrant (container, auto-detects docker/podman), backend, frontend. Checks your oMLX server is reachable and warns (doesn't start it — that's external). `--no-qdrant` to skip it. |
| `dev-down.sh` | Stops backend + frontend. Leaves Qdrant running (`--with-qdrant` to stop it too). |
| `dev-restart.sh` | Stops and restarts backend + frontend only. |
| `dev-status.sh` | Prints what's running and whether each is actually responding. |
| `deploy-mini.sh` | Deploys both halves to the mini (ssh alias `mini`): backend, then frontend. |
| `deploy-mini-backend.sh` | Syncs `backend/` to the mini and rebuilds its container. Leaves an already-customized remote `docker-compose.yml` (`OMLX_BASE_URL`, `QDRANT_HOST`, `SANDBOX_API_TOKEN`) alone; state dirs (sessions/workspace/repl_state/skills_generated/mcp_servers.json) persist across redeploys. |
| `ollama-env.sh` | Runs a command on the Experimental Lab profile: `MODEL_PROVIDER=ollama_cloud` against the local Ollama daemon (no API key, nothing pulled). `make tui-lab`. Refuses to start, with the fix, if the daemon isn't answering. |
| `deploy-mini-frontend.sh` | Syncs `frontend/` to the mini and rebuilds its container. Leaves an already-customized remote `docker-compose.yml` (`BACKEND_URL`, `SANDBOX_API_TOKEN`) alone. |
| `laya-finetune.sh` | The Laya fine-tune loop (`export` · `train` · `register` · `gate`, [`docs/laya-triage.md`](../docs/laya-triage.md)). `export` writes the training rows from the ledger; `train` prints what to run off-machine rather than pretending to run it; `gate` measures a registered checkpoint against the eval suite before it may go live. |

Also available as `make setup` / `make up` / `make down` / `make restart` / `make status` / `make deploy-mini` / `make deploy-mini-backend` / `make deploy-mini-frontend` from the repo root.

The Rust clients under `lab/` and `tui/` are built directly, not by a script here: `make lab` runs
the native GPUI window (`make lab WATCH=<dir>` to watch a workspace; `make lab-test` for its
tests; `make lab-demo` — [`lab-demo.sh`](lab-demo.sh) — for the window against a scripted daemon,
nothing spent; `make lab-live` for the Rust client against the real Python bridge) and `make tui` / `make tui-test` / `make stress` cover the terminal clients. See
[`docs/meta-harness.md`](../docs/meta-harness.md) and [`tui/README.md`](../tui/README.md).

State lives in `.dev/` at the repo root (gitignored): one `<name>.pid` + `<name>.log` per process. `tail -f .dev/backend.log` / `.dev/frontend.log` to watch either live.
