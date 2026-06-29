#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e2e-lib.sh"
require_binaries
IFS='|' read -r dir tcp_port _udp_port _multicast inventory server_config client_config uds count < <(make_single_fixture "client-first-dynamic" 12)
expected="$dir/expected.json"
write_expected_manifest "$expected" "$inventory"
start_backend_pid "$client_config" "" "$dir/backend.log"
backend_pid="$E2E_LAST_PID"
wait_for_socket "$uds"
sleep 1
start_server_pid "$server_config" "$dir/server.log"
server_pid="$E2E_LAST_PID"
wait_for_socket "$uds"
query_inventory_until "$uds" "$expected" "$dir/query.json"
assert_query_latency "$uds" 1000000 1000 "$dir/query-latency.json"
stop_e2e_pid "${backend_pid}"
sleep 0.2
stop_e2e_pid "${server_pid}"
echo "client-first-dynamic ok nodes=1 gres=$count"
