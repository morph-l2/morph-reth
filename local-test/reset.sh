#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

assume_yes=0
if [[ "${1:-}" == "--yes" ]]; then
  assume_yes=1
fi

echo "=========================================="
echo "Reset local sync state (morph-reth + node)"
echo "=========================================="
echo
echo "This will remove:"
echo "  - ${RETH_DATA_DIR}/db"
echo "  - ${RETH_DATA_DIR}/static_files"
echo "  - ${GETH_DATA_DIR}/geth"
echo "  - ${NODE_HOME}/data"
echo
echo "This keeps:"
echo "  - ${NODE_HOME}/config (genesis/keys)"
echo "  - ${GETH_DATA_DIR}/keystore"
echo "  - log files"
echo

if [[ ${assume_yes} -ne 1 ]]; then
  read -r -p "Continue? [y/N] " confirm
  if [[ "${confirm}" != "y" && "${confirm}" != "Y" ]]; then
    echo "Cancelled."
    exit 0
  fi
fi

"${SCRIPT_DIR}/stop-all.sh" || true
pm2_stop "morph-geth" 2>/dev/null || true

rm -rf "${RETH_DATA_DIR}/db" "${RETH_DATA_DIR}/static_files" "${GETH_DATA_DIR}/geth" "${NODE_HOME}/data"
mkdir -p "${RETH_DATA_DIR}" "${GETH_DATA_DIR}" "${NODE_HOME}/data"

cat > "${NODE_HOME}/data/priv_validator_state.json" <<'EOF'
{"height":"0","round":0,"step":0}
EOF
cleanup_runtime_logs

echo
echo "Reset finished."
echo "Next steps:"
echo "  1) $(rel_path "${SCRIPT_DIR}")/prepare.sh"
echo "  2) $(rel_path "${SCRIPT_DIR}")/start-all.sh"
