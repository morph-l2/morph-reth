#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

echo "Starting morph-geth..."

# Check prerequisites
pm2_check
check_binary "${GETH_BIN}" "cd ../morph/go-ethereum && make geth"

# Check if already running
if pm2_is_running "morph-geth"; then
  echo "morph-geth already running"
  pm2 describe morph-geth
  exit 0
fi

# Ensure data directory exists
mkdir -p "${GETH_DATA_DIR}"
mkdir -p "$(dirname "${GETH_LOG_FILE}")"

# Start morph-geth with pm2
pm2 start "${GETH_BIN}" --name morph-geth -- \
  --morph \
  --datadir "${GETH_DATA_DIR}" \
  --gcmode archive \
  --syncmode full \
  --http \
  --http.addr "${RETH_HTTP_ADDR}" \
  --http.port "${RETH_HTTP_PORT}" \
  --http.corsdomain "*" \
  --http.vhosts "*" \
  --http.api "web3,eth,debug,txpool,net,morph,engine" \
  --authrpc.addr "${RETH_AUTHRPC_ADDR}" \
  --authrpc.port "${RETH_AUTHRPC_PORT}" \
  --authrpc.vhosts "*" \
  --authrpc.jwtsecret "${JWT_SECRET}" \
  --nodiscover \
  --maxpeers 0 \
  --verbosity 3 \
  --log.filename "${GETH_LOG_FILE}"

echo "Logs: pm2 logs morph-geth"
