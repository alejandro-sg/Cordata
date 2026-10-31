#!/usr/bin/env bash
# Regenerate the Python gRPC stubs for proto/harness.proto into backend/daemon/.
#
# The generated files are committed, so running the daemon needs only `grpcio` + `protobuf`
# (backend/requirements.txt); `grpcio-tools` is needed here, to *regenerate*. The Rust side
# (the retired lab/ client) generated its own code at build time; nothing in the tree does now.
#
#   make proto
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PY="$ROOT/backend/.venv/bin/python"
OUT="$ROOT/backend/daemon"

"$PY" -c "import grpc_tools" 2>/dev/null || { echo "grpcio-tools is not installed: backend/.venv/bin/pip install grpcio-tools==1.83.1" >&2; exit 1; }

"$PY" -m grpc_tools.protoc -I "$ROOT/proto" \
  --python_out="$OUT" --pyi_out="$OUT" --grpc_python_out="$OUT" \
  "$ROOT/proto/harness.proto"

# protoc emits a top-level `import harness_pb2`; inside the `daemon` package it must be relative.
sed -i.bak 's/^import harness_pb2 as harness__pb2/from . import harness_pb2 as harness__pb2/' "$OUT/harness_pb2_grpc.py"
rm -f "$OUT/harness_pb2_grpc.py.bak"
echo "generated $OUT/harness_pb2*.py"
