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

BACKEND_A_ADDR="${GPUSTAT4CLUSTER_MULTINODE_BACKEND_A_ADDR:-127.0.0.1:4533}"
BACKEND_B_ADDR="${GPUSTAT4CLUSTER_MULTINODE_BACKEND_B_ADDR:-127.0.0.1:4534}"
SERVER_QUERY_ADDRS=(127.0.0.1:4932 127.0.0.1:4933 127.0.0.1:4934)
SERVER_PORTS=(39900 39920 39940)
SERVER_HOSTNAMES=(mn-node-a mn-node-b mn-node-c)
SERVER_GPU_COUNTS=(1 2 3)
REQUESTS="${GPUSTAT4CLUSTER_MULTINODE_STRESS_REQUESTS:-40}"
CONCURRENCY="${GPUSTAT4CLUSTER_MULTINODE_STRESS_CONCURRENCY:-8}"
PORT_RELEASE_TIMEOUT_SECS="${GPUSTAT4CLUSTER_MULTINODE_PORT_RELEASE_TIMEOUT_SECS:-10}"

log() { printf '[multinode-stress] %s\n' "$*"; }
fail() { printf '[multinode-stress][error] %s\n' "$*" >&2; exit 1; }

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
  local addr="$1"; split_host_port "$addr"; command -v lsof >/dev/null 2>&1 || return 0
  local pid owner args
  for pid in $(lsof -nP -tiTCP:"$PORT" -sTCP:LISTEN 2>/dev/null); do
    owner="$(ps -o user= -p "$pid" 2>/dev/null | awk '{print $1}')"; args="$(ps -o args= -p "$pid" 2>/dev/null || true)"
    [[ "$owner" == "$USER" ]] || continue
    case "$args" in *"target/debug/server"*|*"target/debug/gpustat4cluster-client-backend"*) stop_pid "$pid" ;; esac
  done
}
reap_known_udp_listener() {
  local port="$1"; command -v lsof >/dev/null 2>&1 || return 0
  local pid owner args
  for pid in $(lsof -nP -tiUDP:"$port" 2>/dev/null); do
    owner="$(ps -o user= -p "$pid" 2>/dev/null | awk '{print $1}')"; args="$(ps -o args= -p "$pid" 2>/dev/null || true)"
    [[ "$owner" == "$USER" ]] || continue
    case "$args" in *"target/debug/server"*|*"target/debug/gpustat4cluster-client-backend"*) stop_pid "$pid" ;; esac
  done
}
require_port_free_after_wait() { reap_known_listener "$1"; wait_until_tcp_free "$1" "$PORT_RELEASE_TIMEOUT_SECS" || { reap_known_listener "$1"; wait_until_tcp_free "$1" 5; } || fail "TCP address still in use: $1"; }
load_rust_module_if_needed() { command -v cargo >/dev/null 2>&1 && return 0; if [[ -f /opt/shell_related/z00_lmod.sh ]]; then source /opt/shell_related/z00_lmod.sh; module load compiler/rust; fi; }

build_binaries() {
  load_rust_module_if_needed; require_cmd cargo
  log "building KCP + mock NVML debug binaries"
  (cd "$ROOT_DIR" && cargo build --locked -p server --features "kcp-transport mock-nvml" && cargo build --locked -p gpustat4cluster-client-backend --features kcp-transport && cargo build --locked -p gpustat4cluster-client-cli)
}
write_config() {
  local path="$1" port_start="$2" multicast="$3"
  cat >"$path" <<CONFIGEOF
[connecting]
port_range = [$port_start, $port_start]
multicast_addr = "$multicast"
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
  local i="$1"
  local config_path="$TMP_DIR/server-${i}.toml"
  local log_file="$TMP_DIR/server-${i}.log"
  local multicast="239.0.1.$((70 + i)):50$((30 + i))"
  write_config "$config_path" "${SERVER_PORTS[$i]}" "$multicast"
  require_port_free_after_wait "${SERVER_QUERY_ADDRS[$i]}"; reap_known_udp_listener "${SERVER_PORTS[$i]}"
  (cd "$ROOT_DIR" && env "$KCP_ENV_NAME=$KCP_ENV_VALUE" "$COLLECTOR_ENV_NAME=$COLLECTOR_ENV_VALUE" "$FORCE_MOCK_ENV_NAME=$FORCE_MOCK_ENV_VALUE" HOSTNAME="${SERVER_HOSTNAMES[$i]}" GPUSTAT4CLUSTER_MOCK_HOSTNAME="${SERVER_HOSTNAMES[$i]}" GPUSTAT4CLUSTER_MOCK_GPU_COUNT="${SERVER_GPU_COUNTS[$i]}" GPUSTAT4CLUSTER_CONFIG="$config_path" GPUSTAT4CLUSTER_QUERY_ADDR="${SERVER_QUERY_ADDRS[$i]}" GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING=1 target/debug/server) >"$log_file" 2>&1 &
  SERVER_PIDS+=("$!")
  wait_for_tcp "${SERVER_QUERY_ADDRS[$i]}" 10 || fail "server query port did not become ready: ${SERVER_QUERY_ADDRS[$i]}"
}
start_backend() {
  local name="$1"
  local addr="$2"
  local static_nodes="$3"
  local log_file="$TMP_DIR/${name}.log"
  local config_path="$TMP_DIR/${name}.toml"
  write_config "$config_path" 39980 "239.0.2.70:5030"
  require_port_free_after_wait "$addr"
  (cd "$ROOT_DIR" && env "$KCP_ENV_NAME=$KCP_ENV_VALUE" "$STATIC_NODES_ENV_NAME=$static_nodes" "$BACKEND_ADDR_ENV_NAME=$addr" GPUSTAT4CLUSTER_CONFIG="$config_path" target/debug/gpustat4cluster-client-backend) >"$log_file" 2>&1 &
  BACKEND_PIDS+=("$!")
  wait_for_tcp "$addr" 15 || fail "backend did not become ready: $addr"
}
run_cli_query() {
  local idx="$1"
  local addr="$2"
  local output="$TMP_DIR/query-${idx}.json"
  (cd "$ROOT_DIR" && target/debug/gpustat4cluster --backend-addr "$addr" --json) >"$output" 2>&1 || return 1
  python3 - "$output" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as fh: data = json.load(fh)
assert data["meta"]["node_count"] >= 2, data
hosts = {n["hostname"] for n in data["nodes"]}
assert "mn-node-a" in hosts and "mn-node-b" in hosts, hosts
text = json.dumps(data)
for token in ["mock-user-0", "87", "1234", "16384"]:
    assert token in text, token
PY
}
run_query_batch() {
  local success_file="$TMP_DIR/success.count" failure_file="$TMP_DIR/failure.count" active=0 idx addr
  : >"$success_file"; : >"$failure_file"
  for ((idx=1; idx<=REQUESTS; idx+=1)); do
    if (( idx % 2 == 0 )); then addr="$BACKEND_A_ADDR"; else addr="$BACKEND_B_ADDR"; fi
    (run_cli_query "$idx" "$addr" && printf '1\n' >>"$success_file" || printf '1\n' >>"$failure_file") &
    active=$((active + 1))
    if (( active >= CONCURRENCY )); then wait -n; active=$((active - 1)); fi
  done
  while (( active > 0 )); do wait -n; active=$((active - 1)); done
}
dump_logs() { [[ -n "$TMP_DIR" && -d "$TMP_DIR" ]] || return 0; local file; for file in "$TMP_DIR"/*.log; do [[ -f "$file" ]] || continue; printf '\n[multinode-stress][log] %s\n' "$file" >&2; tail -n 100 "$file" >&2 || true; done; }

main() {
  require_cmd bash; require_cmd python3
  [[ "$REQUESTS" =~ ^[0-9]+$ ]] && (( REQUESTS > 0 )) || fail "GPUSTAT4CLUSTER_MULTINODE_STRESS_REQUESTS must be positive"
  [[ "$CONCURRENCY" =~ ^[0-9]+$ ]] && (( CONCURRENCY > 0 )) || fail "GPUSTAT4CLUSTER_MULTINODE_STRESS_CONCURRENCY must be positive"
  TMP_DIR="$(mktemp -d)"; build_binaries
  local i all_static subset_static start_ns end_ns success failure elapsed_ms
  for i in 0 1 2; do start_server "$i"; done
  sleep 0.5
  all_static="127.0.0.1:${SERVER_PORTS[0]},127.0.0.1:${SERVER_PORTS[1]},127.0.0.1:${SERVER_PORTS[2]}"
  subset_static="127.0.0.1:${SERVER_PORTS[0]},127.0.0.1:${SERVER_PORTS[1]}"
  start_backend backend-all "$BACKEND_A_ADDR" "$all_static"
  start_backend backend-subset "$BACKEND_B_ADDR" "$subset_static"
  log "running $REQUESTS CLI --json queries across two real backends with concurrency=$CONCURRENCY"
  start_ns="$(date +%s%N)"; run_query_batch; end_ns="$(date +%s%N)"
  success="$(wc -l <"$TMP_DIR/success.count")"; failure="$(wc -l <"$TMP_DIR/failure.count")"; elapsed_ms=$(((end_ns - start_ns) / 1000000))
  log "summary requests=$REQUESTS concurrency=$CONCURRENCY success=$success failure=$failure elapsed_ms=$elapsed_ms"
  (( failure == 0 )) || fail "multinode stress had $failure failures"
  log "multinode stress baseline passed"
}
main "$@"
