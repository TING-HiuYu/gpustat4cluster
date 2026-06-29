#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e2e-lib.sh"
require_binaries
IFS='|' read -r dir _tcp_port _udp_port _multicast server_config client_config uds count < <(make_single_fixture "server-first-dynamic" 14)
start_server "$server_config" "$dir/server.log"
sleep 1
start_backend "$client_config" "" "$dir/backend.log"
wait_for_socket "$uds"
query_until "$uds" 1 "$count" "$dir/query.json"
echo "server-first-dynamic ok nodes=1 gres=$count"
