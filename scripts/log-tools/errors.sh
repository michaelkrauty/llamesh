#!/bin/bash
# Show recent errors and warnings from logs
# Usage: errors.sh [node] [date] [limit]
#   node: local, remote, both (default: both)
#   date: YYYY-MM-DD (default: today)
#   limit: max lines (default: 50)

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NODE="${1:-both}"
DATE="${2:-$(date +%Y-%m-%d)}"
LIMIT="${3:-50}"

fetch_and_filter() {
    local node="$1"
    "$SCRIPT_DIR/fetch-logs.sh" "$node" "$DATE" 2>/dev/null | \
        jq -r 'select(.level == "ERROR" or .level == "WARN") |
            "\(.timestamp | split(".")[0]) [\(.level)] \(.target): \(.fields.message // .fields.event // "no message")"' 2>/dev/null | \
        tail -n "$LIMIT"
}

if [[ "$NODE" == "both" ]]; then
    echo "=== local errors/warnings ==="
    fetch_and_filter "local" || echo "(no errors)"
    echo ""
    echo "=== remote errors/warnings ==="
    fetch_and_filter "remote" || echo "(no errors)"
else
    fetch_and_filter "$NODE"
fi
