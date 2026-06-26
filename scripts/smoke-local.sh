#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR=""
SERVER_PID=""
BACKEND_PID=""

SERVER_QUERY_ADDR="${GPUSTAT4CLUSTER_SMOKE_QUERY_ADDR:-127.0.0.1:4622}"
TEST_HOSTNAME="${GPUSTAT4CLUSTER_SMOKE_TEST_HOSTNAME:-test-smoke-node}"
SERVER_PORT_START="${GPUSTAT4CLUSTER_SMOKE_PORT_START:-39200}"
SERVER_PORT_END="${GPUSTAT4CLUSTER_SMOKE_PORT_END:-39210}"
MULTICAST_ADDR="${GPUSTAT4CLUSTER_SMOKE_MULTICAST_ADDR:-239.0.0.1:4400}"
PORT_RELEASE_TIMEOUT_SECS="${GPUSTAT4CLUSTER_SMOKE_PORT_RELEASE_TIMEOUT_SECS:-10}"
CLIENT_BACKEND_SOCKET=""

log() {
  printf '[smoke] %s\n' "$*"
}

fail() {
  printf '[smoke][error] %s\n' "$*" >&2
  exit 1
}

cleanup() {
  local status=$?
  stop_pid "$BACKEND_PID"
  stop_pid "$SERVER_PID"
  reap_known_listener "$SERVER_QUERY_ADDR"
  wait_until_tcp_free "$SERVER_QUERY_ADDR" "$PORT_RELEASE_TIMEOUT_SECS" >/dev/null 2>&1 || true
  if [[ -n "$CLIENT_BACKEND_SOCKET" ]]; then
    rm -f "$CLIENT_BACKEND_SOCKET"
  fi
  if [[ -n "$TMP_DIR" ]]; then
    rm -rf "$TMP_DIR"
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

stop_pid() {
  local pid="${1:-}"
  if [[ -z "$pid" ]] || ! kill -0 "$pid" >/dev/null 2>&1; then
    return 0
  fi

  kill "$pid" >/dev/null 2>&1 || true
  for _ in 1 2 3 4 5; do
    if ! kill -0 "$pid" >/dev/null 2>&1; then
      wait "$pid" >/dev/null 2>&1 || true
      return 0
    fi
    sleep 0.2
  done

  kill -KILL "$pid" >/dev/null 2>&1 || true
  wait "$pid" >/dev/null 2>&1 || true
}

reap_known_listener() {
  local addr="$1"
  split_host_port "$addr"
  command -v lsof >/dev/null 2>&1 || return 0

  local pid
  for pid in $(lsof -nP -tiTCP:"$PORT" -sTCP:LISTEN 2>/dev/null); do
    local owner
    local args
    owner="$(ps -o user= -p "$pid" 2>/dev/null | awk '{print $1}')"
    args="$(ps -o args= -p "$pid" 2>/dev/null || true)"
    [[ "$owner" == "$USER" ]] || continue
    case "$args" in
      *"target/debug/server"*|*"target/debug/gpustat4cluster-client-backend"*|*"python3 -"*)
        stop_pid "$pid"
        ;;
    esac
  done
}

load_rust_module_if_needed() {
  if command -v cargo >/dev/null 2>&1; then
    return 0
  fi

  if [[ -f /opt/shell_related/z00_lmod.sh ]]; then
    # shellcheck disable=SC1091
    source /opt/shell_related/z00_lmod.sh
    module load compiler/rust
  fi
}

split_host_port() {
  local addr="$1"
  HOST="${addr%:*}"
  PORT="${addr##*:}"
}

wait_for_tcp() {
  local addr="$1"
  local timeout_secs="$2"
  local deadline=$((SECONDS + timeout_secs))
  split_host_port "$addr"

  while (( SECONDS < deadline )); do
    if (echo >"/dev/tcp/${HOST}/${PORT}") >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done

  return 1
}

assert_port_free() {
  local addr="$1"
  if wait_for_tcp "$addr" 1; then
    fail "TCP address already in use: $addr"
  fi
}

wait_until_tcp_free() {
  local addr="$1"
  local timeout_secs="$2"
  local deadline=$((SECONDS + timeout_secs))
  split_host_port "$addr"

  while (( SECONDS < deadline )); do
    if ! (echo >"/dev/tcp/${HOST}/${PORT}") >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done

  return 1
}

write_config() {
  local path="$1"
  local uds_path="$2"
  cat >"$path" <<EOF
[connecting]
port_range = [$SERVER_PORT_START, $SERVER_PORT_END]
multicast_addr = "$MULTICAST_ADDR"
protocol = "udp" # or "tcp"
heartbeat_interval = 5
connection_idle_timeout = 10
max_connections = 64
discover_wait_secs = 3
multicast_retry_limit = 5
# Optional: one or more local IPv4 addresses used as multicast outbound interfaces.
# multicast_outbound_ip = ["192.0.2.10"]

[log]
max_size = "5mb"

[services]
cache_ttl_ms = 40
uds_path = "$uds_path"
EOF
}

build_binaries() {
  load_rust_module_if_needed
  require_cmd cargo

  log "building debug binaries"
  (
    cd "$ROOT_DIR"
    cargo build --locked -p server -p gpustat4cluster-client-backend -p gpustat4cluster-client-cli
  )
}

query_server_once() {
  local output_file="$1"
  split_host_port "$SERVER_QUERY_ADDR"

  python3 - "$HOST" "$PORT" >"$output_file" <<'PY'
import socket
import sys

host = sys.argv[1]
port = int(sys.argv[2])
with socket.create_connection((host, port), timeout=3) as sock:
    sock.sendall(b"PING\n")
    sock.settimeout(3)
    print(sock.recv(65536).decode("utf-8", "replace"))
PY
}

start_fake_backend() {
  local log_file="$1"
  local socket_path="$2"

  rm -f "$socket_path"
  python3 - "$socket_path" "$TEST_HOSTNAME" >"$log_file" 2>&1 <<'PY' &
import json
import os
import socket
import sys
import time

socket_path = sys.argv[1]
hostname = sys.argv[2]

response = {
    "nodes": [
        {
            "connection_id": "test-001",
            "hostname": hostname,
            "addr": "127.0.0.1:39999",
            "timestamp_ms": int(time.time() * 1000),
            "num": 1,
            "gres": [
                {
                    "index": 0,
                    "name": "NVIDIA Test GPU 0",
                    "util": 87,
                    "mem_used_mb": 1234,
                    "mem_total_mb": 16384,
                }
            ],
        }
    ]
}

server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(socket_path)
os.chmod(socket_path, 0o600)
server.listen(16)
while True:
    conn, _ = server.accept()
    with conn:
        data = conn.recv(4096)
        if data.startswith(b"QUERY"):
            conn.sendall((json.dumps(response) + "\n").encode("utf-8"))
        elif data.startswith(b"LIST"):
            conn.sendall(b"test-001 test-smoke-node 127.0.0.1:39999 0\n")
        else:
            conn.sendall(b"ERR unsupported command\n")
PY
  BACKEND_PID=$!
}

wait_for_uds() {
  local socket_path="$1"
  local timeout_secs="$2"
  local deadline=$((SECONDS + timeout_secs))

  while (( SECONDS < deadline )); do
    [[ -S "$socket_path" ]] && return 0
    sleep 0.2
  done

  return 1
}

main() {
  require_cmd bash
  require_cmd python3

  TMP_DIR="$(mktemp -d)"
  local config_path="$TMP_DIR/config.toml"
  local backend_log="$TMP_DIR/client-backend.log"
  local cli_output="$TMP_DIR/cli-output.txt"
  CLIENT_BACKEND_SOCKET="$TMP_DIR/client.sock"

  write_config "$config_path" "$CLIENT_BACKEND_SOCKET"
  build_binaries

  log "starting client-backend; multicast discovery may fall back to empty node list"
  (
    cd "$ROOT_DIR"
    GPUSTAT4CLUSTER_CONFIG="$config_path" \
      target/debug/gpustat4cluster-client-backend
  ) >"$backend_log" 2>&1 &
  BACKEND_PID=$!

  wait_for_uds "$CLIENT_BACKEND_SOCKET" 10 || {
    cat "$backend_log" >&2 || true
    fail "client-backend UDS did not become ready: $CLIENT_BACKEND_SOCKET"
  }

  log "client-backend UDS is ready"
  kill "$BACKEND_PID" >/dev/null 2>&1 || true
  wait "$BACKEND_PID" >/dev/null 2>&1 || true
  BACKEND_PID=""
  sleep 0.2
  rm -f "$CLIENT_BACKEND_SOCKET"

  log "starting fake UDS backend with deterministic test GRES row"
  start_fake_backend "$backend_log" "$CLIENT_BACKEND_SOCKET"
  wait_for_uds "$CLIENT_BACKEND_SOCKET" 10 || {
    cat "$backend_log" >&2 || true
    fail "fake backend UDS did not become ready: $CLIENT_BACKEND_SOCKET"
  }

  log "running CLI against local backend"
  (
    cd "$ROOT_DIR"
    GPUSTAT4CLUSTER_BACKEND_SOCKET="$CLIENT_BACKEND_SOCKET" \
      target/debug/gpustat4cluster
  ) >"$cli_output" 2>&1 || {
    cat "$cli_output" >&2 || true
    fail "CLI command failed"
  }

  if ! grep -q "$TEST_HOSTNAME" "$cli_output"; then
    cat "$cli_output" >&2 || true
    fail "CLI output did not contain test hostname: $TEST_HOSTNAME"
  fi
  if ! grep -q 'NVIDIA Test GPU 0' "$cli_output"; then
    cat "$cli_output" >&2 || true
    fail "CLI output did not contain test GRES name"
  fi
  if ! grep -Eq '[0-9]+ %' "$cli_output" || ! grep -Eq '1234[[:space:]]+/[[:space:]]+16384' "$cli_output"; then
    cat "$cli_output" >&2 || true
    fail "CLI output did not contain expected test GRES utilization/memory row"
  fi
  log "CLI rendered deterministic test GRES row"

  log "CLI output:"
  cat "$cli_output"
  log "smoke passed"
}

main "$@"
