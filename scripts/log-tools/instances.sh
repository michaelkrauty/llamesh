#!/bin/bash
# Show instance lifecycle events (spawn, ready, evict, kill)
# Usage: instances.sh [node] [date] [model]
#   node: local, remote, both (default: both)
#   date: YYYY-MM-DD (default: today)
#   model: filter by model name (optional, partial match)

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NODE="${1:-both}"
DATE="${2:-$(date +%Y-%m-%d)}"
MODEL="${3:-}"

filter_instances() {
    local node="$1"
    local jq_filter='select(
        .fields.event == "instance_spawn" or
        .fields.event == "instance_ready" or
        .fields.event == "instance_evicted" or
        .fields.event == "instance_killed" or
        (.fields.message | test("Spawning instance"; "i") // false) or
        (.fields.message | test("Instance .* ready"; "i") // false) or
        (.fields.message | test("evict"; "i") // false)
    )'

    if [[ -n "$MODEL" ]]; then
        jq_filter="$jq_filter | select(.fields.model // .span.model // \"\" | test(\"$MODEL\"; \"i\"))"
    fi

    "$SCRIPT_DIR/fetch-logs.sh" "$node" "$DATE" 2>/dev/null | \
        jq -r "$jq_filter | \"\(.timestamp | split(\".\")[0]) \(.fields.event // \"info\"): model=\(.fields.model // .span.model // \"?\") instance=\(.fields.instance_id // \"?\") port=\(.fields.port // \"?\")\"" 2>/dev/null
}

if [[ "$NODE" == "both" ]]; then
    echo "=== local instances ==="
    filter_instances "local" || echo "(none)"
    echo ""
    echo "=== remote instances ==="
    filter_instances "remote" || echo "(none)"
else
    filter_instances "$NODE"
fi
