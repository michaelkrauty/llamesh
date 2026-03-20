#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
SERVICE_NAME="llamesh"
UNIT_DIR="$HOME/.config/systemd/user"
UNIT_FILE="$UNIT_DIR/$SERVICE_NAME.service"
TEMPLATE="$REPO_DIR/deploy/$SERVICE_NAME.service"

echo "Installing $SERVICE_NAME systemd user unit"
echo "  Repo: $REPO_DIR"

# Create unit directory
mkdir -p "$UNIT_DIR"

# Generate unit file from template
sed "s|%REPO_PATH%|$REPO_DIR|g" "$TEMPLATE" > "$UNIT_FILE"
echo "  Unit: $UNIT_FILE"

# Reload and enable
systemctl --user daemon-reload
systemctl --user enable "$SERVICE_NAME"

echo ""
echo "Installed and enabled. Usage:"
echo "  systemctl --user start  $SERVICE_NAME"
echo "  systemctl --user stop   $SERVICE_NAME"
echo "  systemctl --user status $SERVICE_NAME"
echo "  journalctl --user -u $SERVICE_NAME -f"
