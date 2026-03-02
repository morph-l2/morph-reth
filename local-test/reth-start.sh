#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

echo "Starting morph-reth..."

# Check prerequisites
pm2_check
check_binary "${RETH_BIN}" "cargo build --release --bin morph-reth"

# Check if already running
if pm2_is_running "morph-reth"; then
  echo "morph-reth already running"
  pm2 describe morph-reth
  exit 0
fi

# Ensure data directory exists
mkdir -p "${RETH_DATA_DIR}"
mkdir -p "$(dirname "${RETH_LOG_FILE}")"

# Build command arguments
args=(
  node
  --chain mainnet
  --datadir "${RETH_DATA_DIR}"
  --http
  --http.addr "${RETH_HTTP_ADDR}"
  --http.port "${RETH_HTTP_PORT}"
  --http.api "web3,debug,eth,txpool,net,trace"
  --authrpc.addr "${RETH_AUTHRPC_ADDR}"
  --authrpc.port "${RETH_AUTHRPC_PORT}"
  --authrpc.jwtsecret "${JWT_SECRET}"
  --log.file.directory "$(dirname "${RETH_LOG_FILE}")"
  --log.file.filter info
  --morph.max-tx-payload-bytes "${MORPH_MAX_TX_PAYLOAD_BYTES}"
  --nat none
  --engine.legacy-state-root
)

# Add optional max-tx-per-block if configured
if [[ -n "${MORPH_MAX_TX_PER_BLOCK}" ]]; then
  args+=(--morph.max-tx-per-block "${MORPH_MAX_TX_PER_BLOCK}")
fi

# Add bootnodes if configured
if [[ -n "${RETH_BOOTNODES}" ]]; then
  args+=(--bootnodes "${RETH_BOOTNODES}")
fi

# Start morph-reth with pm2
pm2 start "${RETH_BIN}" --name morph-reth -- "${args[@]}"

echo "Logs: pm2 logs morph-reth"
