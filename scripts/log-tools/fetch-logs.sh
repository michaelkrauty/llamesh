#!/bin/bash
# Fetch logs from local or remote node
# Usage: fetch-logs.sh [node] [date]
#   node: "local", "node-a", "node-b" (default: local)
#   date: YYYY-MM-DD, a UTC day (default: today in UTC)
#
# Configure via environment variables:
#   LLAMESH_LOCAL_LOG_DIR  - local log directory (default: ./logs)
#   LLAMESH_REMOTE_LOG_DIR - remote log directory (default: same as local)
#   LLAMESH_REMOTE_HOST    - SSH host for remote node (e.g., user@host)

set -euo pipefail

NODE="${1:-local}"
DATE="${2:-$(date -u +%Y-%m-%d)}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOCAL_LOG_DIR="${LLAMESH_LOCAL_LOG_DIR:-${SCRIPT_DIR}/logs}"
REMOTE_LOG_DIR="${LLAMESH_REMOTE_LOG_DIR:-./logs}"
REMOTE_HOST="${LLAMESH_REMOTE_HOST:-}"
LOG_FILE="proxy.log.${DATE}"

# Print the day's log file, or its gzip-compressed copy: with
# `logging.compression` enabled (the default), a file is compressed when it
# rotates and the uncompressed file is removed. Fails if neither exists.
# Reading a compressed file requires `gzip` on the node that holds it.
read_local_log() {
    if [[ -f "${LOCAL_LOG_DIR}/${LOG_FILE}" ]]; then
        cat "${LOCAL_LOG_DIR}/${LOG_FILE}"
    elif [[ -f "${LOCAL_LOG_DIR}/${LOG_FILE}.gz" ]]; then
        gzip -dc "${LOCAL_LOG_DIR}/${LOG_FILE}.gz"
    else
        return 1
    fi
}

# The same on the remote node. sshd runs the command through the remote
# user's login shell, which need not be POSIX, so the check runs in `sh -c`
# with the path as its argument. The path is substituted into the command
# unquoted, so the login shell expands a leading `~` in LLAMESH_REMOTE_LOG_DIR
# to the remote home directory.
read_remote_log() {
    # shellcheck disable=SC2029  # Expanded on the client side on purpose.
    ssh "$REMOTE_HOST" "sh -c 'if [ -f \"\$1\" ]; then cat \"\$1\"; elif [ -f \"\$1.gz\" ]; then gzip -dc \"\$1.gz\"; else exit 1; fi' sh ${REMOTE_LOG_DIR}/${LOG_FILE}"
}

case "$NODE" in
    local|node-a)
        read_local_log || {
            echo "Log file not found: ${LOCAL_LOG_DIR}/${LOG_FILE}[.gz]" >&2
            exit 1
        }
        ;;
    node-b|remote)
        if [[ -z "$REMOTE_HOST" ]]; then
            echo "Set LLAMESH_REMOTE_HOST (e.g., user@hostname) to fetch remote logs" >&2
            exit 1
        fi
        read_remote_log || {
            echo "Log file not found on ${REMOTE_HOST}: ${REMOTE_LOG_DIR}/${LOG_FILE}[.gz]" >&2
            exit 1
        }
        ;;
    both)
        echo "=== LOCAL (node-a) ===" >&2
        read_local_log || echo "No local logs for ${DATE}" >&2
        echo "" >&2
        if [[ -z "$REMOTE_HOST" ]]; then
            echo "Set LLAMESH_REMOTE_HOST to include remote logs" >&2
        else
            echo "=== REMOTE (node-b) ===" >&2
            read_remote_log || echo "No remote logs for ${DATE}" >&2
        fi
        ;;
    *)
        echo "Unknown node: $NODE (use: local, node-a, node-b, remote, both)" >&2
        exit 1
        ;;
esac
