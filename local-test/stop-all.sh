#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

# Stop in reverse order: morphnode first, then morph-reth
stop_by_pid_file "morphnode" "${NODE_PID_FILE}"
stop_by_pid_file "morph-reth" "${RETH_PID_FILE}"
