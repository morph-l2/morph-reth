#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Network selection: mainnet (default) or hoodi.
# Accept as first positional arg (e.g. ./start-all.sh hoodi) or MORPH_NETWORK env var.
if [[ "${1:-}" == "mainnet" || "${1:-}" == "hoodi" ]]; then
  MORPH_NETWORK="$1"
  shift
fi
: "${MORPH_NETWORK:=mainnet}"

if [[ "${MORPH_NETWORK}" != "mainnet" && "${MORPH_NETWORK}" != "hoodi" ]]; then
  echo "ERROR: MORPH_NETWORK must be 'mainnet' or 'hoodi', got '${MORPH_NETWORK}'"
  exit 1
fi

# Export so child processes (reth-start.sh, node-start.sh, etc.) inherit the value.
export MORPH_NETWORK

# ─── Network-specific defaults ────────────────────────────────────────────────

if [[ "${MORPH_NETWORK}" == "mainnet" ]]; then
  : "${MORPH_NODE_L1_RPC:=${MORPH_NODE_L1_ETH_RPC:-https://ethereum.publicnode.com}}"
  : "${MORPH_NODE_DEPOSIT_CONTRACT:=${MORPH_NODE_SYNC_DEPOSIT_CONTRACT_ADDRESS:-0x3931ade842f5bb8763164bdd81e5361dce6cc1ef}}"
  : "${MORPH_NODE_ROLLUP_CONTRACT:=}"
  : "${MORPH_NODE_EXTRA_FLAGS:=--mainnet}"
  : "${CONFIG_ZIP_URL:=https://raw.githubusercontent.com/morph-l2/run-morph-node/main/mainnet/data.zip}"
  : "${CONFIG_ZIP_PATH:=./local-test/mainnet-data.zip}"
  : "${MORPH_CHAIN:=mainnet}"
else
  : "${MORPH_NODE_L1_RPC:=${MORPH_NODE_L1_ETH_RPC:-https://ethereum-hoodi-rpc.publicnode.com}}"
  : "${MORPH_NODE_DEPOSIT_CONTRACT:=${MORPH_NODE_SYNC_DEPOSIT_CONTRACT_ADDRESS:-0xd7f39d837f4790b215ba67e0ab63665912648dbe}}"
  : "${MORPH_NODE_ROLLUP_CONTRACT:=0x57e0e6dde89dc52c01fe785774271504b1e04664}"
  : "${MORPH_NODE_EXTRA_FLAGS:=}"
  : "${CONFIG_ZIP_URL:=https://raw.githubusercontent.com/morph-l2/run-morph-node/main/hoodi/data.zip}"
  : "${CONFIG_ZIP_PATH:=./local-test/hoodi-data.zip}"
  : "${MORPH_CHAIN:=hoodi}"
fi

# ─── Shared configuration ─────────────────────────────────────────────────────

: "${MORPHNODE_BIN:=../morph/node/build/bin/morphnode}"
: "${NODE_HOME:=./local-test/${MORPH_NETWORK}/node-data}"
: "${JWT_SECRET:=./local-test/jwt-secret.txt}"
: "${NODE_LOG_FILE:=./local-test/${MORPH_NETWORK}/node.log}"
: "${DOWNLOAD_CONFIG_IF_MISSING:=1}"
: "${KEEP_CONFIG_ARTIFACTS:=0}"
: "${AUTO_RESET_ON_WRONG_BLOCK:=0}"

: "${RETH_BIN:=./target/release/morph-reth}"
: "${RETH_DATA_DIR:=./local-test/${MORPH_NETWORK}/reth-data}"
: "${RETH_LOG_FILE:=./local-test/${MORPH_NETWORK}/reth.log}"
: "${RETH_HTTP_ADDR:=0.0.0.0}"
: "${RETH_HTTP_PORT:=8545}"
: "${RETH_AUTHRPC_ADDR:=127.0.0.1}"
: "${RETH_AUTHRPC_PORT:=8551}"
: "${RETH_BOOTNODES:=}"
: "${MORPH_MAX_TX_PAYLOAD_BYTES:=122880}"
: "${MORPH_MAX_TX_PER_BLOCK:=}"

# ─── Helper functions ─────────────────────────────────────────────────────────

check_binary() {
  local bin_path="$1"
  local build_hint="$2"
  if [[ ! -x "${bin_path}" ]]; then
    echo "Missing executable: ${bin_path}"
    echo "Build hint: ${build_hint}"
    return 1
  fi
}

cleanup_runtime_logs() {
  rm -f "${NODE_LOG_FILE}" "${RETH_LOG_FILE}"
  rm -rf "$(dirname "${RETH_LOG_FILE}")"/{[0-9]*,*.log*}
}

pm2_check() {
  if ! command -v pm2 &> /dev/null; then
    echo "ERROR: pm2 is not installed"
    echo "Install with: npm install -g pm2"
    return 1
  fi
}

pm2_is_running() {
  local name="$1"
  pm2 describe "${name}" &>/dev/null && \
    [[ "$(pm2 jlist 2>/dev/null | jq -r ".[] | select(.name==\"${name}\") | .pm2_env.status")" == "online" ]]
}

pm2_stop() {
  local name="$1"
  if pm2 describe "${name}" &>/dev/null; then
    pm2 stop "${name}" 2>/dev/null || true
    pm2 delete "${name}" 2>/dev/null || true
    echo "${name}: stopped"
  else
    echo "${name}: not running"
  fi
}

rel_path() {
  local path="$1"
  echo "${path#./}"
}
