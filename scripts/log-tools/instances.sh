#!/bin/bash
# Show instance lifecycle events: every event named instance_*, such as
# instance_spawn, instance_ready, instance_startup_failed, instance_exited,
# instance_wedged, and instance_terminate (with its reason: idle, capacity,
# error, drained, wedged, manual, cold_oom_stage1, or cold_oom_stage2).
# Usage: instances.sh [node] [date] [model]
#   node: local, remote, both (default: both)
#   date: YYYY-MM-DD, a UTC day (default: today in UTC)
#   model: filter by model name (optional, partial match, case-insensitive regex)
# Exits nonzero if either log query fails; diagnostics are written to stderr.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NODE="${1:-both}"
DATE="${2:-$(date -u +%Y-%m-%d)}"
MODEL="${3:-}"

filter_instances() {
    local node="$1"
    "$SCRIPT_DIR/fetch-logs.sh" "$node" "$DATE" | \
        jq -r --arg model "$MODEL" '
            select(.fields.event // "" | startswith("instance_")) |
            (.fields.model // .span.model // "?") as $name |
            select($model == "" or ($name | test($model; "i"))) |
            [
                (.timestamp | split(".")[0]),
                " \(.fields.event)",
                (if .fields.reason then " (\(.fields.reason))" else "" end),
                ": model=\($name)",
                (if .fields.profile then " profile=\(.fields.profile)" else "" end),
                " instance=\(.fields.instance_id // "?")",
                (if .fields.port then " port=\(.fields.port)" else "" end),
                (if .fields.startup_ms then " startup_ms=\(.fields.startup_ms)" else "" end),
                (if .fields.exit_status then " exit_status=\(.fields.exit_status)" else "" end),
                (if .fields.error then " (error: \(.fields.error))" else "" end)
            ] | add'
}

if [[ "$NODE" == "both" ]]; then
    status=0
    echo "=== local instances ==="
    filter_instances "local" || status=1
    echo ""
    echo "=== remote instances ==="
    filter_instances "remote" || status=1
    exit "$status"
else
    filter_instances "$NODE"
fi
