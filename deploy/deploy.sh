#!/bin/bash
set -euo pipefail

# Deploy brenn-forgejo-mcp release tarball to a target environment.
# Usage: deploy.sh <environment>
# Run from the unpacked tarball directory.
#
# Reads config from ~/.deploy/brenn-forgejo-mcp-<environment>.conf which defines:
#   BINARY_DIR   — where to install the brenn-forgejo-mcp binary
#   SERVICES     — (optional) array of systemd user services to stop/start during deploy
#                   (usually empty: the binary is launched per MCP session, so the next session picks up the new file)

ENV="${1:?Usage: deploy.sh <environment>}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CONF="$HOME/.deploy/brenn-forgejo-mcp-${ENV}.conf"

if [[ ! -f "$CONF" ]]; then
    echo "Error: config not found: $CONF" >&2
    exit 1
fi

source "$CONF"

echo "=== Deploying brenn-forgejo-mcp ${ENV} ==="
echo "Version: $(cat "${SCRIPT_DIR}/VERSION")"

if [[ ${#SERVICES[@]} -gt 0 ]] 2>/dev/null; then
    for svc in "${SERVICES[@]}"; do
        echo "Stopping ${svc}..."
        systemctl --user stop "$svc"
    done
fi

# Install binary — rm first in case it's held open by a running process
# (e.g. a running MCP server process).
echo "Installing brenn-forgejo-mcp to ${BINARY_DIR}..."
mkdir -p "$BINARY_DIR"
rm -f "${BINARY_DIR}/brenn-forgejo-mcp"
cp "${SCRIPT_DIR}/bin/brenn-forgejo-mcp" "${BINARY_DIR}/brenn-forgejo-mcp"

if [[ ${#SERVICES[@]} -gt 0 ]] 2>/dev/null; then
    for svc in "${SERVICES[@]}"; do
        echo "Starting ${svc}..."
        systemctl --user start "$svc"
    done
fi

echo "=== Deploy ${ENV} complete ==="
