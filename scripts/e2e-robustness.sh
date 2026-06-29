#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e2e-lib.sh"
require_binaries

failures=0
scenarios=()

record_failure() {
  local title="$1" dir="$2" message="$3"
  failures=$((failures + 1))
  echo "::error title=$title::$message" >&2
  dump_e2e_diagnostics "$title" "$dir"
}

register_scenario() {
  scenarios+=("$1")
}

run_registered_scenarios() {
  local scenario
  for scenario in "${scenarios[@]}"; do
    echo "running robustness scenario: $scenario" >&2
    "$scenario"
  done
}

slow_collector_config() {
  local config="$1"
  python3 - "$config" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
raw = path.read_text()
raw = raw.replace("collector_interval_ms = 5", "collector_interval_ms = 60000")
path.write_text(raw)
PY
}

make_count_fixture() {
  local name="$1" count="$2" seed="$3" reload="$4" slow="$5"
  local dir="$E2E_TMP_ROOT/$name"
  mkdir -p "$dir"
  read -r tcp_port udp_port mcast_port < <(alloc_ports 3)
  local multicast="239.255.0.$((seed % 200 + 20)):$mcast_port"
  local inventory="$dir/inventory.json"
  local runtime="$dir/runtime.mmap"
  local server_config="$dir/server.toml"
  local client_config="$dir/client.toml"
  local uds="$dir/client.sock"
  write_inventory_count "$inventory" "node-$name" "$count" "$seed" >/dev/null
  write_server_config "$server_config" "$tcp_port" "$udp_port" "$multicast" "$inventory" "$runtime" "$reload"
  if [[ "$slow" == "true" ]]; then
    slow_collector_config "$server_config"
  fi
  write_client_config "$client_config" "$uds" "$multicast"
  echo "$dir|$tcp_port|$udp_port|$multicast|$inventory|$server_config|$client_config|$uds"
}

scenario_expand_inventory() {
  local name="robust-expand-inventory"
  IFS='|' read -r dir _tcp_port udp_port _multicast inventory server_config client_config uds < <(make_count_fixture "$name" 6 301 true false)
  start_server "$server_config" "$dir/server.log"
  sleep 1
  start_backend "$client_config" "127.0.0.1:$udp_port" "$dir/backend.log"
  wait_for_socket "$uds"
  if ! query_until "$uds" 1 6 "$dir/query-initial.json"; then
    record_failure "robust-expand-initial" "$dir" "initial 6-GRES query failed"
    return
  fi
  write_inventory_count "$inventory" "node-$name" 8 302 >/dev/null
  sleep 1
  if ! query_until "$uds" 1 8 "$dir/query-expanded.json"; then
    record_failure "robust-expand-stale-shape" "$dir" "inventory changed 6 -> 8, but frontend did not observe 8 GRES after TTL expiry"
  fi
}

scenario_shrink_inventory() {
  local name="robust-shrink-inventory"
  IFS='|' read -r dir _tcp_port udp_port _multicast inventory server_config client_config uds < <(make_count_fixture "$name" 8 401 true false)
  start_server "$server_config" "$dir/server.log"
  sleep 1
  start_backend "$client_config" "127.0.0.1:$udp_port" "$dir/backend.log"
  wait_for_socket "$uds"
  if ! query_until "$uds" 1 8 "$dir/query-initial.json"; then
    record_failure "robust-shrink-initial" "$dir" "initial 8-GRES query failed"
    return
  fi
  write_inventory_count "$inventory" "node-$name" 4 402 >/dev/null
  sleep 1
  if ! query_until "$uds" 1 4 "$dir/query-shrunk.json"; then
    record_failure "robust-shrink-stale-shape" "$dir" "inventory changed 8 -> 4, but frontend did not observe 4 GRES after TTL expiry"
  fi
}

scenario_metadata_reload() {
  local name="robust-metadata-reload"
  IFS='|' read -r dir _tcp_port udp_port _multicast inventory server_config client_config uds < <(make_count_fixture "$name" 4 501 true false)
  start_server "$server_config" "$dir/server.log"
  sleep 1
  start_backend "$client_config" "127.0.0.1:$udp_port" "$dir/backend.log"
  wait_for_socket "$uds"
  if ! query_until "$uds" 1 4 "$dir/query-initial.json"; then
    record_failure "robust-metadata-initial" "$dir" "initial metadata query failed"
    return
  fi
  write_inventory_count "$inventory" "node-$name" 4 509 >/dev/null
  sleep 1
  if ! query_expect_driver "$uds" "test-driver-509" "$dir/query-driver.json"; then
    record_failure "robust-metadata-stale" "$dir" "driver_version changed with same GRES count, but frontend still exposes stale node metadata"
  fi
}

scenario_collector_error_recovery() {
  local name="robust-collector-error"
  IFS='|' read -r dir _tcp_port udp_port _multicast inventory server_config client_config uds < <(make_count_fixture "$name" 4 601 true false)
  start_server "$server_config" "$dir/server.log"
  sleep 1
  start_backend "$client_config" "127.0.0.1:$udp_port" "$dir/backend.log"
  wait_for_socket "$uds"
  if ! query_until "$uds" 1 4 "$dir/query-initial.json"; then
    record_failure "robust-collector-initial" "$dir" "initial collector query failed"
    return
  fi
  printf '{"hostname": "broken", "gres": [' >"$inventory"
  sleep 1
  if ! grep -q 'test_collector_error\|collector_refresh_error\|configuration invalid' "$dir/server.log"; then
    record_failure "robust-collector-error-log-missing" "$dir" "malformed inventory did not produce an explicit collector error log"
  fi
  if ! GPUSTAT4CLUSTER_BACKEND_SOCKET="$uds" "$CLIENT_BIN" --json >"$dir/query-during-error.json" 2>"$dir/query-during-error.err"; then
    record_failure "robust-collector-query-failed" "$dir" "frontend query failed completely while collector inventory was malformed"
  fi
  write_inventory_count "$inventory" "node-$name" 4 602 >/dev/null
  sleep 1
  if ! query_until "$uds" 1 4 "$dir/query-recovered.json"; then
    record_failure "robust-collector-recovery" "$dir" "collector did not recover after malformed inventory was replaced with a valid file"
  fi
}

scenario_network_outage_recovery() {
  local name="robust-network-recovery"
  IFS='|' read -r dir _tcp_port udp_port _multicast _inventory server_config client_config uds < <(make_count_fixture "$name" 3 701 true false)
  local server_pid
  start_server_pid "$server_config" "$dir/server.log"
  server_pid="$E2E_LAST_PID"
  sleep 1
  start_backend "$client_config" "127.0.0.1:$udp_port" "$dir/backend.log"
  wait_for_socket "$uds"
  if ! query_until "$uds" 1 3 "$dir/query-initial.json"; then
    record_failure "robust-network-initial" "$dir" "initial query before outage failed"
    return
  fi
  stop_e2e_pid "$server_pid"
  sleep 1
  if ! query_expect_any_stale_or_error "$uds" "$dir/query-outage.json"; then
    record_failure "robust-network-outage-state" "$dir" "backend did not expose stale/error state after server process stopped"
  fi
  start_server_pid "$server_config" "$dir/server-restarted.log"
  server_pid="$E2E_LAST_PID"
  sleep 1
  if ! query_until "$uds" 1 3 "$dir/query-recovered.json"; then
    record_failure "robust-network-reconnect" "$dir" "backend did not reconnect after server restarted on the same address"
  fi
}

scenario_dynamic_scale_robustness() {
  local name="robust-dynamic-scale"
  local scale_dir="$E2E_TMP_ROOT/$name"
  mkdir -p "$scale_dir"
  read -r mcast_port < <(alloc_ports 1)
  local multicast="239.255.0.245:$mcast_port"
  local server_count=16
  local backend_count=8
  local frontend_count=32
  local server_configs=()
  local server_handles=()
  local inventories=()
  local final_counts=()
  local backend_configs=()
  local backend_sockets=()
  local backend_protocols=()
  for i in $(seq 1 "$server_count"); do
    read -r tcp_port udp_port < <(alloc_ports 2)
    local dir="$scale_dir/server-$i"
    mkdir -p "$dir"
    local inventory="$dir/inventory.json"
    local runtime="$dir/runtime.mmap"
    local server_config="$dir/server.toml"
    local count
    count="$(write_inventory "$inventory" "robust-scale-node-$i" "$((800 + i))")"
    write_server_config "$server_config" "$tcp_port" "$udp_port" "$multicast" "$inventory" "$runtime" true
    server_configs+=("$server_config|$dir/server.log")
    server_handles+=("")
    inventories+=("$inventory")
    final_counts+=("$count")
  done
  for i in $(seq 1 "$backend_count"); do
    local dir="$scale_dir/backend-$i"
    mkdir -p "$dir"
    local uds="$dir/client.sock"
    local client_config="$dir/client.toml"
    local protocol="udp"
    if (( i <= backend_count / 2 )); then
      protocol="tcp"
    fi
    write_client_config "$client_config" "$uds" "$multicast" "$protocol"
    backend_configs+=("$client_config|$dir/backend.log")
    backend_sockets+=("$uds")
    backend_protocols+=("$protocol")
  done
  for action in \
    "server:4" "backend:0" "server:0" "probe:0" \
    "expand:0:8:930" "backend:4" "server:9" "backend:2" \
    "server:5" "server:1" "corrupt:1" "probe:2" \
    "backend:6" "server:12" "backend:1" "server:3" \
    "shrink:3:2:931" "server:14" "recover:1:5:932" "stop:4" \
    "backend:5" "server:10" "backend:3" "server:2" \
    "restart:4" "server:6" "probe:1" "server:7" \
    "backend:7" "server:11" "probe:3" "server:8" \
    "server:13" "server:15" "probe:5" "probe:7"; do
    IFS=':' read -r kind idx arg1 arg2 <<<"$action"
    case "$kind" in
      server|restart)
        IFS='|' read -r cfg log <<<"${server_configs[$idx]}"
        start_server_pid "$cfg" "$log"
        server_handles[$idx]="$E2E_LAST_PID"
        ;;
      backend)
        IFS='|' read -r cfg log <<<"${backend_configs[$idx]}"
        echo "starting robust backend-$((idx + 1)) protocol=${backend_protocols[$idx]}" >>"$scale_dir/robustness.log"
        start_backend "$cfg" "" "$log"
        ;;
      probe)
        uds="${backend_sockets[$idx]}"
        [[ -S "$uds" ]] && probe_frontend "$uds" "$scale_dir/probe-$idx.json"
        ;;
      expand|shrink|recover)
        write_inventory_count "${inventories[$idx]}" "robust-scale-node-$((idx + 1))" "$arg1" "$arg2" >/dev/null
        final_counts[$idx]="$arg1"
        ;;
      corrupt)
        printf '{"hostname": "broken", "gres": [' >"${inventories[$idx]}"
        ;;
      stop)
        if [[ -n "${server_handles[$idx]}" ]]; then
          stop_e2e_pid "${server_handles[$idx]}"
        fi
        ;;
    esac
    sleep 0.1
  done
  for uds in "${backend_sockets[@]}"; do
    wait_for_socket "$uds"
  done
  local expected_total=0
  for count in "${final_counts[@]}"; do
    expected_total=$((expected_total + count))
  done
  local uds
  for i in $(seq 1 "$frontend_count"); do
    uds="${backend_sockets[$(((i - 1) % backend_count))]}"
    if ! query_until "$uds" "$server_count" "$expected_total" "$scale_dir/query-final-$i.json"; then
      record_failure "robust-scale-final" "$scale_dir" "dynamic robustness scenario did not converge to the recovered final topology"
      break
    fi
  done
  for i in "${!backend_configs[@]}"; do
    IFS='|' read -r _cfg log <<<"${backend_configs[$i]}"
    if ! assert_connected_events_at_least "$log" "$server_count"; then
      record_failure "robust-scale-connections" "$scale_dir" "backend-$((i + 1)) protocol=${backend_protocols[$i]} did not connect to all recovered servers"
    fi
  done
}

register_scenario scenario_expand_inventory
register_scenario scenario_shrink_inventory
register_scenario scenario_collector_error_recovery
register_scenario scenario_network_outage_recovery
register_scenario scenario_dynamic_scale_robustness

run_registered_scenarios

if (( failures > 0 )); then
  echo "e2e-robustness failed scenarios=$failures" >&2
  exit 1
fi

echo "e2e-robustness ok"
