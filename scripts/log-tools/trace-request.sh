#!/bin/bash
# Trace a request across nodes by request_id or session_id
# Usage: trace-request.sh <id> [date]
#   id: request_id or session_id (ULID format)
#   date: YYYY-MM-DD (default: today)

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ -z "${1:-}" ]]; then
    echo "Usage: trace-request.sh <request_id|session_id> [date]" >&2
    exit 1
fi

ID="$1"
DATE="${2:-$(date +%Y-%m-%d)}"

trace_node() {
    local node="$1"
    local label="$2"
    echo "=== $label ===" >&2
    "$SCRIPT_DIR/fetch-logs.sh" "$node" "$DATE" 2>/dev/null | \
        jq -r "select(.span.request_id == \"$ID\" or .span.session_id == \"$ID\" or .spans[]?.request_id == \"$ID\" or .spans[]?.session_id == \"$ID\") |
            \"\(.timestamp | split(\".\")[0]) [\(.level)] \(.target | split(\"::\")[-1]): \(.fields.message // .fields.event // \"?\")\"" 2>/dev/null || echo "(no matches)"
}

trace_node "local" "local"
echo ""
trace_node "remote" "remote"
