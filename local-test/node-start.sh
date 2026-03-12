#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

echo "Starting morphnode..."

# Check prerequisites
pm2_check
check_binary "${MORPHNODE_BIN}" "cd ../morph/node && make build"

# Check if already running
if pm2_is_running "morph-node"; then
  echo "morphnode already running"
  pm2 describe morph-node
  exit 0
fi

# Ensure log directory exists
mkdir -p "$(dirname "${NODE_LOG_FILE}")"

# Start morphnode with pm2
pm2 start "${MORPHNODE_BIN}" --name morph-node -- \
  --home "${NODE_HOME}" \
  --l2.jwt-secret "${JWT_SECRET}" \
  --l2.eth "http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}" \
  --l2.engine "http://${RETH_AUTHRPC_ADDR}:${RETH_AUTHRPC_PORT}" \
  --log.filename "${NODE_LOG_FILE}"

echo "Logs: pm2 logs morph-node"
