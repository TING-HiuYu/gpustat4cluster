#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e2e-lib.sh"
require_binaries

failures=0

record_failure() {
  local title="$1" dir="$2" message="$3"
  failures=$((failures + 1))
  echo "::error title=$title::$message" >&2
  dump_e2e_diagnostics "$title" "$dir"
}

A_EVENTS=(
  gres_shrink_2 gres_expand_8 gres_expand_6 gres_memory_96
  gres_memory_8 gres_rename_alpha gres_rename_beta gres_four_24
  gres_five_36 gres_three_80 gres_seven_16 gres_one_48
  noop noop noop noop noop noop noop noop noop noop noop noop
)
B_EVENTS=(
  server_sigterm server_sigkill server_graceful server_restart_then_stop
  server_network_drop server_disconnect_during_query server_connection_limit server_bad_frame
  server_invalid_config server_bind_conflict server_collector_panic server_partial_frame
  noop noop noop noop noop noop noop noop noop noop noop noop
)
C_EVENTS=(
  client_shutdown client_sigterm client_sigkill client_frontend_burst
  client_uds_removed client_static_unreachable client_duplicate_announce client_stale_cache
  client_reconnect client_bad_frame client_filter_node client_filter_user
  noop noop noop noop noop noop noop noop noop noop noop noop
)

apply_a_event() {
  local inventory="$1" hostname="$2" event="$3" seed="$4"
  python3 - "$inventory" "$hostname" "$event" "$seed" <<'PY'
import json, sys
path, hostname, event, seed = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
with open(path, encoding='utf-8') as f:
    data = json.load(f)
mem_default = [8, 16, 24, 36, 48, 80, 96]
def count_from_event():
    if event == 'gres_shrink_2': return 2
    if event == 'gres_expand_8': return 8
    if event == 'gres_expand_6': return 6
    if event == 'gres_four_24': return 4
    if event == 'gres_five_36': return 5
    if event == 'gres_three_80': return 3
    if event == 'gres_seven_16': return 7
    if event == 'gres_one_48': return 1
    return len(data.get('gres', [])) or 4
count = count_from_event()
name_prefix = 'NVIDIA Test GPU'
if event == 'gres_rename_alpha':
    name_prefix = 'NVIDIA Alpha Test GPU'
elif event == 'gres_rename_beta':
    name_prefix = 'NVIDIA Beta Test GPU'
fixed_mem = None
if event == 'gres_memory_96': fixed_mem = 96
elif event == 'gres_memory_8': fixed_mem = 8
elif event == 'gres_four_24': fixed_mem = 24
elif event == 'gres_five_36': fixed_mem = 36
elif event == 'gres_three_80': fixed_mem = 80
elif event == 'gres_seven_16': fixed_mem = 16
elif event == 'gres_one_48': fixed_mem = 48
gres = []
for idx in range(count):
    mem_gib = fixed_mem if fixed_mem is not None else mem_default[(seed + idx * 5) % len(mem_default)]
    gres.append({
        'index': idx,
        'name': f'{name_prefix} {idx}',
        'uuid': f'GRES-E2E-{hostname.replace(".", "-")}-{idx:04d}',
        'memory_total_mb': mem_gib * 1024,
    })
data['hostname'] = hostname
data['driver_version'] = f'test-driver-{seed}-{event}'
data['gres'] = gres
with open(path, 'w', encoding='utf-8') as f:
    json.dump(data, f)
PY
}

start_server_index() {
  local idx="$1"
  IFS='|' read -r cfg log <<<"${server_configs[$idx]}"
  start_server_pid "$cfg" "$log"
  server_handles[$idx]="$E2E_LAST_PID"
  server_alive[$idx]=1
}

start_backend_index() {
  local idx="$1"
  IFS='|' read -r cfg log <<<"${backend_configs[$idx]}"
  start_backend_pid "$cfg" "" "$log"
  backend_handles[$idx]="$E2E_LAST_PID"
  backend_alive[$idx]=1
}

stop_server_index() {
  local idx="$1" reason="$2"
  local hostname="${hostnames[$idx]}"
  if [[ -n "${server_handles[$idx]}" ]]; then
    stop_e2e_pid "${server_handles[$idx]}"
  fi
  server_alive[$idx]=0
  echo "server-$idx stopped reason=$reason" >>"$case_dir/events.log"
  for backend_idx in "${!backend_sockets[@]}"; do
    if (( backend_alive[$backend_idx] == 1 )) && [[ -S "${backend_sockets[$backend_idx]}" ]]; then
      request_backend_disconnect_host "${backend_sockets[$backend_idx]}" "$hostname" 2>>"$case_dir/disconnect-host-$idx-client-$backend_idx.err" || true
    fi
  done
}

stop_backend_index() {
  local idx="$1" reason="$2"
  if [[ -n "${backend_handles[$idx]}" ]]; then
    if [[ -S "${backend_sockets[$idx]}" ]]; then
      request_backend_shutdown "${backend_sockets[$idx]}" "robustness $reason" 2>>"$case_dir/client-stop-$idx.err" || true
    fi
    stop_e2e_pid "${backend_handles[$idx]}"
  fi
  backend_alive[$idx]=0
  echo "backend-$idx stopped reason=$reason" >>"$case_dir/events.log"
}

write_expected_alive() {
  local out="$1"
  local items=()
  for idx in "${!inventories[@]}"; do
    if (( server_alive[$idx] == 1 )); then
      items+=("${inventories[$idx]}")
    fi
  done
  write_expected_manifest "$out" "${items[@]}"
}

query_live_clients() {
  local expected="$1" label="$2" mode="${3:-all}"
  local checked=0
  for idx in "${!backend_sockets[@]}"; do
    if (( backend_alive[$idx] == 1 )); then
      if [[ "$mode" == "first" && "$checked" -gt 0 ]]; then
        continue
      fi
      wait_for_socket "${backend_sockets[$idx]}"
      if ! query_inventory_until "${backend_sockets[$idx]}" "$expected" "$case_dir/query-$label-client-$idx.json"; then
        record_failure "robustness-$label" "$case_dir" "FAIL: test $label client=$idx did not match expected inventory"
        return 1
      fi
      checked=$((checked + 1))
    fi
  done
  if (( checked == 0 )); then
    record_failure "robustness-$label" "$case_dir" "FAIL: test $label has no live clients to validate"
    return 1
  fi
}

make_server() {
  local idx="$1" hostname="$2" seed="$3" protocol="$4"
  read -r tcp_port udp_port < <(alloc_ports 2)
  local dir="$case_dir/server-$idx"
  mkdir -p "$dir"
  local inventory="$dir/inventory.json"
  local runtime="$dir/runtime.mmap"
  local server_config="$dir/server.toml"
  write_inventory_count "$inventory" "$hostname" 4 "$seed" >/dev/null
  write_server_config "$server_config" "$tcp_port" "$udp_port" "$multicast" "$inventory" "$runtime" true "$protocol"
  server_configs+=("$server_config|$dir/server.log")
  server_handles+=("")
  server_alive+=(0)
  inventories+=("$inventory")
  hostnames+=("$hostname")
}

make_backend() {
  local idx="$1" protocol="$2"
  local dir="$case_dir/client-$idx"
  mkdir -p "$dir"
  local uds="$dir/client.sock"
  local client_config="$dir/client.toml"
  write_client_config "$client_config" "$uds" "$multicast" "$protocol"
  backend_configs+=("$client_config|$dir/backend.log")
  backend_sockets+=("$uds")
  backend_handles+=("")
  backend_alive+=(0)
  backend_protocols+=("$protocol")
}

run_ac_group() {
  local group="$1" protocol="$2"
  case_dir="$E2E_TMP_ROOT/robustness-group-$group"
  mkdir -p "$case_dir"
  read -r mcast_port < <(alloc_ports 1)
  multicast="239.255.2.$((group + 20)):$mcast_port"
  server_configs=(); server_handles=(); server_alive=(); inventories=(); hostnames=()
  backend_configs=(); backend_sockets=(); backend_handles=(); backend_alive=(); backend_protocols=()

  make_server 0 "robust-$group-active-ac" "$((20000 + group * 100))" "$protocol"
  for i in $(seq 1 3); do
    make_server "$i" "robust-$group-passive-server-$i" "$((20000 + group * 100 + i))" "$protocol"
  done
  for i in $(seq 0 27); do
    make_backend "$i" "$protocol"
  done
  for i in "${!server_configs[@]}"; do start_server_index "$i"; done
  for i in "${!backend_configs[@]}"; do start_backend_index "$i"; done
  sleep 2

  for ai in "${!A_EVENTS[@]}"; do
    event="${A_EVENTS[$ai]}"
    apply_a_event "${inventories[0]}" "${hostnames[0]}" "$event" "$((21000 + group * 1000 + ai))"
    expected="$case_dir/expected-a-$ai.json"
    write_expected_alive "$expected"
    if ! query_live_clients "$expected" "${protocol}-A${ai}-${event}" all; then
      return
    fi
  done

  for ci in $(seq 0 23); do
    stop_backend_index "$ci" "C${ci}-${C_EVENTS[$ci]}"
  done
  sleep 2
  expected="$case_dir/expected-final.json"
  write_expected_alive "$expected"
  query_live_clients "$expected" "${protocol}-final-after-C" all || true
}

run_mixed_group() {
  local group="$1" protocol="$2" offset="$3"
  case_dir="$E2E_TMP_ROOT/robustness-group-$group"
  mkdir -p "$case_dir"
  read -r mcast_port < <(alloc_ports 1)
  multicast="239.255.2.$((group + 20)):$mcast_port"
  server_configs=(); server_handles=(); server_alive=(); inventories=(); hostnames=()
  backend_configs=(); backend_sockets=(); backend_handles=(); backend_alive=(); backend_protocols=()

  for i in $(seq 0 7); do make_server "$i" "robust-$group-B-$i" "$((22000 + group * 100 + i))" "$protocol"; done
  for i in $(seq 0 7); do make_server "$((8 + i))" "robust-$group-AB-$i" "$((22100 + group * 100 + i))" "$protocol"; done
  for i in $(seq 0 3); do make_server "$((16 + i))" "robust-$group-passive-server-$i" "$((22200 + group * 100 + i))" "$protocol"; done
  for i in $(seq 0 11); do make_backend "$i" "$protocol"; done

  for i in "${!server_configs[@]}"; do start_server_index "$i"; done
  for i in "${!backend_configs[@]}"; do start_backend_index "$i"; done
  sleep 2

  for lane in $(seq 0 7); do
    server_idx=$((8 + lane))
    b_idx=$((offset + lane))
    for ai in "${!A_EVENTS[@]}"; do
      event="${A_EVENTS[$ai]}"
      apply_a_event "${inventories[$server_idx]}" "${hostnames[$server_idx]}" "$event" "$((23000 + group * 1000 + lane * 100 + ai))"
      expected="$case_dir/expected-ab-lane-$lane-a-$ai.json"
      write_expected_alive "$expected"
      if ! query_live_clients "$expected" "${protocol}-AB${b_idx}-A${ai}-${event}" first; then
        return
      fi
    done
    stop_server_index "$server_idx" "A-to-B${b_idx}-${B_EVENTS[$b_idx]}"
    sleep 0.2
  done

  for lane in $(seq 0 7); do
    b_idx=$((offset + lane))
    stop_server_index "$lane" "B${b_idx}-${B_EVENTS[$b_idx]}"
    sleep 0.1
  done
  for lane in $(seq 0 7); do
    c_idx=$((offset + lane))
    stop_backend_index "$lane" "C${c_idx}-${C_EVENTS[$c_idx]}"
    sleep 0.1
  done

  sleep 7
  expected="$case_dir/expected-final.json"
  write_expected_alive "$expected"
  query_live_clients "$expected" "${protocol}-final-mixed-$offset" all || true
}

group="${E2E_ROBUSTNESS_GROUP:-0}"
case "$group" in
  0) run_ac_group 0 tcp ;;
  1) run_ac_group 1 udp ;;
  2) run_mixed_group 2 tcp 0 ;;
  3) run_mixed_group 3 tcp 8 ;;
  4) run_mixed_group 4 tcp 16 ;;
  5) run_mixed_group 5 udp 0 ;;
  6) run_mixed_group 6 udp 8 ;;
  7) run_mixed_group 7 udp 16 ;;
  *) echo "invalid E2E_ROBUSTNESS_GROUP=$group" >&2; exit 1 ;;
esac

if (( failures > 0 )); then
  echo "e2e-robustness failed scenarios=$failures group=$group" >&2
  exit 1
fi

echo "e2e-robustness ok group=$group"
