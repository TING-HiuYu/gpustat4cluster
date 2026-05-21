#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR=""
SERVER_PID=""
BACKEND_PID=""

KCP_ENV_NAME="${GPUSTAT4CLUSTER_KCP_ENV_NAME:-GPUSTAT4CLUSTER_ENABLE_KCP}"
KCP_ENV_VALUE="${GPUSTAT4CLUSTER_KCP_ENV_VALUE:-1}"
COLLECTOR_ENV_NAME="${GPUSTAT4CLUSTER_KCP_COLLECTOR_ENV_NAME:-GPUSTAT4CLUSTER_COLLECTOR}"
COLLECTOR_ENV_VALUE="${GPUSTAT4CLUSTER_KCP_COLLECTOR_ENV_VALUE:-mock}"
FORCE_MOCK_ENV_NAME="${GPUSTAT4CLUSTER_KCP_FORCE_MOCK_ENV_NAME:-GPUSTAT4CLUSTER_FORCE_MOCK}"
FORCE_MOCK_ENV_VALUE="${GPUSTAT4CLUSTER_KCP_FORCE_MOCK_ENV_VALUE:-1}"
STATIC_NODES_ENV_NAME="${GPUSTAT4CLUSTER_KCP_STATIC_NODES_ENV_NAME:-GPUSTAT4CLUSTER_STATIC_NODES}"
FORCE_KCP_SMOKE="${GPUSTAT4CLUSTER_KCP_SMOKE_FORCE:-0}"
SERVER_QUERY_ADDR="${GPUSTAT4CLUSTER_KCP_QUERY_ADDR:-127.0.0.1:4722}"
CLIENT_BACKEND_ADDR="127.0.0.1:4521"
MOCK_HOSTNAME="${GPUSTAT4CLUSTER_KCP_MOCK_HOSTNAME:-mock-smoke-node}"
MOCK_UTIL="${GPUSTAT4CLUSTER_KCP_MOCK_UTIL:-87}"
MOCK_MEM_USED="${GPUSTAT4CLUSTER_KCP_MOCK_MEM_USED:-1234}"
MOCK_MEM_TOTAL="${GPUSTAT4CLUSTER_KCP_MOCK_MEM_TOTAL:-16384}"
SERVER_PORT_START="${GPUSTAT4CLUSTER_KCP_PORT_START:-39400}"
SERVER_PORT_END="${GPUSTAT4CLUSTER_KCP_PORT_END:-39410}"
MULTICAST_ADDR="${GPUSTAT4CLUSTER_KCP_MULTICAST_ADDR:-239.0.0.1:4600}"
STATIC_NODES="${GPUSTAT4CLUSTER_KCP_STATIC_NODES:-127.0.0.1:${SERVER_PORT_START}}"
PORT_RELEASE_TIMEOUT_SECS="${GPUSTAT4CLUSTER_KCP_PORT_RELEASE_TIMEOUT_SECS:-10}"

log() {
  printf '[kcp-smoke] %s\n' "$*"
}

skip() {
  printf '[kcp-smoke][skip] %s\n' "$*"
  exit 0
}

fail() {
  printf '[kcp-smoke][error] %s\n' "$*" >&2
  exit 1
}

cleanup() {
  local status=$?
  stop_pid "$BACKEND_PID"
  stop_pid "$SERVER_PID"
  reap_known_listener "$CLIENT_BACKEND_ADDR"
  reap_known_listener "$SERVER_QUERY_ADDR"
  reap_known_listener "127.0.0.1:${SERVER_PORT_START}"
  wait_until_tcp_free "$CLIENT_BACKEND_ADDR" "$PORT_RELEASE_TIMEOUT_SECS" >/dev/null 2>&1 || true
  wait_until_tcp_free "$SERVER_QUERY_ADDR" "$PORT_RELEASE_TIMEOUT_SECS" >/dev/null 2>&1 || true
  wait_until_tcp_free "127.0.0.1:${SERVER_PORT_START}" "$PORT_RELEASE_TIMEOUT_SECS" >/dev/null 2>&1 || true
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

kcp_switch_available() {
  grep -R "$KCP_ENV_NAME" -n "$ROOT_DIR/crates" >/dev/null 2>&1
}

collector_switch_available() {
  grep -R "$COLLECTOR_ENV_NAME" -n "$ROOT_DIR/crates" >/dev/null 2>&1
}

force_mock_switch_available() {
  grep -R "$FORCE_MOCK_ENV_NAME" -n "$ROOT_DIR/crates" >/dev/null 2>&1
}

static_nodes_switch_available() {
  grep -R "$STATIC_NODES_ENV_NAME" -n "$ROOT_DIR/crates" >/dev/null 2>&1
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
  cat >"$path" <<EOF
[connecting]
port_range = [$SERVER_PORT_START, $SERVER_PORT_END]
multicast_addr = "$MULTICAST_ADDR"
protocol = "kcp" # or "tcp"
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
# Optional: UDS path for client frontend <-> client-backend.
# uds_path = "/run/gpustat4cluster/client.sock"
EOF
}

build_binaries() {
  load_rust_module_if_needed
  require_cmd cargo

  log "building KCP feature debug binaries with locked dependency graph"
  (
    cd "$ROOT_DIR"
    cargo build --locked -p server --features "kcp-transport mock-nvml"
    cargo build --locked -p gpustat4cluster-client-backend --features kcp-transport
    cargo build --locked -p gpustat4cluster-client-cli
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

run_with_kcp_env() {
  env \
    "$KCP_ENV_NAME=$KCP_ENV_VALUE" \
    "$COLLECTOR_ENV_NAME=$COLLECTOR_ENV_VALUE" \
    "$FORCE_MOCK_ENV_NAME=$FORCE_MOCK_ENV_VALUE" \
    "$STATIC_NODES_ENV_NAME=$STATIC_NODES" \
    HOSTNAME="$MOCK_HOSTNAME" \
    "$@"
}

main() {
  require_cmd bash
  require_cmd python3

  if [[ "$FORCE_KCP_SMOKE" != "1" ]] && ! kcp_switch_available; then
    skip "KCP env switch '$KCP_ENV_NAME' is not present in crates yet; set GPUSTAT4CLUSTER_KCP_SMOKE_FORCE=1 to force-run"
  fi
  if [[ "$FORCE_KCP_SMOKE" != "1" ]] && ! collector_switch_available; then
    skip "mock collector env '$COLLECTOR_ENV_NAME' is not present in crates yet; set GPUSTAT4CLUSTER_KCP_SMOKE_FORCE=1 to force-run"
  fi
  if [[ "$FORCE_KCP_SMOKE" != "1" ]] && ! force_mock_switch_available; then
    skip "force mock env '$FORCE_MOCK_ENV_NAME' is not present in crates yet; set GPUSTAT4CLUSTER_KCP_SMOKE_FORCE=1 to force-run"
  fi
  if [[ "$FORCE_KCP_SMOKE" != "1" ]] && ! static_nodes_switch_available; then
    skip "static nodes env '$STATIC_NODES_ENV_NAME' is not present in crates yet; set GPUSTAT4CLUSTER_KCP_SMOKE_FORCE=1 to force-run"
  fi

  TMP_DIR="$(mktemp -d)"
  local config_path="$TMP_DIR/config.toml"
  local server_log="$TMP_DIR/server.log"
  local backend_log="$TMP_DIR/client-backend.log"
  local server_query="$TMP_DIR/server-query.json"
  local cli_output="$TMP_DIR/cli-output.txt"

  write_config "$config_path"
  build_binaries

  assert_port_free "$SERVER_QUERY_ADDR"
  assert_port_free "$CLIENT_BACKEND_ADDR"

  log "starting server with $KCP_ENV_NAME=$KCP_ENV_VALUE, $COLLECTOR_ENV_NAME=$COLLECTOR_ENV_VALUE, and $FORCE_MOCK_ENV_NAME=$FORCE_MOCK_ENV_VALUE"
  (
    cd "$ROOT_DIR"
    run_with_kcp_env \
      GPUSTAT4CLUSTER_CONFIG="$config_path" \
      GPUSTAT4CLUSTER_QUERY_ADDR="$SERVER_QUERY_ADDR" \
      GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING=1 \
      target/debug/server
  ) >"$server_log" 2>&1 &
  SERVER_PID=$!

  wait_for_tcp "$SERVER_QUERY_ADDR" 10 || {
    cat "$server_log" >&2 || true
    fail "server query port did not become ready: $SERVER_QUERY_ADDR"
  }

  query_server_once "$server_query"
  if ! grep -Eq '"ok":(true|false)' "$server_query"; then
    cat "$server_query" >&2 || true
    fail "server query response did not contain JSON ok field"
  fi

  log "starting client-backend with $KCP_ENV_NAME=$KCP_ENV_VALUE and $STATIC_NODES_ENV_NAME=$STATIC_NODES"
  (
    cd "$ROOT_DIR"
    run_with_kcp_env \
      GPUSTAT4CLUSTER_CONFIG="$config_path" \
      target/debug/gpustat4cluster-client-backend
  ) >"$backend_log" 2>&1 &
  BACKEND_PID=$!

  wait_for_tcp "$CLIENT_BACKEND_ADDR" 10 || {
    cat "$backend_log" >&2 || true
    fail "client-backend port did not become ready: $CLIENT_BACKEND_ADDR"
  }

  log "running CLI against KCP-enabled loopback backend"
  (
    cd "$ROOT_DIR"
    GPUSTAT4CLUSTER_BACKEND_ADDR="$CLIENT_BACKEND_ADDR" target/debug/gpustat4cluster
  ) >"$cli_output" 2>&1 || {
    cat "$cli_output" >&2 || true
    fail "CLI command failed"
  }

  if ! grep -q "$MOCK_HOSTNAME" "$cli_output"; then
    cat "$server_query" >&2 || true
    cat "$cli_output" >&2 || true
    if grep -q '"ok":false' "$server_query"; then
      fail "KCP loopback reached degraded server response but did not render required mock hostname"
    fi
    fail "CLI output did not contain mock hostname: $MOCK_HOSTNAME"
  fi
  if ! grep -Eq "${MOCK_UTIL}[[:space:]]*%" "$cli_output" \
    || ! grep -q "$MOCK_MEM_USED" "$cli_output" \
    || ! grep -q "$MOCK_MEM_TOTAL" "$cli_output"; then
    cat "$server_query" >&2 || true
    cat "$cli_output" >&2 || true
    if grep -q '"ok":false' "$server_query"; then
      fail "KCP loopback reached degraded server response but mock row is now required"
    fi
    fail "CLI output did not contain expected mock GPU utilization/memory row"
  fi
  log "KCP loopback rendered required mock GPU row"

  log "server query response: $(tr -d '\n' <"$server_query")"
  log "CLI output:"
  cat "$cli_output"
  log "KCP loopback smoke passed"
}

main "$@"
