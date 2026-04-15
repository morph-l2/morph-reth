#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

echo "=========================================="
echo "Morph ${MORPH_NETWORK} Node Status (with morph-reth)"
echo "=========================================="
echo

# Process status via pm2
echo "--- Process Status (pm2) ---"
pm2 list --no-color 2>/dev/null | grep -E "morph-reth|morph-node|name" || echo "No pm2 processes found"
echo

# morph-reth RPC status
echo "--- morph-reth RPC ---"

# Chain ID
echo -n "Chain ID: "
chain_id=$(curl -s -X POST \
  -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' \
  "http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}" 2>/dev/null | jq -r '.result // "error"')
if [[ "${chain_id}" != "error" && "${chain_id}" != "null" ]]; then
  # Convert hex to decimal
  chain_id_dec=$((chain_id))
  echo "${chain_id} (${chain_id_dec})"
else
  echo "unavailable"
fi

# Block number
echo -n "Block Number: "
block_num=$(curl -s -X POST \
  -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
  "http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}" 2>/dev/null | jq -r '.result // "error"')
if [[ "${block_num}" != "error" && "${block_num}" != "null" ]]; then
  block_num_dec=$((block_num))
  echo "${block_num} (${block_num_dec})"
else
  echo "unavailable"
fi

# Peer count
echo -n "Peer Count: "
peer_count=$(curl -s -X POST \
  -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"net_peerCount","params":[],"id":1}' \
  "http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}" 2>/dev/null | jq -r '.result // "error"')
if [[ "${peer_count}" != "error" && "${peer_count}" != "null" ]]; then
  peer_count_dec=$((peer_count))
  echo "${peer_count} (${peer_count_dec})"
else
  echo "unavailable"
fi

# Syncing status
echo -n "Syncing: "
syncing=$(curl -s -X POST \
  -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}' \
  "http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}" 2>/dev/null | jq -r '.result // "error"')
if [[ "${syncing}" == "false" ]]; then
  echo "false (synced)"
elif [[ "${syncing}" != "error" && "${syncing}" != "null" ]]; then
  echo "true (in progress)"
else
  echo "unavailable"
fi

echo

# morphnode status
echo "--- morphnode Status ---"
morphnode_status=$(curl -s "http://127.0.0.1:26657/status" 2>/dev/null)
if [[ -n "${morphnode_status}" ]]; then
  echo -n "Latest Block Height: "
  echo "${morphnode_status}" | jq -r '.result.sync_info.latest_block_height // "unknown"'
  echo -n "Latest Block Time: "
  echo "${morphnode_status}" | jq -r '.result.sync_info.latest_block_time // "unknown"'
  echo -n "Catching Up: "
  echo "${morphnode_status}" | jq -r '.result.sync_info.catching_up // "unknown"'
else
  echo "morphnode RPC not available"
fi

echo

# morphnode net_info
echo "--- morphnode Network ---"
morphnode_netinfo=$(curl -s "http://127.0.0.1:26657/net_info" 2>/dev/null)
if [[ -n "${morphnode_netinfo}" ]]; then
  echo -n "Peers: "
  echo "${morphnode_netinfo}" | jq -r '.result.n_peers // "unknown"'
else
  echo "morphnode RPC not available"
fi

echo
echo "=========================================="
echo "Logs:"
echo "  - pm2 logs morph-reth"
echo "  - pm2 logs morph-node"
echo "  - pm2 monit  (real-time monitoring)"
echo "=========================================="
