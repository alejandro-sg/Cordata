# Scripts

macOS-oriented, plain bash.

| Script | Does |
|---|---|
| `install-cli.sh` | Symlinks `bin/cordata` into `~/.local/bin` (`make cli`). |
| `store-key.sh` / `check-key.sh` | Store the Ollama API key in the macOS Keychain; show where the key comes from and whether Ollama accepts it. |
| `cloud-env.sh` | Run a command on the Ollama Cloud profile, with the key from the shell or Keychain. |
| `package-app.sh` | Builds, signs and self-tests `Cordata.app` (`make app`). |
| `guard.sh` | The pre-tool hook (`.cordata/settings.json`): runs `cordata-guard`; exit 2 blocks the call. |
| `verify.sh` | Secrets check + `make tui-test` (`--all` adds `make app-test`). Exit 0 or fail. |

The secrets scan is a Rust binary (`tui/crates/cordata-guard`): `make secrets`, `make secrets ARGS=--history`, and the
pre-commit hook (`make hooks`).
