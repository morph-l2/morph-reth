#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Morphnode configuration (binary is in ../morph/node, data is in local-test)
: "${MORPHNODE_BIN:=../morph/node/build/bin/morphnode}"
: "${NODE_HOME:=./local-test/node-data}"
: "${JWT_SECRET:=./local-test/jwt-secret.txt}"
: "${NODE_LOG_FILE:=./local-test/node.log}"
: "${DOWNLOAD_CONFIG_IF_MISSING:=1}"
: "${MAINNET_CONFIG_ZIP_URL:=https://raw.githubusercontent.com/morph-l2/run-morph-node/main/mainnet/data.zip}"
: "${CONFIG_ZIP_PATH:=./local-test/mainnet-data.zip}"
: "${KEEP_CONFIG_ARTIFACTS:=0}"
: "${AUTO_RESET_ON_WRONG_BLOCK:=0}"

# Morph Geth configuration
: "${GETH_BIN:=../morph/go-ethereum/build/bin/geth}"
: "${GETH_DATA_DIR:=./local-test/geth-data}"
: "${GETH_LOG_FILE:=./local-test/geth.log}"

# Morph-Reth configuration
: "${RETH_BIN:=./target/release/morph-reth}"
: "${RETH_DATA_DIR:=./local-test/reth-data}"
: "${RETH_LOG_FILE:=./local-test/reth.log}"
: "${RETH_HTTP_ADDR:=0.0.0.0}"
: "${RETH_HTTP_PORT:=8545}"
: "${RETH_AUTHRPC_ADDR:=127.0.0.1}"
: "${RETH_AUTHRPC_PORT:=8551}"
: "${RETH_BOOTNODES:=}"
: "${MORPH_MAX_TX_PAYLOAD_BYTES:=122880}"
: "${MORPH_MAX_TX_PER_BLOCK:=}"

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

# pm2 helper functions
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
