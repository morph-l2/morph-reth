#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

echo "Starting morph-reth..."

# Check if reth binary exists
check_binary "${RETH_BIN}" "cargo build --release --bin morph-reth"

# Check if already running
if [[ -f "${RETH_PID_FILE}" ]]; then
  old_pid="$(cat "${RETH_PID_FILE}")"
  if [[ -n "${old_pid}" ]] && pid_running "${old_pid}"; then
    echo "morph-reth already running (pid ${old_pid})"
    exit 0
  fi
  rm -f "${RETH_PID_FILE}"
fi

# Ensure data directory exists
mkdir -p "${RETH_DATA_DIR}"
mkdir -p "$(dirname "${RETH_LOG_FILE}")"

# Build command array
cmd=(
  "${RETH_BIN}"
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
  --morph.max-tx-payload-bytes "${MORPH_MAX_TX_PAYLOAD_BYTES}"
)

# Add optional max-tx-per-block if configured
if [[ -n "${MORPH_MAX_TX_PER_BLOCK}" ]]; then
  cmd+=(--morph.max-tx-per-block "${MORPH_MAX_TX_PER_BLOCK}")
fi

# Add bootnodes if configured
if [[ -n "${RETH_BOOTNODES}" ]]; then
  cmd+=(--bootnodes "${RETH_BOOTNODES}")
fi

# Start morph-reth in background
nohup "${cmd[@]}" >> "${RETH_LOG_FILE}" 2>&1 &

echo $! > "${RETH_PID_FILE}"
echo "✓ morph-reth started (pid $(cat "${RETH_PID_FILE}"))"
echo "Logs: $(rel_path "${RETH_LOG_FILE}")"
