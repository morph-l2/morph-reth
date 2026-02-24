#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

echo "Starting morphnode..."

# Check if morphnode binary exists
check_binary "${MORPHNODE_BIN}" "cd ../morph/node && make build"

# Check if already running
if [[ -f "${NODE_PID_FILE}" ]]; then
  old_pid="$(cat "${NODE_PID_FILE}")"
  if [[ -n "${old_pid}" ]] && pid_running "${old_pid}"; then
    echo "morphnode already running (pid ${old_pid})"
    exit 0
  fi
  rm -f "${NODE_PID_FILE}"
fi

# Ensure log directory exists
mkdir -p "$(dirname "${NODE_LOG_FILE}")"

# Start morphnode in background
nohup "${MORPHNODE_BIN}" \
  --home "${NODE_HOME}" \
  --l2.jwt-secret "${JWT_SECRET}" \
  --l2.eth "http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}" \
  --l2.engine "http://${RETH_AUTHRPC_ADDR}:${RETH_AUTHRPC_PORT}" \
  --log.filename "${NODE_LOG_FILE}" \
  >> "${NODE_LOG_FILE}" 2>&1 &

echo $! > "${NODE_PID_FILE}"
echo "✓ morphnode started (pid $(cat "${NODE_PID_FILE}"))"
echo "Logs: $(rel_path "${NODE_LOG_FILE}")"
