#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 1 ]]; then
  echo "usage: $0 scripts/e2e-*.sh" >&2
  exit 2
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$1"
RUN_ID="${E2E_RUN_ID:-g4c-$(date +%s)-$$}"
NODE_IMAGE="${E2E_NODE_IMAGE:-gpustat4cluster-e2e-node:local}"

normalize_path() {
  local value="$1"
  if [[ "$value" == /work/* ]]; then
    printf '%s\n' "$ROOT_DIR/${value#/work/}"
  else
    printf '%s\n' "$value"
  fi
}

SERVER_BIN_PATH="$(normalize_path "${SERVER_BIN:-$ROOT_DIR/target/release/server}")"
BACKEND_BIN_PATH="$(normalize_path "${BACKEND_BIN:-$ROOT_DIR/target/release/gpustat4cluster-client-backend}")"
CLIENT_BIN_PATH="$(normalize_path "${CLIENT_BIN:-$ROOT_DIR/target/release/gpustat4cluster}")"

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required to run containerized e2e tests" >&2
  exit 127
fi

docker build -f "$ROOT_DIR/docker/e2e-node.Dockerfile" -t "$NODE_IMAGE" "$ROOT_DIR" >/dev/null

E2E_NODE_MODE=docker \
E2E_RUN_ID="$RUN_ID" \
E2E_NODE_IMAGE="$NODE_IMAGE" \
E2E_SKIP_BUILD="${E2E_SKIP_BUILD:-0}" \
SERVER_BIN="$SERVER_BIN_PATH" \
BACKEND_BIN="$BACKEND_BIN_PATH" \
CLIENT_BIN="$CLIENT_BIN_PATH" \
E2E_PROTOCOL="${E2E_PROTOCOL:-}" \
E2E_ROBUSTNESS_GROUP="${E2E_ROBUSTNESS_GROUP:-}" \
E2E_SCALE_GROUP="${E2E_SCALE_GROUP:-}" \
"$SCRIPT"
