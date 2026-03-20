#!/bin/bash
# Show recent log entries (tail equivalent for JSON logs)
# Usage: recent.sh [node] [lines] [level]
#   node: local, remote (default: local)
#   lines: number of lines (default: 20)
#   level: filter by level (INFO, WARN, ERROR, DEBUG - optional)

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NODE="${1:-local}"
LINES="${2:-20}"
LEVEL="${3:-}"

DATE="$(date +%Y-%m-%d)"

jq_filter='.'
if [[ -n "$LEVEL" ]]; then
    jq_filter="select(.level == \"$LEVEL\")"
fi

"$SCRIPT_DIR/fetch-logs.sh" "$NODE" "$DATE" 2>/dev/null | \
    jq -r "$jq_filter | \"\(.timestamp | split(\".\")[0]) [\(.level | .[0:4])] \(.target | split(\"::\")[-1]): \(.fields.message // .fields.event // \"?\")\"" 2>/dev/null | \
    tail -n "$LINES"
