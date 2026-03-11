#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

# ─── Configuration ────────────────────────────────────────────────────────────
: "${TEST_DURATION:=120}"       # seconds to run each node
: "${RPC_WAIT_TIMEOUT:=60}"    # seconds to wait for RPC readiness
: "${SAMPLE_INTERVAL:=10}"     # seconds between BPS samples
: "${SKIP_GETH:=0}"            # set to 1 to skip geth test
: "${SKIP_RETH:=0}"            # set to 1 to skip reth test
: "${MAINNET_TIP:=21100000}"   # approximate current mainnet tip for ETA calc

# ─── Helpers ──────────────────────────────────────────────────────────────────

get_block_number() {
  local result
  result=$(curl -s -X POST \
    -H "Content-Type: application/json" \
    --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    "http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}" 2>/dev/null | jq -r '.result // ""')
  if [[ -n "${result}" && "${result}" != "null" ]]; then
    printf "%d" "${result}"
  else
    echo "0"
  fi
}

wait_for_rpc() {
  local name="$1"
  local retries=0
  echo -n "  Waiting for ${name} RPC..."
  while [[ ${retries} -lt ${RPC_WAIT_TIMEOUT} ]]; do
    if curl -s -X POST \
      -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' \
      "http://${RETH_HTTP_ADDR}:${RETH_HTTP_PORT}" >/dev/null 2>&1; then
      echo " ready"
      return 0
    fi
    retries=$((retries + 1))
    sleep 1
  done
  echo " TIMEOUT"
  return 1
}

# Collect BPS samples over the test duration by polling eth_blockNumber.
# Outputs: start_block end_block elapsed_seconds avg_bps peak_bps
run_bps_sampling() {
  local name="$1"
  local duration="$2"
  local interval="${SAMPLE_INTERVAL}"

  local start_block end_block prev_block
  local elapsed=0 sample_count=0
  local total_bps=0 peak_bps=0

  start_block=$(get_block_number)
  prev_block=${start_block}

  echo "  Sampling BPS for ${name} (${duration}s, every ${interval}s)..."

  while [[ ${elapsed} -lt ${duration} ]]; do
    sleep "${interval}"
    elapsed=$((elapsed + interval))

    local current_block
    current_block=$(get_block_number)
    local delta=$((current_block - prev_block))
    local bps
    bps=$(echo "scale=2; ${delta} / ${interval}" | bc)

    sample_count=$((sample_count + 1))
    total_bps=$(echo "${total_bps} + ${bps}" | bc)

    # Track peak
    if [[ $(echo "${bps} > ${peak_bps}" | bc -l) -eq 1 ]]; then
      peak_bps=${bps}
    fi

    printf "    [%3ds] block=%d  delta=+%d  bps=%.2f\n" "${elapsed}" "${current_block}" "${delta}" "${bps}"
    prev_block=${current_block}
  done

  end_block=$(get_block_number)
  local total_blocks=$((end_block - start_block))
  local avg_bps
  avg_bps=$(echo "scale=2; ${total_blocks} / ${duration}" | bc)

  echo "  ${name} sampling complete: ${start_block} -> ${end_block} (+${total_blocks} blocks)"

  # Export results via global vars (bash doesn't have return values for multiple)
  RESULT_START_BLOCK=${start_block}
  RESULT_END_BLOCK=${end_block}
  RESULT_TOTAL_BLOCKS=${total_blocks}
  RESULT_AVG_BPS=${avg_bps}
  RESULT_PEAK_BPS=${peak_bps}
}

# Full reset: stop everything, clean EL + node data, re-prepare config.
full_reset() {
  echo "  Resetting all data..."
  pm2_stop "morph-geth" 2>/dev/null || true
  pm2_stop "morph-reth" 2>/dev/null || true
  pm2_stop "morph-node" 2>/dev/null || true

  rm -rf "${RETH_DATA_DIR}/db" "${RETH_DATA_DIR}/static_files"
  rm -rf "${GETH_DATA_DIR}/geth"
  rm -rf "${NODE_HOME}/data"
  mkdir -p "${RETH_DATA_DIR}" "${GETH_DATA_DIR}" "${NODE_HOME}/data"

  cat > "${NODE_HOME}/data/priv_validator_state.json" <<'EOF'
{"height":"0","round":0,"step":0}
EOF

  cleanup_runtime_logs
  echo "  Reset complete"
}

stop_all() {
  pm2_stop "morph-geth" 2>/dev/null || true
  pm2_stop "morph-reth" 2>/dev/null || true
  pm2_stop "morph-node" 2>/dev/null || true
}

format_duration() {
  local total_seconds=$1
  local days=$((total_seconds / 86400))
  local hours=$(( (total_seconds % 86400) / 3600 ))
  local minutes=$(( (total_seconds % 3600) / 60 ))
  if [[ ${days} -gt 0 ]]; then
    printf "%dd %dh %dm" "${days}" "${hours}" "${minutes}"
  elif [[ ${hours} -gt 0 ]]; then
    printf "%dh %dm" "${hours}" "${minutes}"
  else
    printf "%dm" "${minutes}"
  fi
}

# ─── Main ─────────────────────────────────────────────────────────────────────

echo "=========================================="
echo "  Morph Sync Speed Test: Geth vs Reth"
echo "=========================================="
echo "  Duration per node:  ${TEST_DURATION}s"
echo "  Sample interval:    ${SAMPLE_INTERVAL}s"
echo "  Mainnet tip (est):  ${MAINNET_TIP}"
echo "=========================================="
echo

# Check prerequisites
pm2_check
check_binary "${MORPHNODE_BIN}" "cd ../morph/node && make build"

# Results storage
GETH_AVG_BPS=0
GETH_PEAK_BPS=0
GETH_START=0
GETH_END=0
GETH_TOTAL=0

RETH_AVG_BPS=0
RETH_PEAK_BPS=0
RETH_START=0
RETH_END=0
RETH_TOTAL=0

# ─── Phase 1: Test Geth ──────────────────────────────────────────────────────

if [[ "${SKIP_GETH}" != "1" ]]; then
  check_binary "${GETH_BIN}" "cd ../morph/go-ethereum && make geth"

  echo "=== Phase 1: Testing Geth ==="
  echo

  # Reset
  full_reset

  # Prepare config (need jwt-secret and node config)
  "${SCRIPT_DIR}/prepare.sh" 2>/dev/null

  # Start geth
  echo "  Starting morph-geth..."
  "${SCRIPT_DIR}/geth-start.sh"

  # Wait for geth RPC
  wait_for_rpc "geth"

  # Start morphnode
  echo "  Starting morphnode..."
  "${SCRIPT_DIR}/node-start.sh"

  # Brief warmup to let morphnode establish connection
  echo "  Warming up (10s)..."
  sleep 10

  # Run BPS sampling
  run_bps_sampling "geth" "${TEST_DURATION}"
  GETH_AVG_BPS=${RESULT_AVG_BPS}
  GETH_PEAK_BPS=${RESULT_PEAK_BPS}
  GETH_START=${RESULT_START_BLOCK}
  GETH_END=${RESULT_END_BLOCK}
  GETH_TOTAL=${RESULT_TOTAL_BLOCKS}

  # Collect morphnode BPS logs
  echo
  echo "  morphnode Block Sync Rate samples (geth):"
  grep "Block Sync Rate" "${NODE_LOG_FILE}" 2>/dev/null | tail -5 | while read -r line; do
    echo "    ${line}"
  done

  # Stop everything
  echo
  echo "  Stopping geth test..."
  stop_all

  echo
  echo "=== Geth test complete ==="
  echo
else
  echo "=== Skipping Geth test (SKIP_GETH=1) ==="
  echo
fi

# ─── Phase 2: Test Reth ──────────────────────────────────────────────────────

if [[ "${SKIP_RETH}" != "1" ]]; then
  check_binary "${RETH_BIN}" "cargo build --release --bin morph-reth"

  echo "=== Phase 2: Testing Reth ==="
  echo

  # Reset
  full_reset

  # Prepare config
  "${SCRIPT_DIR}/prepare.sh" 2>/dev/null

  # Start reth
  echo "  Starting morph-reth..."
  "${SCRIPT_DIR}/reth-start.sh"

  # Wait for reth RPC
  wait_for_rpc "reth"

  # Start morphnode
  echo "  Starting morphnode..."
  "${SCRIPT_DIR}/node-start.sh"

  # Brief warmup
  echo "  Warming up (10s)..."
  sleep 10

  # Run BPS sampling
  run_bps_sampling "reth" "${TEST_DURATION}"
  RETH_AVG_BPS=${RESULT_AVG_BPS}
  RETH_PEAK_BPS=${RESULT_PEAK_BPS}
  RETH_START=${RESULT_START_BLOCK}
  RETH_END=${RESULT_END_BLOCK}
  RETH_TOTAL=${RESULT_TOTAL_BLOCKS}

  # Collect morphnode BPS logs
  echo
  echo "  morphnode Block Sync Rate samples (reth):"
  grep "Block Sync Rate" "${NODE_LOG_FILE}" 2>/dev/null | tail -5 | while read -r line; do
    echo "    ${line}"
  done

  # Stop everything
  echo
  echo "  Stopping reth test..."
  stop_all

  echo
  echo "=== Reth test complete ==="
  echo
else
  echo "=== Skipping Reth test (SKIP_RETH=1) ==="
  echo
fi

# ─── Results ──────────────────────────────────────────────────────────────────

echo "=========================================="
echo "  RESULTS"
echo "=========================================="
echo

printf "%-20s  %12s  %12s\n" "" "Geth" "Reth"
printf "%-20s  %12s  %12s\n" "---" "---" "---"
printf "%-20s  %12d  %12d\n" "Start Block" "${GETH_START}" "${RETH_START}"
printf "%-20s  %12d  %12d\n" "End Block" "${GETH_END}" "${RETH_END}"
printf "%-20s  %12d  %12d\n" "Total Blocks" "${GETH_TOTAL}" "${RETH_TOTAL}"
printf "%-20s  %12s  %12s\n" "Avg BPS" "${GETH_AVG_BPS}" "${RETH_AVG_BPS}"
printf "%-20s  %12s  %12s\n" "Peak BPS" "${GETH_PEAK_BPS}" "${RETH_PEAK_BPS}"

# ETA calculation
echo
echo "--- Estimated Full Sync Time (to block ${MAINNET_TIP}) ---"
if [[ $(echo "${GETH_AVG_BPS} > 0" | bc -l) -eq 1 ]]; then
  geth_eta_seconds=$(echo "scale=0; ${MAINNET_TIP} / ${GETH_AVG_BPS}" | bc)
  printf "Geth:  %s  (at %.2f bps)\n" "$(format_duration "${geth_eta_seconds}")" "${GETH_AVG_BPS}"
else
  echo "Geth:  N/A (no data)"
fi

if [[ $(echo "${RETH_AVG_BPS} > 0" | bc -l) -eq 1 ]]; then
  reth_eta_seconds=$(echo "scale=0; ${MAINNET_TIP} / ${RETH_AVG_BPS}" | bc)
  printf "Reth:  %s  (at %.2f bps)\n" "$(format_duration "${reth_eta_seconds}")" "${RETH_AVG_BPS}"
else
  echo "Reth:  N/A (no data)"
fi

# Winner
echo
if [[ $(echo "${GETH_AVG_BPS} > 0 && ${RETH_AVG_BPS} > 0" | bc -l) -eq 1 ]]; then
  if [[ $(echo "${RETH_AVG_BPS} > ${GETH_AVG_BPS}" | bc -l) -eq 1 ]]; then
    speedup=$(echo "scale=2; ${RETH_AVG_BPS} / ${GETH_AVG_BPS}" | bc)
    echo "Winner: Reth (${speedup}x faster)"
  elif [[ $(echo "${GETH_AVG_BPS} > ${RETH_AVG_BPS}" | bc -l) -eq 1 ]]; then
    speedup=$(echo "scale=2; ${GETH_AVG_BPS} / ${RETH_AVG_BPS}" | bc)
    echo "Winner: Geth (${speedup}x faster)"
  else
    echo "Result: Tie"
  fi
fi

echo
echo "=========================================="
echo "  Test complete"
echo "=========================================="
