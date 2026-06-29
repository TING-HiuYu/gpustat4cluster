#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e2e-lib.sh"
require_binaries
scale_dir="$E2E_TMP_ROOT/scale"
mkdir -p "$scale_dir"
read -r mcast_port < <(alloc_ports 1)
multicast="239.255.0.222:$mcast_port"
protocol="${E2E_PROTOCOL:-udp}"
server_count=16
backend_count=8
frontend_count=32
server_configs=()
server_handles=()
inventories=()
backend_configs=()
backend_sockets=()
backend_handles=()
for i in $(seq 1 "$server_count"); do
  read -r tcp_port udp_port < <(alloc_ports 2)
  dir="$scale_dir/server-$i"
  mkdir -p "$dir"
  inventory="$dir/inventory.json"
  runtime="$dir/runtime.mmap"
  server_config="$dir/server.toml"
  write_inventory "$inventory" "scale-node-$i" "$((100 + i))" >/dev/null
  write_server_config "$server_config" "$tcp_port" "$udp_port" "$multicast" "$inventory" "$runtime" false "$protocol"
  server_configs+=("$server_config|$dir/server.log")
  server_handles+=("")
  inventories+=("$inventory")
done
for i in $(seq 1 "$backend_count"); do
  dir="$scale_dir/backend-$i"
  mkdir -p "$dir"
  uds="$dir/client.sock"
  client_config="$dir/client.toml"
  write_client_config "$client_config" "$uds" "$multicast" "$protocol"
  backend_configs+=("$client_config|$dir/backend.log")
  backend_sockets+=("$uds")
  backend_handles+=("")
done
for action in \
  "server:7" "backend:0" "server:0" "frontend:0" \
  "backend:3" "server:14" "server:3" "backend:6" \
  "frontend:3" "server:10" "backend:1" "server:5" \
  "server:12" "backend:4" "frontend:6" "server:1" \
  "backend:7" "server:8" "backend:2" "server:15" \
  "frontend:1" "server:2" "server:9" "backend:5" \
  "server:6" "frontend:4" "server:13" "server:4" \
  "frontend:7" "server:11"; do
  kind="${action%%:*}"
  idx="${action##*:}"
  case "$kind" in
    server)
      IFS='|' read -r cfg log <<<"${server_configs[$idx]}"
      start_server_pid "$cfg" "$log"
      server_handles[$idx]="$E2E_LAST_PID"
      ;;
    backend)
      IFS='|' read -r cfg log <<<"${backend_configs[$idx]}"
      start_backend_pid "$cfg" "" "$log"
      backend_handles[$idx]="$E2E_LAST_PID"
      ;;
    frontend)
      uds="${backend_sockets[$idx]}"
      [[ -S "$uds" ]] && probe_frontend "$uds" "$scale_dir/probe-$idx.json"
      ;;
  esac
  sleep 0.05
done
for uds in "${backend_sockets[@]}"; do
  wait_for_socket "$uds"
done
expected="$scale_dir/expected-all.json"
write_expected_manifest "$expected" "${inventories[@]}"
for i in $(seq 1 "$frontend_count"); do
  uds="${backend_sockets[$(((i - 1) % backend_count))]}"
  query_inventory_until "$uds" "$expected" "$scale_dir/query-$i.json"
done
# After successful startup, disconnect half the servers in a deterministic random order and verify no residue remains.
mapfile -t disconnect_indices < <(python3 - <<'PY'
import random
items=list(range(16))
random.Random(4242).shuffle(items)
print('\n'.join(map(str, items[:8])))
PY
)
remaining=()
for idx in "${disconnect_indices[@]}"; do
  [[ -n "${server_handles[$idx]}" ]] && stop_e2e_pid "${server_handles[$idx]}"
done
sleep 6
for idx in "${!inventories[@]}"; do
  skip=0
  for gone in "${disconnect_indices[@]}"; do [[ "$idx" == "$gone" ]] && skip=1; done
  (( skip == 0 )) && remaining+=("${inventories[$idx]}")
done
expected_remaining="$scale_dir/expected-after-disconnect.json"
write_expected_manifest "$expected_remaining" "${remaining[@]}"
for i in "${!backend_sockets[@]}"; do
  query_inventory_until "${backend_sockets[$i]}" "$expected_remaining" "$scale_dir/query-disconnect-$i.json"
  IFS='|' read -r _cfg log <<<"${backend_configs[$i]}"
  assert_connected_events_at_least "$log" "$server_count"
done
echo "dynamic-scale ok protocol=$protocol nodes_before=$server_count nodes_after=${#remaining[@]} frontends=$frontend_count backends=$backend_count"
