#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Morphnode configuration (binary is in ../morph/node, data is in local-test)
: "${MORPHNODE_BIN:=../morph/node/build/bin/morphnode}"
: "${NODE_HOME:=./local-test/node-data}"
: "${JWT_SECRET:=./local-test/jwt-secret.txt}"
: "${NODE_PID_FILE:=./local-test/node.pid}"
: "${NODE_LOG_FILE:=./local-test/node.log}"
: "${DOWNLOAD_CONFIG_IF_MISSING:=1}"
: "${MAINNET_CONFIG_ZIP_URL:=https://raw.githubusercontent.com/morph-l2/run-morph-node/main/mainnet/data.zip}"
: "${CONFIG_ZIP_PATH:=./local-test/mainnet-data.zip}"
: "${KEEP_CONFIG_ARTIFACTS:=0}"
: "${AUTO_RESET_ON_WRONG_BLOCK:=0}"

# Morph-Reth configuration
: "${RETH_BIN:=./target/release/morph-reth}"
: "${RETH_DATA_DIR:=./local-test/reth-data}"
: "${RETH_PID_FILE:=./local-test/reth.pid}"
: "${RETH_LOG_FILE:=./local-test/reth.log}"
: "${RETH_HTTP_ADDR:=127.0.0.1}"
: "${RETH_HTTP_PORT:=8545}"
: "${RETH_AUTHRPC_ADDR:=127.0.0.1}"
: "${RETH_AUTHRPC_PORT:=8551}"
: "${RETH_BOOTNODES:=}"
: "${MORPH_MAX_TX_PAYLOAD_BYTES:=122880}"
: "${MORPH_MAX_TX_PER_BLOCK:=}"

pid_running() {
  local pid="$1"
  kill -0 "${pid}" >/dev/null 2>&1
}

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

stop_by_pid_file() {
  local name="$1"
  local pid_file="$2"

  if [[ ! -f "${pid_file}" ]]; then
    echo "${name}: no pid file"
    return 0
  fi

  local pid
  pid="$(cat "${pid_file}")"
  if [[ -z "${pid}" ]]; then
    rm -f "${pid_file}"
    echo "${name}: empty pid file removed"
    return 0
  fi

  if pid_running "${pid}"; then
    kill "${pid}"
    for _ in {1..20}; do
      if ! pid_running "${pid}"; then
        break
      fi
      sleep 1
    done

    if pid_running "${pid}"; then
      kill -9 "${pid}"
    fi
    echo "${name}: stopped (pid ${pid})"
  else
    echo "${name}: not running (stale pid ${pid})"
  fi

  rm -f "${pid_file}"
}

rel_path() {
  local path="$1"
  echo "${path#./}"
}
