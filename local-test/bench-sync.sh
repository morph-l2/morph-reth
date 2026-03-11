#!/usr/bin/env bash

set -euo pipefail

# Benchmark: Geth vs Reth sync speed comparison.
# No geth RPC URL cross-validation — pure sync speed comparison.

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

# ─── Configuration ────────────────────────────────────────────────────────────
: "${TEST_DURATION:=300}"       # seconds to run each config (5 min default)
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

  RESULT_START_BLOCK=${start_block}
  RESULT_END_BLOCK=${end_block}
  RESULT_TOTAL_BLOCKS=${total_blocks}
  RESULT_AVG_BPS=${avg_bps}
  RESULT_PEAK_BPS=${peak_bps}
}

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

echo "=========================================================="
echo "  Morph Sync Speed Benchmark"
echo "  Geth vs Reth"
echo "=========================================================="
echo "  Duration per config:  ${TEST_DURATION}s"
echo "  Sample interval:      ${SAMPLE_INTERVAL}s"
echo "  Mainnet tip (est):    ${MAINNET_TIP}"
echo "=========================================================="
echo

pm2_check

# Results storage
GETH_AVG_BPS=0; GETH_PEAK_BPS=0; GETH_START=0; GETH_END=0; GETH_TOTAL=0
RETH_AVG_BPS=0; RETH_PEAK_BPS=0; RETH_START=0; RETH_END=0; RETH_TOTAL=0

# ─── Phase 1: Test Geth ──────────────────────────────────────────────────────

if [[ "${SKIP_GETH}" != "1" ]]; then
  check_binary "${GETH_BIN}" "cd ../morph/go-ethereum && make geth"
  check_binary "${MORPHNODE_BIN}" "cd ../morph/node && make build"

  echo "=== Phase 1/2: Testing Geth ==="
  echo

  full_reset
  "${SCRIPT_DIR}/prepare.sh" 2>/dev/null

  echo "  Starting morph-geth..."
  "${SCRIPT_DIR}/geth-start.sh"
  wait_for_rpc "geth"

  echo "  Starting morphnode..."
  "${SCRIPT_DIR}/node-start.sh"

  echo "  Warming up (10s)..."
  sleep 10

  run_bps_sampling "geth" "${TEST_DURATION}"
  GETH_AVG_BPS=${RESULT_AVG_BPS}
  GETH_PEAK_BPS=${RESULT_PEAK_BPS}
  GETH_START=${RESULT_START_BLOCK}
  GETH_END=${RESULT_END_BLOCK}
  GETH_TOTAL=${RESULT_TOTAL_BLOCKS}

  echo
  echo "  morphnode Block Sync Rate samples (geth):"
  grep "Block Sync Rate" "${NODE_LOG_FILE}" 2>/dev/null | tail -5 | while read -r line; do
    echo "    ${line}"
  done

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
  check_binary "${MORPHNODE_BIN}" "cd ../morph/node && make build"

  echo "=== Phase 2/2: Testing Reth ==="
  echo

  full_reset
  "${SCRIPT_DIR}/prepare.sh" 2>/dev/null

  echo "  Starting morph-reth..."
  "${SCRIPT_DIR}/reth-start.sh"
  wait_for_rpc "reth"

  echo "  Starting morphnode..."
  "${SCRIPT_DIR}/node-start.sh"

  echo "  Warming up (10s)..."
  sleep 10

  run_bps_sampling "reth" "${TEST_DURATION}"
  RETH_AVG_BPS=${RESULT_AVG_BPS}
  RETH_PEAK_BPS=${RESULT_PEAK_BPS}
  RETH_START=${RESULT_START_BLOCK}
  RETH_END=${RESULT_END_BLOCK}
  RETH_TOTAL=${RESULT_TOTAL_BLOCKS}

  echo
  echo "  morphnode Block Sync Rate samples (reth):"
  grep "Block Sync Rate" "${NODE_LOG_FILE}" 2>/dev/null | tail -5 | while read -r line; do
    echo "    ${line}"
  done

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

echo "=========================================================="
echo "  RESULTS"
echo "=========================================================="
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
  geth_eta=$(echo "scale=0; ${MAINNET_TIP} / ${GETH_AVG_BPS}" | bc)
  printf "%-20s %s  (at %.2f bps)\n" "Geth:" "$(format_duration "${geth_eta}")" "${GETH_AVG_BPS}"
else
  printf "%-20s N/A (no data)\n" "Geth:"
fi

if [[ $(echo "${RETH_AVG_BPS} > 0" | bc -l) -eq 1 ]]; then
  reth_eta=$(echo "scale=0; ${MAINNET_TIP} / ${RETH_AVG_BPS}" | bc)
  printf "%-20s %s  (at %.2f bps)\n" "Reth:" "$(format_duration "${reth_eta}")" "${RETH_AVG_BPS}"
else
  printf "%-20s N/A (no data)\n" "Reth:"
fi

# Comparison
echo
echo "--- Geth vs Reth ---"

if [[ $(echo "${GETH_AVG_BPS} > 0 && ${RETH_AVG_BPS} > 0" | bc -l) -eq 1 ]]; then
  if [[ $(echo "${GETH_AVG_BPS} > ${RETH_AVG_BPS}" | bc -l) -eq 1 ]]; then
    diff_pct=$(echo "scale=1; (${GETH_AVG_BPS} - ${RETH_AVG_BPS}) * 100 / ${GETH_AVG_BPS}" | bc)
    echo "Geth is faster by ${diff_pct}%"
    echo "  geth=${GETH_AVG_BPS} bps, reth=${RETH_AVG_BPS} bps"
  elif [[ $(echo "${RETH_AVG_BPS} > ${GETH_AVG_BPS}" | bc -l) -eq 1 ]]; then
    diff_pct=$(echo "scale=1; (${RETH_AVG_BPS} - ${GETH_AVG_BPS}) * 100 / ${RETH_AVG_BPS}" | bc)
    echo "Reth is faster by ${diff_pct}%"
    echo "  geth=${GETH_AVG_BPS} bps, reth=${RETH_AVG_BPS} bps"
  else
    echo "Tie: both at ${GETH_AVG_BPS} bps"
  fi
else
  echo "Insufficient data for comparison"
fi

echo
echo "=========================================================="
echo "  Benchmark complete"
echo "=========================================================="
