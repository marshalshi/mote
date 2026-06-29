#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────
# Mote Docker sandbox — convenience wrapper.
#
# Usage:
#   ./docker/run.sh                    # mount CWD as workspace
#   ./docker/run.sh /path/to/project   # mount specific directory
#   ./docker/run.sh . --server         # pass extra args to mote-tui
#
# Environment:
#   MOTE_IMAGE   Docker image tag (default: mote:latest)
# ─────────────────────────────────────────────────────────────
set -euo pipefail

IMAGE_NAME="${MOTE_IMAGE:-mote:latest}"

# Parse first argument as optional workspace path; rest go to mote.
if [ $# -gt 0 ]; then
    WORKSPACE="$1"
    shift
else
    WORKSPACE="$(pwd)"
fi

# Resolve workspace to an absolute path.
if [ -d "$WORKSPACE" ]; then
    WORKSPACE="$(cd "$WORKSPACE" && pwd)"
else
    echo "Error: directory '$WORKSPACE' does not exist" >&2
    exit 1
fi

# Host config directory (mounted into the container).
CONFIG_DIR="${HOME}/.config/mote"

echo "────────────────────────────────────────"
echo " Mote Docker Sandbox"
echo "────────────────────────────────────────"
echo " Workspace:  $WORKSPACE  →  /workspace"
echo " Config:     $CONFIG_DIR"
echo " Image:      $IMAGE_NAME"
echo "────────────────────────────────────────"

exec docker run -it --rm \
    -v "${WORKSPACE}:/workspace" \
    -w /workspace \
    -v "${CONFIG_DIR}:/root/.config/mote" \
    "${IMAGE_NAME}" "$@"
