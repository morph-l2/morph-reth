#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

echo "Starting morphnode (${MORPH_NETWORK})..."

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

# Build node args
args=(
  --home "${NODE_HOME}"
  --l2.jwt-secret "${JWT_SECRET}"
  --l2.eth "http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}"
  --l2.engine "http://${RETH_AUTHRPC_ADDR}:${RETH_AUTHRPC_PORT}"
  --l1.rpc "${MORPH_NODE_L1_RPC}"
  --sync.depositContractAddr "${MORPH_NODE_DEPOSIT_CONTRACT}"
  --log.filename "${NODE_LOG_FILE}"
)

# Hoodi requires rollup contract address
if [[ -n "${MORPH_NODE_ROLLUP_CONTRACT}" ]]; then
  args+=(--derivation.rollupAddress "${MORPH_NODE_ROLLUP_CONTRACT}")
fi

if [[ -n "${MORPH_NODE_EXTRA_FLAGS}" ]]; then
  # shellcheck disable=SC2206
  extra_flags=(${MORPH_NODE_EXTRA_FLAGS})
  args+=("${extra_flags[@]}")
fi

# Start morphnode with pm2
pm2 start "${MORPHNODE_BIN}" --name morph-node -- "${args[@]}"

echo "Logs: pm2 logs morph-node"
