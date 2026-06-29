#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e2e-lib.sh"
require_binaries
IFS='|' read -r dir _tcp_port udp_port _multicast server_config client_config uds count < <(make_single_fixture "server-first-static-udp" 23 udp)
start_server "$server_config" "$dir/server.log"
sleep 1
start_backend "$client_config" "127.0.0.1:$udp_port" "$dir/backend.log"
wait_for_socket "$uds"
query_until "$uds" 1 "$count" "$dir/query.json"
echo "server-first-static-udp ok nodes=1 gres=$count"
