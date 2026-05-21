#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR=""
SERVER_PIDS=()
BACKEND_PIDS=()

KCP_ENV_NAME="${GPUSTAT4CLUSTER_KCP_ENV_NAME:-GPUSTAT4CLUSTER_ENABLE_KCP}"
KCP_ENV_VALUE="${GPUSTAT4CLUSTER_KCP_ENV_VALUE:-1}"
COLLECTOR_ENV_NAME="${GPUSTAT4CLUSTER_KCP_COLLECTOR_ENV_NAME:-GPUSTAT4CLUSTER_COLLECTOR}"
COLLECTOR_ENV_VALUE="${GPUSTAT4CLUSTER_KCP_COLLECTOR_ENV_VALUE:-mock}"
FORCE_MOCK_ENV_NAME="${GPUSTAT4CLUSTER_KCP_FORCE_MOCK_ENV_NAME:-GPUSTAT4CLUSTER_FORCE_MOCK}"
FORCE_MOCK_ENV_VALUE="${GPUSTAT4CLUSTER_KCP_FORCE_MOCK_ENV_VALUE:-1}"
STATIC_NODES_ENV_NAME="${GPUSTAT4CLUSTER_KCP_STATIC_NODES_ENV_NAME:-GPUSTAT4CLUSTER_STATIC_NODES}"
BACKEND_ADDR_ENV_NAME="${GPUSTAT4CLUSTER_BACKEND_ADDR_ENV_NAME:-GPUSTAT4CLUSTER_BACKEND_ADDR}"
FORCE_MULTI_SMOKE="${GPUSTAT4CLUSTER_MULTINODE_SMOKE_FORCE:-0}"

BACKEND_A_ADDR="${GPUSTAT4CLUSTER_MULTINODE_BACKEND_A_ADDR:-127.0.0.1:4523}"
BACKEND_B_ADDR="${GPUSTAT4CLUSTER_MULTINODE_BACKEND_B_ADDR:-127.0.0.1:4524}"
SERVER_QUERY_ADDRS=(
  "${GPUSTAT4CLUSTER_MULTINODE_QUERY_ADDR_1:-127.0.0.1:4922}"
  "${GPUSTAT4CLUSTER_MULTINODE_QUERY_ADDR_2:-127.0.0.1:4923}"
  "${GPUSTAT4CLUSTER_MULTINODE_QUERY_ADDR_3:-127.0.0.1:4924}"
)
SERVER_PORTS=(
  "${GPUSTAT4CLUSTER_MULTINODE_PORT_1:-39800}"
  "${GPUSTAT4CLUSTER_MULTINODE_PORT_2:-39820}"
  "${GPUSTAT4CLUSTER_MULTINODE_PORT_3:-39840}"
)
SERVER_HOSTNAMES=(
  "${GPUSTAT4CLUSTER_MULTINODE_HOST_1:-mn-node-a}"
  "${GPUSTAT4CLUSTER_MULTINODE_HOST_2:-mn-node-b}"
  "${GPUSTAT4CLUSTER_MULTINODE_HOST_3:-mn-node-c}"
)
SERVER_GPU_COUNTS=(
  "${GPUSTAT4CLUSTER_MULTINODE_GPU_COUNT_1:-1}"
  "${GPUSTAT4CLUSTER_MULTINODE_GPU_COUNT_2:-2}"
  "${GPUSTAT4CLUSTER_MULTINODE_GPU_COUNT_3:-3}"
)
MULTICAST_BASE="${GPUSTAT4CLUSTER_MULTINODE_MULTICAST_BASE:-239.0.0}"
PORT_RELEASE_TIMEOUT_SECS="${GPUSTAT4CLUSTER_MULTINODE_PORT_RELEASE_TIMEOUT_SECS:-10}"

log() { printf '[multinode-smoke] %s\n' "$*"; }
skip() { printf '[multinode-smoke][skip] %s\n' "$*"; exit 0; }
fail() { printf '[multinode-smoke][error] %s\n' "$*" >&2; exit 1; }

cleanup() {
  local status=$?
  if (( status != 0 )); then dump_logs; fi
  local pid addr port
  for pid in "${BACKEND_PIDS[@]:-}"; do stop_pid "$pid"; done
  for pid in "${SERVER_PIDS[@]:-}"; do stop_pid "$pid"; done
  for addr in "$BACKEND_A_ADDR" "$BACKEND_B_ADDR" "${SERVER_QUERY_ADDRS[@]}"; do
    reap_known_listener "$addr"
    wait_until_tcp_free "$addr" "$PORT_RELEASE_TIMEOUT_SECS" >/dev/null 2>&1 || true
  done
  for port in "${SERVER_PORTS[@]}"; do reap_known_udp_listener "$port"; done
  [[ -n "$TMP_DIR" ]] && rm -rf "$TMP_DIR"
  exit "$status"
}
trap cleanup EXIT INT TERM

require_cmd() { command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"; }

stop_pid() {
  local pid="${1:-}"
  [[ -n "$pid" ]] && kill -0 "$pid" >/dev/null 2>&1 || return 0
  kill "$pid" >/dev/null 2>&1 || true
  for _ in 1 2 3 4 5; do
    if ! kill -0 "$pid" >/dev/null 2>&1; then wait "$pid" >/dev/null 2>&1 || true; return 0; fi
    sleep 0.2
  done
  kill -KILL "$pid" >/dev/null 2>&1 || true
  wait "$pid" >/dev/null 2>&1 || true
}

split_host_port() { HOST="${1%:*}"; PORT="${1##*:}"; }

wait_for_tcp() {
  local addr="$1" timeout="${2:-10}" deadline
  deadline=$((SECONDS + timeout))
  split_host_port "$addr"
  while (( SECONDS < deadline )); do
    if (echo >"/dev/tcp/${HOST}/${PORT}") >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  return 1
}

wait_until_tcp_free() {
  local addr="$1" timeout="${2:-10}" deadline
  deadline=$((SECONDS + timeout))
  split_host_port "$addr"
  while (( SECONDS < deadline )); do
    if ! (echo >"/dev/tcp/${HOST}/${PORT}") >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  return 1
}

reap_known_listener() {
  local addr="$1"
  split_host_port "$addr"
  command -v lsof >/dev/null 2>&1 || return 0
  local pid owner args
  for pid in $(lsof -nP -tiTCP:"$PORT" -sTCP:LISTEN 2>/dev/null); do
    owner="$(ps -o user= -p "$pid" 2>/dev/null | awk '{print $1}')"
    args="$(ps -o args= -p "$pid" 2>/dev/null || true)"
    [[ "$owner" == "$USER" ]] || continue
    case "$args" in *"target/debug/server"*|*"target/debug/gpustat4cluster-client-backend"*) stop_pid "$pid" ;; esac
  done
}

reap_known_udp_listener() {
  local port="$1"
  command -v lsof >/dev/null 2>&1 || return 0
  local pid owner args
  for pid in $(lsof -nP -tiUDP:"$port" 2>/dev/null); do
    owner="$(ps -o user= -p "$pid" 2>/dev/null | awk '{print $1}')"
    args="$(ps -o args= -p "$pid" 2>/dev/null || true)"
    [[ "$owner" == "$USER" ]] || continue
    case "$args" in *"target/debug/server"*|*"target/debug/gpustat4cluster-client-backend"*) stop_pid "$pid" ;; esac
  done
}

require_port_free_after_wait() {
  local addr="$1"
  reap_known_listener "$addr"
  wait_until_tcp_free "$addr" "$PORT_RELEASE_TIMEOUT_SECS" || { reap_known_listener "$addr"; wait_until_tcp_free "$addr" 5; } \
    || fail "TCP address still in use: $addr"
}

load_rust_module_if_needed() {
  command -v cargo >/dev/null 2>&1 && return 0
  if [[ -f /opt/shell_related/z00_lmod.sh ]]; then
    # shellcheck disable=SC1091
    source /opt/shell_related/z00_lmod.sh
    module load compiler/rust
  fi
}

switch_available() { grep -R "$1" -n "$ROOT_DIR/crates" >/dev/null 2>&1; }

build_binaries() {
  load_rust_module_if_needed
  require_cmd cargo
  log "building KCP + mock NVML debug binaries"
  (
    cd "$ROOT_DIR"
    cargo build --locked -p server --features "kcp-transport mock-nvml"
    cargo build --locked -p gpustat4cluster-client-backend --features kcp-transport
    cargo build --locked -p gpustat4cluster-client-cli
  )
}

write_config() {
  local path="$1" port_start="$2" multicast_addr="$3"
  cat >"$path" <<CONFIGEOF
[connecting]
port_range = [$port_start, $port_start]
multicast_addr = "$multicast_addr"
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
CONFIGEOF
}

start_server() {
  local index="$1" hostname query_addr port config_path log_file multicast_addr
  hostname="${SERVER_HOSTNAMES[$index]}"
  query_addr="${SERVER_QUERY_ADDRS[$index]}"
  port="${SERVER_PORTS[$index]}"
  config_path="$TMP_DIR/server-${index}.toml"
  log_file="$TMP_DIR/server-${index}.log"
  multicast_addr="${MULTICAST_BASE}.$((70 + index)):49$((30 + index))"

  write_config "$config_path" "$port" "$multicast_addr"
  require_port_free_after_wait "$query_addr"
  reap_known_udp_listener "$port"
  log "starting server $hostname query=$query_addr kcp=127.0.0.1:$port gpu_count=${SERVER_GPU_COUNTS[$index]}"
  (
    cd "$ROOT_DIR"
    env \
      "$KCP_ENV_NAME=$KCP_ENV_VALUE" \
      "$COLLECTOR_ENV_NAME=$COLLECTOR_ENV_VALUE" \
      "$FORCE_MOCK_ENV_NAME=$FORCE_MOCK_ENV_VALUE" \
      HOSTNAME="$hostname" \
      GPUSTAT4CLUSTER_MOCK_HOSTNAME="$hostname" \
      GPUSTAT4CLUSTER_MOCK_GPU_COUNT="${SERVER_GPU_COUNTS[$index]}" \
      GPUSTAT4CLUSTER_CONFIG="$config_path" \
      GPUSTAT4CLUSTER_QUERY_ADDR="$query_addr" \
      GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING=1 \
      target/debug/server
  ) >"$log_file" 2>&1 &
  SERVER_PIDS+=("$!")
  wait_for_tcp "$query_addr" 10 || fail "server query port did not become ready: $query_addr"
}

start_backend() {
  local name="$1"
  local addr="$2"
  local static_nodes="$3"
  local log_file="$TMP_DIR/${name}.log"
  local config_path="$TMP_DIR/${name}.toml"
  write_config "$config_path" 39980 "239.0.2.70:5030"
  require_port_free_after_wait "$addr"
  log "starting client-backend $name addr=$addr static_nodes=$static_nodes"
  (
    cd "$ROOT_DIR"
    env \
      "$KCP_ENV_NAME=$KCP_ENV_VALUE" \
      "$STATIC_NODES_ENV_NAME=$static_nodes" \
      "$BACKEND_ADDR_ENV_NAME=$addr" \
      GPUSTAT4CLUSTER_CONFIG="$config_path" \
      target/debug/gpustat4cluster-client-backend
  ) >"$log_file" 2>&1 &
  BACKEND_PIDS+=("$!")
  wait_for_tcp "$addr" 15 || fail "client-backend did not become ready: $addr"
}

run_cli_json() {
  local label="$1"
  local addr="$2"
  local output="$TMP_DIR/${label}.json"
  (cd "$ROOT_DIR" && target/debug/gpustat4cluster --backend-addr "$addr" --json) >"$output" 2>&1
  printf '%s\n' "$output"
}

run_cli_table() {
  local label="$1"
  local addr="$2"
  local output="$TMP_DIR/${label}.table"
  (cd "$ROOT_DIR" && target/debug/gpustat4cluster --backend-addr "$addr") >"$output" 2>&1
  printf '%s\n' "$output"
}

assert_json() {
  local output="$1" expected_count="$2"
  shift 2
  python3 - "$output" "$expected_count" "$@" <<'PY'
import json
import sys

path = sys.argv[1]
expected_count = int(sys.argv[2])
expected_hosts = sys.argv[3:]
with open(path, encoding="utf-8") as fh:
    data = json.load(fh)
assert data["meta"]["node_count"] == expected_count, data
nodes = data["nodes"]
assert len(nodes) == expected_count, data
by_host = {node["hostname"]: node for node in nodes}
for host in expected_hosts:
    assert host in by_host, by_host.keys()
    node = by_host[host]
    assert node["stale"] is False, node
    assert node["error"] is None, node
    assert node["gpus"], node
    for idx, gpu in enumerate(node["gpus"]):
        assert gpu["index"] == idx, gpu
        assert 0 <= gpu["util"] <= 100, gpu
        assert gpu["mem_used_mb"] > 0, gpu
        assert gpu["mem_total_mb"] >= gpu["mem_used_mb"], gpu
        assert gpu["processes"], gpu
        for proc in gpu["processes"]:
            assert proc["username"].startswith(("mock-user-", "mock-helper-")), proc
            assert proc["pid"] > 0, proc
            assert proc["used_memory_mb"] > 0, proc
# Mock GPU count is varied by node so this also verifies per-server env shaping.
assert len(by_host["mn-node-a"]["gpus"]) == 1, by_host["mn-node-a"]
if "mn-node-b" in by_host:
    assert len(by_host["mn-node-b"]["gpus"]) == 2, by_host["mn-node-b"]
if "mn-node-c" in by_host:
    assert len(by_host["mn-node-c"]["gpus"]) == 3, by_host["mn-node-c"]
PY
}

assert_table() {
  local output="$1"
  shift
  grep -q 'HOSTNAME' "$output" || fail "table output missing HOSTNAME: $output"
  local token
  for token in "$@"; do
    grep -q "$token" "$output" || fail "table output missing '$token': $output"
  done
}

dump_logs() {
  [[ -n "$TMP_DIR" && -d "$TMP_DIR" ]] || return 0
  local file
  for file in "$TMP_DIR"/*.log; do
    [[ -f "$file" ]] || continue
    printf '\n[multinode-smoke][log] %s\n' "$file" >&2
    tail -n 100 "$file" >&2 || true
  done
}

main() {
  require_cmd bash
  require_cmd python3
  if [[ "$FORCE_MULTI_SMOKE" != "1" ]] && ! switch_available "$KCP_ENV_NAME"; then skip "KCP env switch '$KCP_ENV_NAME' is not present in crates yet"; fi
  if [[ "$FORCE_MULTI_SMOKE" != "1" ]] && ! switch_available "$COLLECTOR_ENV_NAME"; then skip "mock collector env '$COLLECTOR_ENV_NAME' is not present in crates yet"; fi
  if [[ "$FORCE_MULTI_SMOKE" != "1" ]] && ! switch_available "$FORCE_MOCK_ENV_NAME"; then skip "force mock env '$FORCE_MOCK_ENV_NAME' is not present in crates yet"; fi
  if [[ "$FORCE_MULTI_SMOKE" != "1" ]] && ! switch_available "$STATIC_NODES_ENV_NAME"; then skip "static nodes env '$STATIC_NODES_ENV_NAME' is not present in crates yet"; fi
  if [[ "$FORCE_MULTI_SMOKE" != "1" ]] && ! switch_available "$BACKEND_ADDR_ENV_NAME"; then skip "backend addr env '$BACKEND_ADDR_ENV_NAME' is not present in crates yet"; fi

  TMP_DIR="$(mktemp -d)"
  build_binaries

  local i all_static subset_static cli_all_json cli_all_table cli_subset_json cli_subset_table
  for i in 0 1 2; do start_server "$i"; done
  # Give KCP listener tasks a brief chance to finish binding after TCP query readiness.
  sleep 0.5

  all_static="127.0.0.1:${SERVER_PORTS[0]},127.0.0.1:${SERVER_PORTS[1]},127.0.0.1:${SERVER_PORTS[2]}"
  subset_static="127.0.0.1:${SERVER_PORTS[0]},127.0.0.1:${SERVER_PORTS[1]}"
  start_backend backend-all "$BACKEND_A_ADDR" "$all_static"
  start_backend backend-subset "$BACKEND_B_ADDR" "$subset_static"

  cli_all_json="$(run_cli_json backend-all "$BACKEND_A_ADDR")"
  assert_json "$cli_all_json" 3 mn-node-a mn-node-b mn-node-c
  cli_all_table="$(run_cli_table backend-all "$BACKEND_A_ADDR")"
  assert_table "$cli_all_table" mn-node-a mn-node-b mn-node-c '87%' '80%' '73%' '1234/16384' '1746/17408' '2258/18432' 'proc mock-user-0' 'proc mock-user-1' 'proc mock-user-2'

  cli_subset_json="$(run_cli_json backend-subset "$BACKEND_B_ADDR")"
  assert_json "$cli_subset_json" 2 mn-node-a mn-node-b
  cli_subset_table="$(run_cli_table backend-subset "$BACKEND_B_ADDR")"
  assert_table "$cli_subset_table" mn-node-a mn-node-b '87%' '80%' '1234/16384' '1746/17408' 'proc mock-user-0' 'proc mock-user-1'

  log "verified real backend JSON node_count, hostnames, GPU rows, and process fields"
  log "verified real backend table output for all-node and subset-node clients"
  log "multinode local smoke passed"
}

main "$@"
