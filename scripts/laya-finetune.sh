#!/usr/bin/env bash
# The Laya fine-tune loop, end to end: collect → export → train → register → gate.
# docs/laya-triage.md has the long version; this is the four commands it describes, with
# the checks that catch a mistake before it costs hours of training.
#
#     scripts/laya-finetune.sh export  [--project DIR] [--out DIR]
#     scripts/laya-finetune.sh train   <data dir> <output dir>     # needs Laya's repo + a GPU/MPS box
#     scripts/laya-finetune.sh register <id> <checkpoint dir>
#     scripts/laya-finetune.sh gate    [--mode shadow|live] [--checkpoint ID]
#
# `export` and `gate` run here. `train` does not: it needs Laya's own training script, which lives in its
# repository (notebooks/laya_finetune_typed_decisions_mps.py) and hours on a GPU or Apple Silicon box. This
# script only tells you what to run and then waits for a directory — the honest boundary, because a training
# run that silently did nothing is worse than one that was never started here.
#
# The shipped checkpoints are near chance, so the first pass is a way to exercise the loop, not a model that
# routes better. Whatever comes out is a *candidate*: `gate` measures it against the live checkpoint on the
# eval suite and only a pass may go live. Nothing here can lower the Guard floor or waive an approval.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

BACKEND="$ROOT/backend"
PY="$BACKEND/.venv/bin/python"

[[ -x "$PY" ]] || { err "no venv at $BACKEND/.venv — run make setup"; exit 1; }

command="${1:-}"; shift || true

export_laya() {
  log "exporting the ledger and Laya's triage log as training rows…"
  ( cd "$BACKEND" && PYTHONPATH=. "$PY" trajectory.py export --validate "$@" )
}

train_laya() {
  data="${1:-}"; out="${2:-}"
  if [[ -z "$data" || -z "$out" ]]; then
    err "usage: scripts/laya-finetune.sh train <data dir> <output dir>"
    exit 1
  fi
  [[ -f "$data/train.jsonl" || -f "$data/laya.train.jsonl" ]] || { err "$data has no train.jsonl or laya.train.jsonl — run 'export' first"; exit 1; }
  cat <<EOF
Training runs off this machine. In Laya's repository:

  1. use its script        notebooks/laya_finetune_typed_decisions_mps.py
  2. point its loader at   $data
     (replace load_dataset("LocalLLaMA/typed-decisions", …) with
      datasets.load_dataset("json", data_files={"train": "$data/laya.train.jsonl",
                                                "eval": "$data/laya.eval.jsonl"}))
  3. write the checkpoint to $out

Then come back:
  scripts/laya-finetune.sh register <id> $out
  scripts/laya-finetune.sh gate --mode shadow
EOF
  if [[ -d "$out" ]]; then
    warn "$out already exists — if that is the finished checkpoint, register it; otherwise this will overwrite it"
  fi
}

register_laya() {
  id="${1:-}"; path="${2:-}"
  [[ -n "$id" && -n "$path" ]] || { err "usage: scripts/laya-finetune.sh register <id> <checkpoint dir>"; exit 1; }
  ( cd "$BACKEND" && PYTHONPATH=. "$PY" laya_checkpoints.py register "$id" "$path" )
}

gate_laya() {
  log "gating candidate checkpoints against the eval suite (default: shadow — records, activates nothing)…"
  ( cd "$BACKEND" && PYTHONPATH=. "$PY" laya_checkpoints.py evaluate "$@" )
}

case "$command" in
  export)   export_laya "$@";;
  train)    train_laya "$@";;
  register) register_laya "$@";;
  gate)     gate_laya "$@";;
  *)        err "usage: scripts/laya-finetune.sh {export|train|register|gate} …"; exit 1;;
esac
