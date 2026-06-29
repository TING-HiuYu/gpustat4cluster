#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 1 ]]; then
  echo "usage: $0 scripts/e2e-*.sh" >&2
  exit 2
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$1"
RUNNER_IMAGE="${E2E_RUNNER_IMAGE:-gpustat4cluster-e2e-runner:local}"
RUN_ID="${E2E_RUN_ID:-g4c-$(date +%s)-$$}"

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required to run containerized e2e tests" >&2
  exit 127
fi

docker build -f "$ROOT_DIR/docker/e2e-runner.Dockerfile" -t "$RUNNER_IMAGE" "$ROOT_DIR" >/dev/null

docker run --rm --privileged \
  --name "g4c-e2e-runner-$RUN_ID" \
  --entrypoint bash \
  -v "$ROOT_DIR:/work" \
  -w /work \
  -e DOCKER_TLS_CERTDIR="" \
  -e E2E_NODE_MODE=docker \
  -e E2E_RUN_ID="$RUN_ID" \
  -e E2E_NODE_IMAGE="gpustat4cluster-e2e-node:local" \
  -e CARGO_TARGET_DIR="/tmp/gpustat4cluster-target" \
  -e SERVER_BIN="/tmp/gpustat4cluster-target/debug/server" \
  -e BACKEND_BIN="/tmp/gpustat4cluster-target/debug/gpustat4cluster-client-backend" \
  -e CLIENT_BIN="/tmp/gpustat4cluster-target/debug/gpustat4cluster" \
  "$RUNNER_IMAGE" \
  -lc '
    set -euo pipefail
    : > /tmp/dockerd.log
    dockerd-entrypoint.sh >/tmp/dockerd.log 2>&1 &
    for _ in $(seq 1 60); do
      docker info >/dev/null 2>&1 && break
      sleep 1
    done
    docker info >/dev/null 2>&1 || { cat /tmp/dockerd.log >&2; exit 1; }
    docker build -f docker/e2e-node.Dockerfile -t "$E2E_NODE_IMAGE" . >/dev/null
    exec "'$SCRIPT'"
  '
