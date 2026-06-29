#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVER_BIN="${SERVER_BIN:-$ROOT_DIR/target/debug/server}"
BACKEND_BIN="${BACKEND_BIN:-$ROOT_DIR/target/debug/gpustat4cluster-client-backend}"
CLIENT_BIN="${CLIENT_BIN:-$ROOT_DIR/target/debug/gpustat4cluster}"
E2E_TMP_ROOT="${E2E_TMP_ROOT:-$(mktemp -d)}"
E2E_PIDS=()
E2E_CONTAINERS=()
E2E_LAST_PID=""
E2E_RUN_ID="${E2E_RUN_ID:-$(basename "$E2E_TMP_ROOT" | tr -c 'A-Za-z0-9_.-' '-')}"
E2E_NODE_MODE="${E2E_NODE_MODE:-local}"
E2E_NODE_IMAGE="${E2E_NODE_IMAGE:-gpustat4cluster-e2e-node:local}"
E2E_DOCKER_NETWORK="${E2E_DOCKER_NETWORK:-}"
declare -A E2E_SERVER_BY_TCP=()
declare -A E2E_SERVER_BY_UDP=()
declare -A E2E_USED_PORTS=()

cleanup_e2e() {
  local pid
  for pid in "${E2E_PIDS[@]:-}"; do
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  wait "${E2E_PIDS[@]:-}" 2>/dev/null || true
  if [[ "$E2E_NODE_MODE" == "docker" ]]; then
    local container
    for container in "${E2E_CONTAINERS[@]:-}"; do
      docker rm -f "$container" >/dev/null 2>&1 || true
    done
    if [[ -n "$E2E_DOCKER_NETWORK" ]]; then
      docker network rm "$E2E_DOCKER_NETWORK" >/dev/null 2>&1 || true
    fi
  fi
  if [[ "${E2E_KEEP:-0}" != "1" ]]; then
    rm -rf "$E2E_TMP_ROOT"
  else
    echo "kept e2e temp dir: $E2E_TMP_ROOT" >&2
  fi
}
trap cleanup_e2e EXIT

require_binaries() {
  (cd "$ROOT_DIR" && cargo build --locked -p server --features test-collector && cargo build --locked -p gpustat4cluster-client-backend -p gpustat4cluster-client-cli)
  if [[ "$E2E_NODE_MODE" == "docker" ]]; then
    docker image inspect "$E2E_NODE_IMAGE" >/dev/null 2>&1 || {
      docker build -f "$ROOT_DIR/docker/e2e-node.Dockerfile" -t "$E2E_NODE_IMAGE" "$ROOT_DIR"
    }
    if [[ -z "$E2E_DOCKER_NETWORK" ]]; then
      E2E_DOCKER_NETWORK="g4c-e2e-$E2E_RUN_ID"
      docker network create "$E2E_DOCKER_NETWORK" >/dev/null
    elif ! docker network inspect "$E2E_DOCKER_NETWORK" >/dev/null 2>&1; then
      docker network create "$E2E_DOCKER_NETWORK" >/dev/null
    fi
  fi
}

alloc_ports() {
  local count="$1"
  local ports=()
  while ((${#ports[@]} < count)); do
    local candidate
    candidate="$(python3 - <<'PY'
import socket
while True:
    tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    tcp.bind(("0.0.0.0", 0))
    port = tcp.getsockname()[1]
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        udp.bind(("0.0.0.0", port))
    except OSError:
        tcp.close()
        udp.close()
        continue
    tcp.close()
    udp.close()
    print(port)
    break
PY
)"
    if [[ -z "${E2E_USED_PORTS[$candidate]:-}" ]]; then
      E2E_USED_PORTS["$candidate"]=1
      ports+=("$candidate")
    fi
  done
  echo "${ports[*]}"
}

write_inventory() {
  local path="$1" hostname="$2" seed="$3"
  python3 - "$path" "$hostname" "$seed" <<'PY'
import json, sys
path, hostname, seed = sys.argv[1], sys.argv[2], int(sys.argv[3])
mem_gib = [8, 16, 24, 36, 48, 80, 96]
count = seed % 8 + 1
gres = []
for idx in range(count):
    gres.append({
        "index": idx,
        "name": f"NVIDIA Test GPU {idx}",
        "uuid": f"GRES-E2E-{hostname.replace('.', '-')}-{idx:04d}",
        "memory_total_mb": mem_gib[(seed + idx * 3) % len(mem_gib)] * 1024,
    })
with open(path, "w", encoding="utf-8") as f:
    json.dump({"hostname": hostname, "driver_version": f"test-driver-{seed}", "gres": gres}, f)
print(count)
PY
}

write_inventory_count() {
  local path="$1" hostname="$2" count="$3" seed="$4"
  python3 - "$path" "$hostname" "$count" "$seed" <<'PY'
import json, sys
path, hostname, count, seed = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
mem_gib = [8, 16, 24, 36, 48, 80, 96]
gres = []
for idx in range(count):
    gres.append({
        "index": idx,
        "name": f"NVIDIA Test GPU {idx}",
        "uuid": f"GRES-E2E-{hostname.replace('.', '-')}-{idx:04d}",
        "memory_total_mb": mem_gib[(seed + idx * 3) % len(mem_gib)] * 1024,
    })
with open(path, "w", encoding="utf-8") as f:
    json.dump({"hostname": hostname, "driver_version": f"test-driver-{seed}", "gres": gres}, f)
print(count)
PY
}

write_server_config() {
  local path="$1" tcp_port="$2" udp_port="$3" multicast="$4" inventory="$5" runtime="$6"
  local reload="${7:-false}"
  local protocol="${8:-udp}"
  local container_name
  local outbound_ips='["127.0.0.1"]'
  if [[ "$E2E_NODE_MODE" == "docker" ]]; then
    outbound_ips='[]'
  fi
  container_name="$(server_container_name "$path")"
  E2E_SERVER_BY_TCP["$tcp_port"]="$container_name"
  E2E_SERVER_BY_UDP["$udp_port"]="$container_name"
  cat > "$path" <<EOF_CFG
[connecting]
port_range = [$tcp_port, $((tcp_port + 100))]
multicast_addr = "$multicast"
protocol = "$protocol"
tcp_port = $tcp_port
udp_port = $udp_port
udp_mtu = 0
heartbeat_interval = 1
connection_idle_timeout = 5
max_connections = 64
discover_wait_secs = 1
multicast_retry_limit = 2
multicast_outbound_ip = $outbound_ips

[log]
max_size = "5mb"

[services]
cache_ttl_ms = 10
collector_interval_ms = 5
latency_display = true

[runtime]
test_inventory_path = "$inventory"
test_runtime_path = "$runtime"
test_inventory_reload = $reload
EOF_CFG
}

write_client_config() {
  local path="$1" uds="$2" multicast="$3"
  local protocol="${4:-udp}"
  local outbound_ips='["127.0.0.1"]'
  if [[ "$E2E_NODE_MODE" == "docker" ]]; then
    outbound_ips='[]'
  fi
  cat > "$path" <<EOF_CFG
[connecting]
port_range = [30000, 40000]
multicast_addr = "$multicast"
protocol = "$protocol"
udp_mtu = 0
heartbeat_interval = 1
connection_idle_timeout = 5
max_connections = 64
discover_wait_secs = 1
multicast_retry_limit = 2
multicast_outbound_ip = $outbound_ips

[log]
max_size = "5mb"

[services]
cache_ttl_ms = 10
collector_interval_ms = 5
latency_display = true
uds_path = "$uds"
EOF_CFG
}

start_server() {
  local config="$1" log="$2"
  if [[ "$E2E_NODE_MODE" == "docker" ]]; then
    start_container "server" "$(server_container_name "$config")" "$log" GPUSTAT4CLUSTER_CONFIG="$config" "$SERVER_BIN"
    return
  fi
  GPUSTAT4CLUSTER_CONFIG="$config" "$SERVER_BIN" >"$log" 2>&1 &
  E2E_PIDS+=("$!")
}

start_server_pid() {
  local config="$1" log="$2"
  if [[ "$E2E_NODE_MODE" == "docker" ]]; then
    start_container "server" "$(server_container_name "$config")" "$log" GPUSTAT4CLUSTER_CONFIG="$config" "$SERVER_BIN"
    E2E_LAST_PID="$(server_container_name "$config")"
    return
  fi
  GPUSTAT4CLUSTER_CONFIG="$config" "$SERVER_BIN" >"$log" 2>&1 &
  local pid="$!"
  E2E_PIDS+=("$pid")
  E2E_LAST_PID="$pid"
}

start_backend() {
  local config="$1" static_nodes="$2" log="$3"
  if [[ "$E2E_NODE_MODE" == "docker" ]]; then
    local container_name translated_static_nodes
    container_name="$(backend_container_name "$config")"
    register_server_configs_from_tmp
    translated_static_nodes="$(translate_static_nodes "$static_nodes")"
    if [[ -n "$translated_static_nodes" ]]; then
      start_container "backend" "$container_name" "$log" GPUSTAT4CLUSTER_CONFIG="$config" GPUSTAT4CLUSTER_STATIC_NODES="$translated_static_nodes" "$BACKEND_BIN"
    else
      start_container "backend" "$container_name" "$log" GPUSTAT4CLUSTER_CONFIG="$config" "$BACKEND_BIN"
    fi
    return
  fi
  if [[ -n "$static_nodes" ]]; then
    GPUSTAT4CLUSTER_CONFIG="$config" GPUSTAT4CLUSTER_STATIC_NODES="$static_nodes" "$BACKEND_BIN" >"$log" 2>&1 &
  else
    GPUSTAT4CLUSTER_CONFIG="$config" "$BACKEND_BIN" >"$log" 2>&1 &
  fi
  E2E_PIDS+=("$!")
}


start_backend_pid() {
  local config="$1" static_nodes="$2" log="$3"
  if [[ "$E2E_NODE_MODE" == "docker" ]]; then
    local container_name translated_static_nodes
    container_name="$(backend_container_name "$config")"
    register_server_configs_from_tmp
    translated_static_nodes="$(translate_static_nodes "$static_nodes")"
    if [[ -n "$translated_static_nodes" ]]; then
      start_container "backend" "$container_name" "$log" GPUSTAT4CLUSTER_CONFIG="$config" GPUSTAT4CLUSTER_STATIC_NODES="$translated_static_nodes" "$BACKEND_BIN"
    else
      start_container "backend" "$container_name" "$log" GPUSTAT4CLUSTER_CONFIG="$config" "$BACKEND_BIN"
    fi
    E2E_LAST_PID="$container_name"
    return
  fi
  if [[ -n "$static_nodes" ]]; then
    GPUSTAT4CLUSTER_CONFIG="$config" GPUSTAT4CLUSTER_STATIC_NODES="$static_nodes" "$BACKEND_BIN" >"$log" 2>&1 &
  else
    GPUSTAT4CLUSTER_CONFIG="$config" "$BACKEND_BIN" >"$log" 2>&1 &
  fi
  local pid="$!"
  E2E_PIDS+=("$pid")
  E2E_LAST_PID="$pid"
}

write_expected_manifest() {
  local out="$1"
  shift
  python3 - "$out" "$@" <<'PY'
import json, sys
out = sys.argv[1]
nodes = []
for path in sys.argv[2:]:
    with open(path, encoding='utf-8') as f:
        node = json.load(f)
    nodes.append({'hostname': node.get('hostname'), 'driver_version': node.get('driver_version'), 'gres': node.get('gres', [])})
with open(out, 'w', encoding='utf-8') as f:
    json.dump({'nodes': sorted(nodes, key=lambda n: n.get('hostname') or '')}, f, sort_keys=True)
PY
}

compare_inventory_json() {
  local obtained="$1" expected="$2"
  python3 - "$obtained" "$expected" <<'PY'
import json, sys
obtained_path, expected_path = sys.argv[1], sys.argv[2]
def norm_gres(gres):
    normalized = []
    for g in gres or []:
        normalized.append({
            'index': g.get('index'),
            'name': g.get('name'),
            'mem_total_mb': g.get('mem_total_mb', g.get('memory_total_mb')),
        })
    return sorted(normalized, key=lambda g: (g.get('index', -1), g.get('name') or ''))
def load_nodes(path):
    with open(path, encoding='utf-8') as f:
        data = json.load(f)
    return {n.get('hostname'): norm_gres(n.get('gres', [])) for n in data.get('nodes', [])}
obt, exp = load_nodes(obtained_path), load_nodes(expected_path)
messages=[]
for name in sorted(set(exp) | set(obt)):
    if name not in obt:
        messages.append(f"expected {name} with {exp[name]} but obtained <missing node>")
    elif name not in exp:
        messages.append(f"expected <absent node> but obtained {name} with {obt[name]}")
    elif exp[name] != obt[name]:
        messages.append(f"expected {name} with {exp[name]} but obtained {obt[name]}")
if messages:
    print(f"FAIL: obtained {len(messages)} result not matching expected outcome:", file=sys.stderr)
    for i, msg in enumerate(messages, 1):
        print(f"{i}: {msg}", file=sys.stderr)
    sys.exit(1)
print("PASS: result obtained equals to result expected", file=sys.stderr)
PY
}

query_inventory_until() {
  local uds="$1" expected_json="$2" out="$3" deadline=$((SECONDS + 20))
  while (( SECONDS < deadline )); do
    if GPUSTAT4CLUSTER_BACKEND_SOCKET="$uds" "$CLIENT_BIN" --json >"$out" 2>"$out.err"; then
      if compare_inventory_json "$out" "$expected_json" >>"$out.err" 2>&1; then
        cat "$out.err" >&2
        return 0
      fi
    fi
    sleep 0.2
  done
  echo "query did not reach expected inventory $expected_json" >&2
  [[ -f "$out" ]] && cat "$out" >&2 || true
  [[ -f "$out.err" ]] && cat "$out.err" >&2 || true
  return 1
}

assert_query_latency() {
  local uds="$1" max_first_us="$2" avg_max_us="$3" out="$4"
  python3 - "$CLIENT_BIN" "$uds" "$max_first_us" "$avg_max_us" "$out" <<'PY'
import json, os, subprocess, sys
client, uds, first_limit, avg_limit, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
env = os.environ.copy(); env['GPUSTAT4CLUSTER_BACKEND_SOCKET'] = uds
samples=[]
def max_delay_us(payload):
    data = json.loads(payload)
    delays = [node.get('delay_us') for node in data.get('nodes', []) if node.get('delay_us') is not None]
    if not delays:
        raise RuntimeError('query response did not include delay_us')
    return max(delays)
for i in range(11):
    cp=subprocess.run([client, '--json'], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if cp.returncode:
        sys.stderr.write(cp.stderr.decode(errors='replace')); sys.exit(cp.returncode)
    try:
        dur = max_delay_us(cp.stdout)
    except Exception as exc:
        sys.stderr.write(f"latency parse failed: {exc}\n")
        sys.stderr.write(cp.stdout.decode(errors='replace'))
        sys.exit(1)
    if i==0:
        open(out, 'wb').write(cp.stdout)
        if dur > first_limit:
            print(f"latency first query {dur}us exceeds limit {first_limit}us", file=sys.stderr); sys.exit(1)
    else:
        samples.append(dur)
avg=sum(samples)//len(samples)
print(f"latency first_ok<= {first_limit}us avg_10={avg}us limit={avg_limit}us", file=sys.stderr)
if avg > avg_limit:
    sys.exit(1)
PY
}

stop_e2e_pid() {
  local pid="$1"
  if [[ "$E2E_NODE_MODE" == "docker" && ! "$pid" =~ ^[0-9]+$ ]]; then
    docker rm -f "$pid" >/dev/null 2>&1 || true
    return
  fi
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
}

server_container_name() {
  local config="$1"
  local base
  base="$(basename "$(dirname "$config")" | tr -c 'A-Za-z0-9_.-' '-')"
  echo "g4c-$E2E_RUN_ID-server-$base"
}

backend_container_name() {
  local config="$1"
  local base
  base="$(basename "$(dirname "$config")" | tr -c 'A-Za-z0-9_.-' '-')"
  echo "g4c-$E2E_RUN_ID-backend-$base"
}

start_container() {
  local role="$1" name="$2" log="$3"
  shift 3
  docker rm -f "$name" >/dev/null 2>&1 || true
  local env_args=()
  local mount_args=("-v" "$ROOT_DIR:$ROOT_DIR" "-v" "$E2E_TMP_ROOT:$E2E_TMP_ROOT")
  if [[ -n "${CARGO_TARGET_DIR:-}" && "$CARGO_TARGET_DIR" != "$ROOT_DIR"/* ]]; then
    mount_args+=("-v" "$CARGO_TARGET_DIR:$CARGO_TARGET_DIR")
  fi
  while [[ "$#" -gt 0 && "$1" == *=* ]]; do
    env_args+=("-e" "$1")
    shift
  done
  docker run -d \
    --name "$name" \
    --hostname "$name" \
    --network "$E2E_DOCKER_NETWORK" \
    "${mount_args[@]}" \
    -w "$ROOT_DIR" \
    "${env_args[@]}" \
    "$E2E_NODE_IMAGE" \
    "$@" >/dev/null
  E2E_CONTAINERS+=("$name")
  docker logs -f "$name" >"$log" 2>&1 &
  E2E_PIDS+=("$!")
  E2E_LAST_PID="$name"
  echo "started $role container $name" >>"$log"
}

translate_static_nodes() {
  local raw="$1"
  [[ -z "$raw" ]] && return 0
  local out=() item host port replacement
  IFS=',' read -r -a items <<<"$raw"
  for item in "${items[@]}"; do
    item="${item//[[:space:]]/}"
    [[ -z "$item" ]] && continue
    host="${item%:*}"
    port="${item##*:}"
    replacement=""
    if [[ "$host" == "127.0.0.1" || "$host" == "localhost" ]]; then
      replacement="${E2E_SERVER_BY_TCP[$port]:-${E2E_SERVER_BY_UDP[$port]:-}}"
    fi
    if [[ -n "$replacement" ]]; then
      out+=("$replacement:$port")
    else
      out+=("$item")
    fi
  done
  local joined
  joined="$(IFS=','; echo "${out[*]}")"
  echo "$joined"
}

register_server_configs_from_tmp() {
  local config tcp_port udp_port container_name
  while IFS= read -r config; do
    container_name="$(server_container_name "$config")"
    tcp_port="$(toml_number "$config" "tcp_port" || true)"
    udp_port="$(toml_number "$config" "udp_port" || true)"
    [[ -n "$tcp_port" ]] && E2E_SERVER_BY_TCP["$tcp_port"]="$container_name"
    [[ -n "$udp_port" ]] && E2E_SERVER_BY_UDP["$udp_port"]="$container_name"
  done < <(find "$E2E_TMP_ROOT" -name server.toml -type f 2>/dev/null | sort)
}

toml_number() {
  local path="$1" key="$2"
  awk -F= -v key="$key" '
    $1 ~ "^[[:space:]]*" key "[[:space:]]*$" {
      gsub(/[[:space:]]/, "", $2)
      print $2
      found = 1
      exit
    }
    END { if (!found) exit 1 }
  ' "$path"
}

wait_for_socket() {
  local path="$1" deadline=$((SECONDS + 10))
  while (( SECONDS < deadline )); do
    [[ -S "$path" ]] && return 0
    sleep 0.05
  done
  echo "timeout waiting for UDS $path" >&2
  return 1
}

wait_for_log() {
  local path="$1" pattern="$2" deadline=$((SECONDS + 10))
  while (( SECONDS < deadline )); do
    if [[ -f "$path" ]] && grep -q "$pattern" "$path"; then
      return 0
    fi
    sleep 0.05
  done
  echo "timeout waiting for log pattern '$pattern' in $path" >&2
  [[ -f "$path" ]] && sed -n '1,160p' "$path" >&2 || true
  return 1
}

query_until() {
  local uds="$1" expected_nodes="$2" expected_gres="$3" out="$4" deadline=$((SECONDS + 20))
  while (( SECONDS < deadline )); do
    if GPUSTAT4CLUSTER_BACKEND_SOCKET="$uds" "$CLIENT_BIN" --json >"$out" 2>"$out.err"; then
      if python3 - "$out" "$expected_nodes" "$expected_gres" <<'PY'
import json, sys
path, expected_nodes, expected_gres = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
with open(path, encoding="utf-8") as f:
    data = json.load(f)
nodes = data.get("nodes", [])
gres = sum(len(node.get("gres", [])) for node in nodes)
if len(nodes) == expected_nodes and gres == expected_gres:
    sys.exit(0)
print(f"got nodes={len(nodes)} gres={gres}, expected nodes={expected_nodes} gres={expected_gres}", file=sys.stderr)
sys.exit(1)
PY
      then
        return 0
      fi
    fi
    sleep 0.2
  done
  echo "query did not reach expected nodes=$expected_nodes gres=$expected_gres" >&2
  [[ -f "$out" ]] && cat "$out" >&2 || true
  [[ -f "$out.err" ]] && cat "$out.err" >&2 || true
  return 1
}

probe_frontend() {
  local uds="$1" out="$2"
  GPUSTAT4CLUSTER_BACKEND_SOCKET="$uds" "$CLIENT_BIN" --json >"$out" 2>"$out.err" || true
}

assert_connected_events_at_least() {
  local log="$1" expected="$2"
  local actual
  actual="$(grep -c 'event=connected' "$log" 2>/dev/null || true)"
  if (( actual < expected )); then
    echo "connected event count $actual is lower than expected minimum $expected in $log" >&2
    [[ -f "$log" ]] && sed -n '1,200p' "$log" >&2 || true
    return 1
  fi
}

query_expect_any_stale_or_error() {
  local uds="$1" out="$2" deadline=$((SECONDS + 20))
  while (( SECONDS < deadline )); do
    if GPUSTAT4CLUSTER_BACKEND_SOCKET="$uds" "$CLIENT_BIN" --json >"$out" 2>"$out.err"; then
      if python3 - "$out" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as f:
    data = json.load(f)
nodes = data.get("nodes", [])
if any(node.get("stale") or node.get("error") for node in nodes):
    sys.exit(0)
print("no stale/error node found", file=sys.stderr)
sys.exit(1)
PY
      then
        return 0
      fi
    fi
    sleep 0.2
  done
  echo "query did not expose a stale/error node" >&2
  [[ -f "$out" ]] && cat "$out" >&2 || true
  [[ -f "$out.err" ]] && cat "$out.err" >&2 || true
  return 1
}

query_expect_driver() {
  local uds="$1" expected_driver="$2" out="$3" deadline=$((SECONDS + 20))
  while (( SECONDS < deadline )); do
    if GPUSTAT4CLUSTER_BACKEND_SOCKET="$uds" "$CLIENT_BIN" --json >"$out" 2>"$out.err"; then
      if python3 - "$out" "$expected_driver" <<'PY'
import json, sys
path, expected = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as f:
    data = json.load(f)
drivers = [node.get("driver_version") for node in data.get("nodes", [])]
if expected in drivers:
    sys.exit(0)
print(f"driver versions={drivers}, expected {expected}", file=sys.stderr)
sys.exit(1)
PY
      then
        return 0
      fi
    fi
    sleep 0.2
  done
  echo "query did not reach expected driver_version=$expected_driver" >&2
  [[ -f "$out" ]] && cat "$out" >&2 || true
  [[ -f "$out.err" ]] && cat "$out.err" >&2 || true
  return 1
}

dump_e2e_diagnostics() {
  local title="$1" dir="$2"
  echo "::error title=$title::e2e failure diagnostics from $dir" >&2
  find "$dir" -maxdepth 2 -type f | sort | while read -r file; do
    case "$file" in
      *.log|*.json|*.err|*.toml)
        echo "::group::$file" >&2
        sed -n '1,240p' "$file" >&2 || true
        echo "::endgroup::" >&2
        ;;
    esac
  done
}

make_single_fixture() {
  local name="$1" seed="$2"
  local protocol="${3:-tcp}"
  local dir="$E2E_TMP_ROOT/$name"
  mkdir -p "$dir"
  read -r tcp_port udp_port mcast_port < <(alloc_ports 3)
  local multicast="239.255.0.$((seed % 200 + 1)):$mcast_port"
  local inventory="$dir/inventory.json"
  local runtime="$dir/runtime.mmap"
  local server_config="$dir/server.toml"
  local client_config="$dir/client.toml"
  local uds="$dir/client.sock"
  local count
  count="$(write_inventory "$inventory" "node-$name" "$seed")"
  write_server_config "$server_config" "$tcp_port" "$udp_port" "$multicast" "$inventory" "$runtime" false "$protocol"
  write_client_config "$client_config" "$uds" "$multicast" "$protocol"
  echo "$dir|$tcp_port|$udp_port|$multicast|$inventory|$server_config|$client_config|$uds|$count"
}
