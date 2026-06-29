#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e2e-lib.sh"
require_binaries
IFS='|' read -r dir tcp_port _udp_port _multicast inventory server_config client_config uds count < <(make_single_fixture "server-first-static" 13)
expected="$dir/expected.json"
write_expected_manifest "$expected" "$inventory"
start_server_pid "$server_config" "$dir/server.log"
server_pid="$E2E_LAST_PID"
sleep 1
start_backend_pid "$client_config" "127.0.0.1:$tcp_port" "$dir/backend.log"
backend_pid="$E2E_LAST_PID"
wait_for_socket "$uds"
query_inventory_until "$uds" "$expected" "$dir/query.json"
assert_query_latency "$uds" 1000000 300 "$dir/query-latency.json"
stop_e2e_pid "${server_pid}"
query_expect_any_stale_or_error "$uds" "$dir/query-after-server-disconnect.json"
assert_backend_disconnected_seen "$dir/backend.log"
sleep 0.2
stop_e2e_pid "${backend_pid}"
echo "server-first-static ok nodes=1 gres=$count"
