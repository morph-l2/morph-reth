#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

echo "=========================================="
echo "Starting Morph full node"
echo "=========================================="

# Step 1: Prepare configuration
echo "[1/4] Preparing configuration..."
"${SCRIPT_DIR}/prepare.sh"

# Step 2: Clean logs
echo "[2/4] Cleaning previous logs..."
cleanup_runtime_logs

# Step 3: Start morph-reth
echo "[3/4] Starting morph-reth..."
"${SCRIPT_DIR}/reth-start.sh"

# Wait for RPC to be ready
echo "Waiting for RPC..."
max_retries=60
retry_count=0
while [[ ${retry_count} -lt ${max_retries} ]]; do
  if curl -s -X POST \
    -H "Content-Type: application/json" \
    --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' \
    "http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}" >/dev/null 2>&1; then
    echo "RPC ready"
    break
  fi

  retry_count=$((retry_count + 1))
  if [[ $((retry_count % 10)) -eq 0 ]]; then
    echo "Still waiting... (${retry_count}/${max_retries})"
  fi
  sleep 1
done

if [[ ${retry_count} -eq ${max_retries} ]]; then
  echo "ERROR: RPC did not become ready after ${max_retries} seconds"
  echo "Check logs: $(rel_path "${RETH_LOG_FILE}")"
  exit 1
fi

# Step 4: Start morphnode
echo "[4/4] Starting morphnode..."
"${SCRIPT_DIR}/node-start.sh"

echo
echo "✓ Full node started"
echo "RPC: http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}"
echo "Check status: $(rel_path "${SCRIPT_DIR}")/status.sh"
