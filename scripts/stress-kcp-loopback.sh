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
SERVER_QUERY_ADDR="${GPUSTAT4CLUSTER_STRESS_QUERY_ADDR:-127.0.0.1:4822}"
CLIENT_BACKEND_ADDR="127.0.0.1:4521"
MOCK_HOSTNAME="${GPUSTAT4CLUSTER_STRESS_MOCK_HOSTNAME:-mock-smoke-node}"
MOCK_UTIL="${GPUSTAT4CLUSTER_STRESS_MOCK_UTIL:-87}"
MOCK_MEM_USED="${GPUSTAT4CLUSTER_STRESS_MOCK_MEM_USED:-1234}"
MOCK_MEM_TOTAL="${GPUSTAT4CLUSTER_STRESS_MOCK_MEM_TOTAL:-16384}"
SERVER_PORT_START="${GPUSTAT4CLUSTER_STRESS_PORT_START:-39600}"
SERVER_PORT_END="${GPUSTAT4CLUSTER_STRESS_PORT_END:-39610}"
MULTICAST_ADDR="${GPUSTAT4CLUSTER_STRESS_MULTICAST_ADDR:-239.0.0.1:4800}"
STATIC_NODES="${GPUSTAT4CLUSTER_STRESS_STATIC_NODES:-127.0.0.1:${SERVER_PORT_START}}"
CONCURRENCY="${GPUSTAT4CLUSTER_STRESS_CONCURRENCY:-8}"
REQUESTS="${GPUSTAT4CLUSTER_STRESS_REQUESTS:-32}"
PORT_RELEASE_TIMEOUT_SECS="${GPUSTAT4CLUSTER_STRESS_PORT_RELEASE_TIMEOUT_SECS:-20}"

log() {
  printf '[kcp-stress] %s\n' "$*"
}

fail() {
  printf '[kcp-stress][error] %s\n' "$*" >&2
  exit 1
}

cleanup() {
  local status=$?
  stop_pid "$BACKEND_PID"
  stop_pid "$SERVER_PID"
  reap_known_listener "$CLIENT_BACKEND_ADDR"
  reap_known_listener "$SERVER_QUERY_ADDR"
  reap_known_listener "127.0.0.1:${SERVER_PORT_START}"
  wait_until_tcp_free "$CLIENT_BACKEND_ADDR" 5 >/dev/null 2>&1 || true
  wait_until_tcp_free "$SERVER_QUERY_ADDR" 5 >/dev/null 2>&1 || true
  wait_until_tcp_free "127.0.0.1:${SERVER_PORT_START}" 5 >/dev/null 2>&1 || true
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

require_port_free_after_wait() {
  local addr="$1"
  local timeout_secs="$2"
  log "waiting for TCP address to be free: $addr"
  reap_known_listener "$addr"
  if ! wait_until_tcp_free "$addr" "$timeout_secs"; then
    reap_known_listener "$addr"
  fi
  if ! wait_until_tcp_free "$addr" 5; then
    fail "TCP address still in use after ${timeout_secs}s: $addr"
  fi
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
discover_wait_secs = 1
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

run_with_env() {
  env \
    "$KCP_ENV_NAME=$KCP_ENV_VALUE" \
    "$COLLECTOR_ENV_NAME=$COLLECTOR_ENV_VALUE" \
    "$FORCE_MOCK_ENV_NAME=$FORCE_MOCK_ENV_VALUE" \
    "$STATIC_NODES_ENV_NAME=$STATIC_NODES" \
    HOSTNAME="$MOCK_HOSTNAME" \
    "$@"
}

run_one_query() {
  local idx="$1"
  local output="$TMP_DIR/query-${idx}.out"
  (
    cd "$ROOT_DIR"
    GPUSTAT4CLUSTER_BACKEND_ADDR="$CLIENT_BACKEND_ADDR" target/debug/gpustat4cluster
  ) >"$output" 2>&1 || return 1

  grep -q "$MOCK_HOSTNAME" "$output" \
    && grep -Eq "${MOCK_UTIL}[[:space:]]*%" "$output" \
    && grep -q "$MOCK_MEM_USED" "$output" \
    && grep -q "$MOCK_MEM_TOTAL" "$output"
}

run_query_batch() {
  local success_file="$TMP_DIR/success.count"
  local failure_file="$TMP_DIR/failure.count"
  : >"$success_file"
  : >"$failure_file"

  local active=0
  local idx
  for ((idx = 1; idx <= REQUESTS; idx += 1)); do
    (
      if run_one_query "$idx"; then
        printf '1\n' >>"$success_file"
      else
        printf '1\n' >>"$failure_file"
      fi
    ) &
    active=$((active + 1))

    if (( active >= CONCURRENCY )); then
      wait -n
      active=$((active - 1))
    fi
  done

  while (( active > 0 )); do
    wait -n
    active=$((active - 1))
  done
}

main() {
  require_cmd bash

  if ! [[ "$CONCURRENCY" =~ ^[0-9]+$ ]] || (( CONCURRENCY < 1 )); then
    fail "GPUSTAT4CLUSTER_STRESS_CONCURRENCY must be a positive integer"
  fi
  if ! [[ "$REQUESTS" =~ ^[0-9]+$ ]] || (( REQUESTS < 1 )); then
    fail "GPUSTAT4CLUSTER_STRESS_REQUESTS must be a positive integer"
  fi

  TMP_DIR="$(mktemp -d)"
  local config_path="$TMP_DIR/config.toml"
  local server_log="$TMP_DIR/server.log"
  local backend_log="$TMP_DIR/client-backend.log"

  write_config "$config_path"
  build_binaries

  require_port_free_after_wait "$SERVER_QUERY_ADDR" "$PORT_RELEASE_TIMEOUT_SECS"
  require_port_free_after_wait "$CLIENT_BACKEND_ADDR" "$PORT_RELEASE_TIMEOUT_SECS"

  log "starting KCP server with mock collector"
  (
    cd "$ROOT_DIR"
    run_with_env \
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

  log "starting KCP client-backend with static nodes: $STATIC_NODES"
  (
    cd "$ROOT_DIR"
    run_with_env \
      GPUSTAT4CLUSTER_CONFIG="$config_path" \
      target/debug/gpustat4cluster-client-backend
  ) >"$backend_log" 2>&1 &
  BACKEND_PID=$!

  wait_for_tcp "$CLIENT_BACKEND_ADDR" 10 || {
    cat "$backend_log" >&2 || true
    fail "client-backend port did not become ready: $CLIENT_BACKEND_ADDR"
  }

  log "running $REQUESTS CLI queries with concurrency=$CONCURRENCY"
  local start_ns
  local end_ns
  start_ns="$(date +%s%N)"
  run_query_batch
  end_ns="$(date +%s%N)"

  local success
  local failure
  success="$(wc -l <"$TMP_DIR/success.count")"
  failure="$(wc -l <"$TMP_DIR/failure.count")"
  local elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))

  log "summary requests=$REQUESTS concurrency=$CONCURRENCY success=$success failure=$failure elapsed_ms=$elapsed_ms"

  if (( failure != 0 )); then
    local sample
    sample="$(find "$TMP_DIR" -name 'query-*.out' -type f | sort | head -n 1)"
    if [[ -n "$sample" ]]; then
      log "sample query output from $sample:"
      cat "$sample"
    fi
    fail "stress baseline had $failure failed query or assertion(s)"
  fi

  log "stress baseline passed"
}

main "$@"
