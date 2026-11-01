#!/bin/bash
# Show cluster health: peer connections, circuit breakers, gossip, discovery,
# and Noise handshake events
# Usage: cluster-health.sh [node] [date]
#   node: local, remote, both (default: both)
#   date: YYYY-MM-DD, a UTC day (default: today in UTC)
# Shows the last 30 events per node for both, or the last 50 for one node.
# Exits nonzero if either log query fails; diagnostics are written to stderr.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NODE="${1:-both}"
DATE="${2:-$(date -u +%Y-%m-%d)}"

filter_cluster() {
    local node="$1"
    local limit="$2"
    "$SCRIPT_DIR/fetch-logs.sh" "$node" "$DATE" | \
        jq -r 'select(
            .target | test("circuit_breaker|cluster|gossip|peer|mdns|noise"; "i")
        ) | "\(.timestamp | split(".")[0]) [\(.level)] \(.target | split("::")[-1]): \(.fields.message // .fields.event // "?")\(if .fields.error then " (error: \(.fields.error))" else "" end)"' | \
        tail -n "$limit"
}

if [[ "$NODE" == "both" ]]; then
    status=0
    echo "=== local cluster events ==="
    filter_cluster "local" 30 || status=1
    echo ""
    echo "=== remote cluster events ==="
    filter_cluster "remote" 30 || status=1
    exit "$status"
else
    filter_cluster "$NODE" 50
fi
