#!/bin/bash
# Show recent errors and warnings from logs
# Usage: errors.sh [node] [date] [limit]
#   node: local, remote, both (default: both)
#   date: YYYY-MM-DD (default: today)
#   limit: max lines (default: 50)
# Exits nonzero if either log query fails; diagnostics are written to stderr.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NODE="${1:-both}"
DATE="${2:-$(date +%Y-%m-%d)}"
LIMIT="${3:-50}"

fetch_and_filter() {
    local node="$1"
    "$SCRIPT_DIR/fetch-logs.sh" "$node" "$DATE" | \
        jq -r 'select(.level == "ERROR" or .level == "WARN") |
            "\(.timestamp | split(".")[0]) [\(.level)] \(.target): \(.fields.message // .fields.event // "no message")"' | \
        tail -n "$LIMIT"
}

if [[ "$NODE" == "both" ]]; then
    status=0
    echo "=== local errors/warnings ==="
    fetch_and_filter "local" || status=1
    echo ""
    echo "=== remote errors/warnings ==="
    fetch_and_filter "remote" || status=1
    exit "$status"
else
    fetch_and_filter "$NODE"
fi
