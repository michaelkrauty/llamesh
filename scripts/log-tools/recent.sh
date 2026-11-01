#!/bin/bash
# Show recent log entries (tail equivalent for JSON logs)
# Usage: recent.sh [node] [lines] [level]
#   node: local, remote (default: local)
#   lines: number of lines (default: 20)
#   level: filter by level: TRACE, DEBUG, INFO, WARN, or ERROR (optional,
#          case-insensitive)
# Reads the current UTC day's log. Exits nonzero if the log query fails;
# diagnostics are written to stderr.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NODE="${1:-local}"
LINES="${2:-20}"
LEVEL="${3:-}"

case "$NODE" in
    local|node-a|remote|node-b) ;;
    *)
        echo "Unknown node: $NODE (use: local or remote)" >&2
        exit 1
        ;;
esac

DATE="$(date -u +%Y-%m-%d)"

"$SCRIPT_DIR/fetch-logs.sh" "$NODE" "$DATE" | \
    jq -r --arg level "$LEVEL" '
        select($level == "" or .level == ($level | ascii_upcase)) |
        "\(.timestamp | split(".")[0]) [\(.level | .[0:4])] \(.target | split("::")[-1]): \(.fields.message // .fields.event // "?")\(if .fields.error then " (error: \(.fields.error))" else "" end)"' | \
    tail -n "$LINES"
