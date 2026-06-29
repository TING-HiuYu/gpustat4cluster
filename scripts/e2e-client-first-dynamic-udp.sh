#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e2e-lib.sh"
require_binaries
IFS='|' read -r dir _tcp_port _udp_port _multicast server_config client_config uds count < <(make_single_fixture "client-first-dynamic-udp" 22 udp)
start_backend "$client_config" "" "$dir/backend.log"
wait_for_socket "$uds"
sleep 1
start_server "$server_config" "$dir/server.log"
query_until "$uds" 1 "$count" "$dir/query.json"
echo "client-first-dynamic-udp ok nodes=1 gres=$count"
