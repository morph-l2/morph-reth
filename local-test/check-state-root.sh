#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

# ─── Configuration ────────────────────────────────────────────────────────────
: "${STATE_ROOT_CHECK_BIN:=./target/release/state-root-check}"
: "${CHAIN:=morph}"
: "${GETH_RPC_URL:=}"
: "${CHECK_BLOCK:=}"          # specific block to check (default: latest)
: "${BISECT:=0}"              # set to 1 to bisect for first mismatch
: "${BISECT_FROM:=0}"         # bisect range start
: "${BISECT_TO:=}"            # bisect range end (default: latest)

# ─── Build ────────────────────────────────────────────────────────────────────

echo "Building state-root-check..."
cargo build --release -p state-root-check
echo "Build complete: ${STATE_ROOT_CHECK_BIN}"
echo

# ─── Run ──────────────────────────────────────────────────────────────────────

args=(
  --datadir "${RETH_DATA_DIR}"
  --chain "${CHAIN}"
)

if [[ "${BISECT}" == "1" ]]; then
  # Bisect mode: find first mismatch in a range
  if [[ -z "${GETH_RPC_URL}" ]]; then
    echo "ERROR: --bisect requires GETH_RPC_URL"
    echo "Usage: GETH_RPC_URL=http://localhost:8546 BISECT=1 $0"
    exit 1
  fi
  args+=(--bisect --from-block "${BISECT_FROM}")
  if [[ -n "${BISECT_TO}" ]]; then
    args+=(--to-block "${BISECT_TO}")
  else
    # Default to latest block from reth RPC
    latest=$(curl -s -X POST -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
      "http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}" 2>/dev/null | jq -r '.result // ""')
    if [[ -n "${latest}" && "${latest}" != "null" ]]; then
      args+=(--to-block "$(printf "%d" "${latest}")")
    else
      echo "ERROR: cannot determine latest block. Is reth running?"
      exit 1
    fi
  fi
  args+=(--geth-rpc-url "${GETH_RPC_URL}")

  echo "=== Bisect Mode ==="
  echo "  Range: ${BISECT_FROM} .. ${args[*]: -1}"
  echo "  Geth:  ${GETH_RPC_URL}"
elif [[ -n "${GETH_RPC_URL}" ]]; then
  # Compare mode: compare reth vs geth at a specific block
  args+=(--geth-rpc-url "${GETH_RPC_URL}")
  if [[ -n "${CHECK_BLOCK}" ]]; then
    args+=(--block "${CHECK_BLOCK}")
    echo "=== Compare Mode (block #${CHECK_BLOCK}) ==="
  else
    echo "=== Compare Mode (latest block) ==="
  fi
  echo "  Geth: ${GETH_RPC_URL}"
else
  # Local-only mode: just compute reth state root
  if [[ -n "${CHECK_BLOCK}" ]]; then
    args+=(--fresh-root-at "${CHECK_BLOCK}")
    echo "=== Local Mode (block #${CHECK_BLOCK}) ==="
  else
    echo "=== Local Mode (latest block) ==="
  fi
fi

echo "  Datadir: ${RETH_DATA_DIR}"
echo

"${STATE_ROOT_CHECK_BIN}" "${args[@]}"
