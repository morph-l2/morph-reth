#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

# Stop in reverse order: morphnode first, then morph-reth
pm2_stop "morph-node"
pm2_stop "morph-reth"

echo "All services stopped"
