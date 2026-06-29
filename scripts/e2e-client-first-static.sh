#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e2e-lib.sh"
require_binaries
IFS='|' read -r dir tcp_port _udp_port _multicast server_config client_config uds count < <(make_single_fixture "client-first-static" 11)
start_backend "$client_config" "127.0.0.1:$tcp_port" "$dir/backend.log"
wait_for_socket "$uds"
sleep 1
start_server "$server_config" "$dir/server.log"
query_until "$uds" 1 "$count" "$dir/query.json"
echo "client-first-static ok nodes=1 gres=$count"
