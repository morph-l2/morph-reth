#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

echo "Starting morph-reth (${MORPH_NETWORK})..."

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
  --chain "${MORPH_CHAIN}"
  --datadir "${RETH_DATA_DIR}"
  --http
  --http.addr "${RETH_HTTP_ADDR}"
  --http.port "${RETH_HTTP_PORT}"
  --http.api "web3,debug,eth,txpool,net,trace,admin,reth"
  --authrpc.addr "${RETH_AUTHRPC_ADDR}"
  --authrpc.port "${RETH_AUTHRPC_PORT}"
  --authrpc.jwtsecret "${JWT_SECRET}"
  --log.file.directory "$(dirname "${RETH_LOG_FILE}")"
  --log.file.filter info
  --rpc.eth-proof-window 1209600
  # Local testing: skip NAT discovery (UPnP/STUN) — not needed on a single host
  --disable-nat
  # Batch MDBX writes so they don't compete with Tendermint's LevelDB fsyncs
  # (v2.0.0 added persistence-backpressure-threshold, which must be > persistence-threshold)
  --engine.persistence-threshold 256
  --engine.memory-block-buffer-target 16
  --engine.persistence-backpressure-threshold 512
)

# Start morph-reth with pm2
pm2 start "${RETH_BIN}" --name morph-reth -- "${args[@]}"

echo "Logs: pm2 logs morph-reth"
