#!/bin/bash
# Show cluster health: peer connections, circuit breakers, gossip events
# Usage: cluster-health.sh [node] [date]
#   node: local, remote, both (default: both)
#   date: YYYY-MM-DD (default: today)

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NODE="${1:-both}"
DATE="${2:-$(date +%Y-%m-%d)}"

filter_cluster() {
    local node="$1"
    "$SCRIPT_DIR/fetch-logs.sh" "$node" "$DATE" 2>/dev/null | \
        jq -r 'select(
            .target | test("circuit_breaker|cluster|gossip|peer|mdns"; "i")
        ) | "\(.timestamp | split(".")[0]) [\(.level)] \(.target | split("::")[-1]): \(.fields.message // .fields.event // "?")"' 2>/dev/null
}

if [[ "$NODE" == "both" ]]; then
    echo "=== local cluster events ==="
    filter_cluster "local" | tail -30 || echo "(none)"
    echo ""
    echo "=== remote cluster events ==="
    filter_cluster "remote" | tail -30 || echo "(none)"
else
    filter_cluster "$NODE" | tail -50
fi
