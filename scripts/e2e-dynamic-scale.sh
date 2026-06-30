#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e2e-lib.sh"
require_binaries

scale_group="${E2E_SCALE_GROUP:-0}"
seed="${E2E_SEED:-$((91000 + scale_group * 997 + ${GITHUB_RUN_ATTEMPT:-1}))}"
scale_dir="$E2E_TMP_ROOT/scale-group-$scale_group"
mkdir -p "$scale_dir"
read -r mcast_port < <(alloc_ports 1)
multicast="239.255.1.$((scale_group % 200 + 20)):$mcast_port"
server_count=16
tcp_backend_count=8
udp_backend_count=8
close_count=5
server_configs=()
server_handles=()
inventories=()
backend_configs=()
backend_sockets=()
backend_handles=()
backend_protocols=()
backend_alive=()

for i in $(seq 0 $((server_count - 1))); do
  read -r tcp_port udp_port < <(alloc_ports 2)
  dir="$scale_dir/server-$i"
  mkdir -p "$dir"
  inventory="$dir/inventory.json"
  runtime="$dir/runtime.mmap"
  server_config="$dir/server.toml"
  write_inventory_count "$inventory" "scale-node-$scale_group-$i" 4 "$((10000 + scale_group * 100 + i))" >/dev/null
  write_server_config "$server_config" "$tcp_port" "$udp_port" "$multicast" "$inventory" "$runtime" false udp
  server_configs+=("$server_config|$dir/server.log")
  server_handles+=("")
  inventories+=("$inventory")
done

for i in $(seq 0 $((tcp_backend_count + udp_backend_count - 1))); do
  dir="$scale_dir/backend-$i"
  mkdir -p "$dir"
  uds="$dir/client.sock"
  client_config="$dir/client.toml"
  protocol="tcp"
  (( i >= tcp_backend_count )) && protocol="udp"
  write_client_config "$client_config" "$uds" "$multicast" "$protocol"
  backend_configs+=("$client_config|$dir/backend.log")
  backend_sockets+=("$uds")
  backend_handles+=("")
  backend_protocols+=("$protocol")
  backend_alive+=(1)
done

python3 - "$seed" "$server_count" "$((tcp_backend_count + udp_backend_count))" "$close_count" >"$scale_dir/plan.tsv" <<'PY'
import random, sys
seed, server_count, backend_count, close_count = map(int, sys.argv[1:5])
rng = random.Random(seed)
not_started = [('server', i) for i in range(server_count)] + [('backend', i) for i in range(backend_count)]
started_backends = []
closed = set()
plan = []
while not_started or len(closed) < close_count:
    choices = []
    if not_started:
        choices.append('start')
    close_candidates = [i for i in started_backends if i not in closed]
    if close_candidates and len(closed) < close_count:
        choices.append('close')
    action = rng.choice(choices)
    if action == 'start':
        idx = rng.randrange(len(not_started))
        kind, ident = not_started.pop(idx)
        plan.append((kind, ident))
        if kind == 'backend':
            started_backends.append(ident)
    else:
        ident = rng.choice(close_candidates)
        closed.add(ident)
        plan.append(('close_backend', ident))
for kind, ident in plan:
    print(f"{kind}\t{ident}")
PY

while IFS=$'\t' read -r kind idx; do
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
    close_backend)
      if [[ -n "${backend_handles[$idx]}" ]]; then
        request_backend_shutdown "${backend_sockets[$idx]}" "dynamic scale planned shutdown" 2>>"$scale_dir/close-$idx.err" || true
        stop_e2e_pid "${backend_handles[$idx]}"
      fi
      backend_alive[$idx]=0
      ;;
  esac
  sleep 0.05
done <"$scale_dir/plan.tsv"

sleep 2
expected="$scale_dir/expected-final.json"
write_expected_manifest "$expected" "${inventories[@]}"
checked=0
for i in "${!backend_sockets[@]}"; do
  if (( backend_alive[$i] == 1 )); then
    wait_for_socket "${backend_sockets[$i]}"
    query_inventory_until "${backend_sockets[$i]}" "$expected" "$scale_dir/query-final-$i.json"
    checked=$((checked + 1))
  fi
done
if (( checked == 0 )); then
  echo "FAIL: dynamic scale group $scale_group closed all clients" >&2
  dump_e2e_diagnostics "dynamic-scale-no-live-client" "$scale_dir"
  exit 1
fi

echo "dynamic-scale ok group=$scale_group seed=$seed servers=$server_count live_clients=$checked closed_clients=$close_count"
