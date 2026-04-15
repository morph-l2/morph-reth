#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

echo "=========================================="
echo "Starting Morph ${MORPH_NETWORK} full node (pm2)"
echo "=========================================="

# Step 1: Check pm2
echo "[1/4] Checking pm2..."
pm2_check

# Step 2: Prepare configuration
echo "[2/4] Preparing configuration..."
"${SCRIPT_DIR}/prepare.sh"

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
  echo "Check logs: pm2 logs morph-reth"
  exit 1
fi

# Step 4: Start morphnode
echo "[4/4] Starting morphnode..."
"${SCRIPT_DIR}/node-start.sh"

echo
echo "Full node started (${MORPH_NETWORK})"
echo "RPC: http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}"
echo
echo "Useful commands:"
echo "  pm2 list              - view process status"
echo "  pm2 logs              - view all logs"
echo "  pm2 logs morph-reth   - view morph-reth logs"
echo "  pm2 logs morph-node   - view morphnode logs"
echo "  pm2 monit             - real-time monitoring"
echo "  pm2 save              - save process list for restart"
echo
echo "Check status: $(rel_path "${SCRIPT_DIR}")/status.sh"
