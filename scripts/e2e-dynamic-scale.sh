#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e2e-lib.sh"
require_binaries
scale_dir="$E2E_TMP_ROOT/scale"
mkdir -p "$scale_dir"
read -r mcast_port < <(alloc_ports 1)
multicast="239.255.0.222:$mcast_port"
protocol="${E2E_PROTOCOL:-udp}"
expected_gres=0
server_count=16
backend_count=8
frontend_count=32
server_configs=()
backend_configs=()
backend_sockets=()
for i in $(seq 1 "$server_count"); do
  read -r tcp_port udp_port < <(alloc_ports 2)
  dir="$scale_dir/server-$i"
  mkdir -p "$dir"
  inventory="$dir/inventory.json"
  runtime="$dir/runtime.mmap"
  server_config="$dir/server.toml"
  count="$(write_inventory "$inventory" "scale-node-$i" "$((100 + i))")"
  expected_gres=$((expected_gres + count))
  write_server_config "$server_config" "$tcp_port" "$udp_port" "$multicast" "$inventory" "$runtime" false "$protocol"
  server_configs+=("$server_config|$dir/server.log")
done
for i in $(seq 1 "$backend_count"); do
  dir="$scale_dir/backend-$i"
  mkdir -p "$dir"
  uds="$dir/client.sock"
  client_config="$dir/client.toml"
  write_client_config "$client_config" "$uds" "$multicast" "$protocol"
  backend_configs+=("$client_config|$dir/backend.log")
  backend_sockets+=("$uds")
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
      start_server "$cfg" "$log"
      ;;
    backend)
      IFS='|' read -r cfg log <<<"${backend_configs[$idx]}"
      start_backend "$cfg" "" "$log"
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
for i in $(seq 1 "$frontend_count"); do
  uds="${backend_sockets[$(((i - 1) % backend_count))]}"
  query_until "$uds" "$server_count" "$expected_gres" "$scale_dir/query-$i.json"
done
for i in "${!backend_configs[@]}"; do
  IFS='|' read -r _cfg log <<<"${backend_configs[$i]}"
  assert_connected_events_at_least "$log" "$server_count"
done
echo "dynamic-scale ok protocol=$protocol nodes=$server_count gres=$expected_gres frontends=$frontend_count backends=$backend_count"
