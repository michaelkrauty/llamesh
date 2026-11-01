#!/bin/bash
# Trace a request across nodes by request_id or session_id
# Usage: trace-request.sh <id> [date] [node]
#   id: request_id or session_id (generated IDs are ULIDs; caller-supplied IDs need not be)
#   date: YYYY-MM-DD, a UTC day (default: today in UTC)
#   node: local, remote, both (default: both)
# Prints "(no matches)" on stderr for a node whose log has no entry for the
# ID. Exits nonzero if a log query fails; diagnostics are written to stderr.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ -z "${1:-}" ]]; then
    echo "Usage: trace-request.sh <request_id|session_id> [date] [node]" >&2
    exit 1
fi

ID="$1"
DATE="${2:-$(date -u +%Y-%m-%d)}"
NODE="${3:-both}"

trace_node() {
    local node="$1"
    local stages
    echo "=== $node ===" >&2
    # Stream the matches, so those before a malformed line are still shown;
    # the final awk exits 3 when there were none.
    set +e
    "$SCRIPT_DIR/fetch-logs.sh" "$node" "$DATE" | \
        jq -r --arg id "$ID" '
            select(.span.request_id == $id or .span.session_id == $id or
                any(.spans[]?; .request_id == $id or .session_id == $id)) |
            "\(.timestamp | split(".")[0]) [\(.level)] \(.target | split("::")[-1]): \(.fields.message // .fields.event // "?")\(if .fields.error then " (error: \(.fields.error))" else "" end)"' | \
        awk '{ print } END { if (NR == 0) exit 3 }'
    stages=("${PIPESTATUS[@]}")
    set -e
    if (( stages[0] != 0 || stages[1] != 0 )); then
        return 1
    fi
    if (( stages[2] == 3 )); then
        echo "(no matches)" >&2
    fi
}

if [[ "$NODE" == "both" ]]; then
    status=0
    trace_node "local" || status=1
    echo ""
    trace_node "remote" || status=1
    exit "$status"
else
    trace_node "$NODE"
fi
