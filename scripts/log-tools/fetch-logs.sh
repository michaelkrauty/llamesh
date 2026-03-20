#!/bin/bash
# Fetch logs from local or remote node
# Usage: fetch-logs.sh [node] [date]
#   node: "local", "node-a", "node-b" (default: local)
#   date: YYYY-MM-DD (default: today)
#
# Configure via environment variables:
#   LLAMESH_LOCAL_LOG_DIR  - local log directory (default: ./logs)
#   LLAMESH_REMOTE_LOG_DIR - remote log directory (default: same as local)
#   LLAMESH_REMOTE_HOST    - SSH host for remote node (e.g., user@host)

set -euo pipefail

NODE="${1:-local}"
DATE="${2:-$(date +%Y-%m-%d)}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOCAL_LOG_DIR="${LLAMESH_LOCAL_LOG_DIR:-${SCRIPT_DIR}/logs}"
REMOTE_LOG_DIR="${LLAMESH_REMOTE_LOG_DIR:-./logs}"
REMOTE_HOST="${LLAMESH_REMOTE_HOST:-}"
LOG_FILE="proxy.log.${DATE}"

case "$NODE" in
    local|node-a)
        if [[ -f "${LOCAL_LOG_DIR}/${LOG_FILE}" ]]; then
            cat "${LOCAL_LOG_DIR}/${LOG_FILE}"
        else
            echo "Log file not found: ${LOCAL_LOG_DIR}/${LOG_FILE}" >&2
            exit 1
        fi
        ;;
    node-b|remote)
        if [[ -z "$REMOTE_HOST" ]]; then
            echo "Set LLAMESH_REMOTE_HOST (e.g., user@hostname) to fetch remote logs" >&2
            exit 1
        fi
        ssh "$REMOTE_HOST" "cat ${REMOTE_LOG_DIR}/${LOG_FILE} 2>/dev/null" || {
            echo "Log file not found on ${REMOTE_HOST}: ${REMOTE_LOG_DIR}/${LOG_FILE}" >&2
            exit 1
        }
        ;;
    both)
        echo "=== LOCAL (node-a) ===" >&2
        cat "${LOCAL_LOG_DIR}/${LOG_FILE}" 2>/dev/null || echo "No local logs for ${DATE}" >&2
        echo "" >&2
        if [[ -z "$REMOTE_HOST" ]]; then
            echo "Set LLAMESH_REMOTE_HOST to include remote logs" >&2
        else
            echo "=== REMOTE (node-b) ===" >&2
            ssh "$REMOTE_HOST" "cat ${REMOTE_LOG_DIR}/${LOG_FILE} 2>/dev/null" || echo "No remote logs for ${DATE}" >&2
        fi
        ;;
    *)
        echo "Unknown node: $NODE (use: local, node-a, node-b, remote, both)" >&2
        exit 1
        ;;
esac
