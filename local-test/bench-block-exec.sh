#!/usr/bin/env bash
#
# bench-block-exec.sh — 4-phase max TPS benchmark orchestration.
#
# Phases:
#   1. Sweep      — find the TPS inflection point for each engine × workload
#   2. Precise    — run exec / e2e / sustained at and around the inflection point
#   3. Degrade    — sustained test with large warmup to measure state degradation
#   4. Summarize  — generate summary TSV and charts
#
# Usage:
#   ./local-test/bench-block-exec.sh
#
# Override defaults via environment variables, e.g.:
#   ENGINES="reth" WORKLOADS="eth-transfer" ./local-test/bench-block-exec.sh

set -euo pipefail

# ─── Resolve repo root ───────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

# ─── Configuration (all overridable via environment) ─────────────────────────

RETH_BIN="${RETH_BIN:-./target/release/morph-reth}"
GETH_BIN="${GETH_BIN:-../go-ethereum/build/bin/geth}"
BENCH_BIN="${BENCH_BIN:-./target/release/bench-block-exec}"
JWT_SECRET="${JWT_SECRET:-./local-test/jwt-secret.txt}"
CHAIN_ID="${CHAIN_ID:-99999}"
RESULTS_DIR="${RESULTS_DIR:-bench-results/$(date +%Y%m%d-%H%M%S)}"
FORCE="${FORCE:-0}"
HTTP_PORT="${HTTP_PORT:-8545}"
AUTH_PORT="${AUTH_PORT:-8551}"
WORKLOADS="${WORKLOADS:-eth-transfer erc20-transfer uniswap-swap}"
ENGINES="${ENGINES:-reth geth}"

# ─── Contract bytecodes (read from compiled artifacts) ───────────────────────

BENCH_TOKEN_CODE="$(python3 -c "import json; print(json.load(open('local-test/bench-contracts/out/BenchToken.sol/BenchToken.json'))['deployedBytecode']['object'])")"
BENCH_SWAP_CODE="$(python3 -c "import json; print(json.load(open('local-test/bench-contracts/out/BenchSwap.sol/BenchSwap.json'))['deployedBytecode']['object'])")"

# ─── Helper functions ────────────────────────────────────────────────────────

generate_genesis() {
  local senders="$1"
  local gas_limit="$2"
  local max_tx="$3"
  local output="$4"

  mkdir -p "$(dirname "${output}")"
  "${BENCH_BIN}" write-genesis \
    --output "${output}" \
    --senders "${senders}" \
    --gas-limit "${gas_limit}" \
    --max-tx-per-block "${max_tx}" \
    --bench-token-code "${BENCH_TOKEN_CODE}" \
    --bench-swap-code "${BENCH_SWAP_CODE}"
}

start_reth() {
  local datadir="$1"
  local genesis="$2"

  rm -rf "${datadir}"
  mkdir -p "${datadir}"

  local args=(
    node
    --chain "${genesis}"
    --datadir "${datadir}"
    --http
    --http.addr 127.0.0.1
    --http.port "${HTTP_PORT}"
    --http.api "web3,debug,eth,txpool,net"
    --authrpc.addr 127.0.0.1
    --authrpc.port "${AUTH_PORT}"
    --authrpc.jwtsecret "${JWT_SECRET}"
    --nat none
    --engine.persistence-threshold 4096
    --engine.memory-block-buffer-target 4096
    --morph.max-tx-payload-bytes 1073741824
    --txpool.pending-max-count 100000
    --txpool.basefee-max-count 100000
    --txpool.queued-max-count 100000
    --txpool.max-account-slots 10000
  )

  pm2 start "${RETH_BIN}" --name "bench-node" -- "${args[@]}"
  echo "Started reth on http=${HTTP_PORT} auth=${AUTH_PORT}"
}

start_geth() {
  local datadir="$1"
  local genesis="$2"

  rm -rf "${datadir}"
  mkdir -p "${datadir}"

  # Initialize geth datadir with genesis
  "${GETH_BIN}" init --datadir "${datadir}" "${genesis}"

  local args=(
    --datadir "${datadir}"
    --gcmode archive
    --syncmode full
    --cache 8192
    --txpool.globalslots 100000
    --txpool.accountslots 1000
    --http
    --http.addr 127.0.0.1
    --http.port "${HTTP_PORT}"
    --http.corsdomain "*"
    --http.vhosts "*"
    --http.api "web3,eth,debug,txpool,net,morph,engine"
    --authrpc.addr 127.0.0.1
    --authrpc.port "${AUTH_PORT}"
    --authrpc.vhosts "*"
    --authrpc.jwtsecret "${JWT_SECRET}"
    --nodiscover
    --maxpeers 0
  )

  pm2 start "${GETH_BIN}" --name "bench-node" -- "${args[@]}"
  echo "Started geth on http=${HTTP_PORT} auth=${AUTH_PORT}"
}

stop_node() {
  if pm2 describe "bench-node" &>/dev/null; then
    pm2 stop "bench-node" 2>/dev/null || true
    pm2 delete "bench-node" 2>/dev/null || true
    echo "bench-node: stopped"
  else
    echo "bench-node: not running"
  fi
}

wait_for_rpc() {
  local url="http://127.0.0.1:${HTTP_PORT}"
  local timeout=120
  local elapsed=0

  echo "Waiting for RPC on port ${HTTP_PORT} (timeout ${timeout}s)..."
  while (( elapsed < timeout )); do
    if curl -s -X POST "${url}" \
         -H "Content-Type: application/json" \
         -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' \
         2>/dev/null | grep -q '"result"'; then
      echo "RPC is ready (${elapsed}s)."
      return 0
    fi
    sleep 1
    (( elapsed++ )) || true
  done

  echo "ERROR: RPC not ready after ${timeout}s"
  return 1
}

# run_test engine mode workload senders txs blocks warmup
#
# Orchestrates a single benchmark run:
#   1. Generate genesis
#   2. Start the engine node
#   3. Run the bench-block-exec subcommand
#   4. Stop the node and clean up
#
# Skips if the output file already exists (unless FORCE=1).
run_test() {
  local engine="$1"
  local mode="$2"
  local workload="$3"
  local senders="$4"
  local txs="$5"
  local blocks="$6"
  local warmup="$7"

  local output_dir="${RESULTS_DIR}/${mode}"
  local output_file="${output_dir}/${engine}-${workload}-s${senders}-w${warmup}.jsonl"
  local datadir="bench-data/bench-${engine}-${mode}-${workload}"
  local genesis_file="bench-data/bench-${engine}-${mode}-${workload}-genesis.json"

  # Skip if output exists and FORCE is not set
  if [[ -f "${output_file}" && "${FORCE}" != "1" ]]; then
    echo "SKIP: ${output_file} already exists (use FORCE=1 to re-run)"
    return 0
  fi

  mkdir -p "${output_dir}"

  echo ""
  echo "────────────────────────────────────────────────────────────"
  echo "  [${engine}] mode=${mode} workload=${workload} senders=${senders} txs=${txs} blocks=${blocks} warmup=${warmup}"
  echo "  output: ${output_file}"
  echo "────────────────────────────────────────────────────────────"

  # Generate genesis
  generate_genesis "${senders}" "0x2540BE400" 0 "${genesis_file}"

  # Start engine
  if [[ "${engine}" == "reth" ]]; then
    start_reth "${datadir}" "${genesis_file}"
  else
    start_geth "${datadir}" "${genesis_file}"
  fi

  wait_for_rpc

  # Run the appropriate benchmark mode
  local rc=0
  case "${mode}" in
    exec)
      "${BENCH_BIN}" run exec \
        --engine-rpc "http://127.0.0.1:${AUTH_PORT}" \
        --jwt-secret "${JWT_SECRET}" \
        --workload "${workload}" \
        --txs-per-block "${txs}" \
        --blocks "${blocks}" \
        --output "${output_file}" \
        --engine-name "${engine}" \
        --chain-id "${CHAIN_ID}" || rc=$?
      ;;
    e2e)
      "${BENCH_BIN}" run e2e \
        --engine-rpc "http://127.0.0.1:${AUTH_PORT}" \
        --jwt-secret "${JWT_SECRET}" \
        --http-rpc "http://127.0.0.1:${HTTP_PORT}" \
        --workload "${workload}" \
        --txs-per-block "${txs}" \
        --blocks "${blocks}" \
        --senders "${senders}" \
        --output "${output_file}" \
        --engine-name "${engine}" \
        --chain-id "${CHAIN_ID}" || rc=$?
      ;;
    sustained)
      "${BENCH_BIN}" run sustained \
        --engine-rpc "http://127.0.0.1:${AUTH_PORT}" \
        --jwt-secret "${JWT_SECRET}" \
        --http-rpc "http://127.0.0.1:${HTTP_PORT}" \
        --workload "${workload}" \
        --txs-per-block "${txs}" \
        --blocks "${blocks}" \
        --warmup-blocks "${warmup}" \
        --senders "${senders}" \
        --output "${output_file}" \
        --engine-name "${engine}" \
        --chain-id "${CHAIN_ID}" || rc=$?
      ;;
    *)
      echo "ERROR: unknown mode '${mode}'"
      rc=1
      ;;
  esac

  # Stop node and clean up
  stop_node
  sleep 2
  rm -rf "${datadir}"

  if [[ "${rc}" -ne 0 ]]; then
    echo "WARNING: benchmark exited with code ${rc} (output may be partial)"
  fi

  return 0
}

# run_sweep engine workload
#
# Runs the sweep subcommand which internally iterates over transaction counts
# to find the inflection point. Requires starting/stopping the node since
# sweep runs multiple steps in a single invocation.
run_sweep() {
  local engine="$1"
  local workload="$2"

  local sweep_dir="${RESULTS_DIR}/sweep"
  local summary_file="${sweep_dir}/${engine}-${workload}-sweep-summary.json"
  local datadir="bench-data/bench-${engine}-sweep-${workload}"
  local genesis_file="bench-data/bench-${engine}-sweep-${workload}-genesis.json"

  # Skip if summary already exists and FORCE is not set
  if [[ -f "${summary_file}" && "${FORCE}" != "1" ]]; then
    echo "SKIP: ${summary_file} already exists (use FORCE=1 to re-run)"
    return 0
  fi

  mkdir -p "${sweep_dir}"

  echo ""
  echo "════════════════════════════════════════════════════════════"
  echo "  SWEEP: engine=${engine} workload=${workload}"
  echo "════════════════════════════════════════════════════════════"

  # Sweep uses exec mode (single sender), so senders=1
  generate_genesis 1 "0x2540BE400" 0 "${genesis_file}"

  if [[ "${engine}" == "reth" ]]; then
    start_reth "${datadir}" "${genesis_file}"
  else
    start_geth "${datadir}" "${genesis_file}"
  fi

  wait_for_rpc

  local rc=0
  "${BENCH_BIN}" sweep \
    --engine-rpc "http://127.0.0.1:${AUTH_PORT}" \
    --jwt-secret "${JWT_SECRET}" \
    --workload "${workload}" \
    --blocks-per-step 30 \
    --output-dir "${sweep_dir}" \
    --engine-name "${engine}" \
    --chain-id "${CHAIN_ID}" || rc=$?

  stop_node
  sleep 2
  rm -rf "${datadir}"

  if [[ "${rc}" -ne 0 ]]; then
    echo "WARNING: sweep exited with code ${rc}"
  fi
}

# get_inflection_txs engine workload
#
# Reads the sweep summary JSON and extracts inflection_txs.
# Defaults to 5000 if the file is missing or the field is absent.
get_inflection_txs() {
  local engine="$1"
  local workload="$2"

  local summary_file="${RESULTS_DIR}/sweep/${engine}-${workload}-sweep-summary.json"

  if [[ -f "${summary_file}" ]]; then
    local val
    val="$(python3 -c "import json; print(json.load(open('${summary_file}'))['inflection_txs'])" 2>/dev/null || echo "")"
    if [[ -n "${val}" && "${val}" != "0" ]]; then
      echo "${val}"
      return 0
    fi
  fi

  echo "5000"
}

# ─── Banner ──────────────────────────────────────────────────────────────────

echo "============================================================"
echo "  bench-block-exec — 4-Phase Max TPS Benchmark"
echo "============================================================"
echo ""
echo "  ENGINES:          ${ENGINES}"
echo "  WORKLOADS:        ${WORKLOADS}"
echo "  RESULTS_DIR:      ${RESULTS_DIR}"
echo "  RETH_BIN:         ${RETH_BIN}"
echo "  GETH_BIN:         ${GETH_BIN}"
echo "  BENCH_BIN:        ${BENCH_BIN}"
echo "  CHAIN_ID:         ${CHAIN_ID}"
echo "  FORCE:            ${FORCE}"
echo ""

# ─── Prerequisites ───────────────────────────────────────────────────────────

if ! command -v pm2 &>/dev/null; then
  echo "ERROR: pm2 is not installed. Install with: npm install -g pm2"
  exit 1
fi

if [[ ! -x "${BENCH_BIN}" ]]; then
  echo "ERROR: Missing executable: ${BENCH_BIN}"
  echo "Build hint: cargo build --release --bin bench-block-exec"
  exit 1
fi

for eng in ${ENGINES}; do
  if [[ "${eng}" == "reth" && ! -x "${RETH_BIN}" ]]; then
    echo "ERROR: Missing executable: ${RETH_BIN}"
    echo "Build hint: cargo build --release --bin morph-reth"
    exit 1
  fi
  if [[ "${eng}" == "geth" && ! -x "${GETH_BIN}" ]]; then
    echo "ERROR: Missing executable: ${GETH_BIN}"
    echo "Build hint: cd ../go-ethereum && make geth"
    exit 1
  fi
done

# Generate JWT secret if missing
if [[ ! -f "${JWT_SECRET}" ]]; then
  echo "Generating JWT secret at ${JWT_SECRET}..."
  mkdir -p "$(dirname "${JWT_SECRET}")"
  openssl rand -hex 32 > "${JWT_SECRET}"
fi

mkdir -p "${RESULTS_DIR}"

# ─── Phase 1: Sweep ─────────────────────────────────────────────────────────

echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "║  Phase 1: Sweep — Inflection Point Discovery            ║"
echo "╚══════════════════════════════════════════════════════════╝"

for engine in ${ENGINES}; do
  for workload in ${WORKLOADS}; do
    run_sweep "${engine}" "${workload}"
  done
done

# ─── Phase 2: Precise Matrix ────────────────────────────────────────────────

echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "║  Phase 2: Precise Matrix                                ║"
echo "╚══════════════════════════════════════════════════════════╝"

for engine in ${ENGINES}; do
  for workload in ${WORKLOADS}; do
    inflection="$(get_inflection_txs "${engine}" "${workload}")"
    echo "  ${engine}/${workload}: inflection_txs=${inflection}"

    # Mode A: exec — senders=1, txs=inflection, blocks=50
    run_test "${engine}" "exec" "${workload}" 1 "${inflection}" 50 0

    # Mode B: e2e — senders=1,100,1000, txs=inflection*0.8, blocks=200
    e2e_txs=$(( inflection * 8 / 10 ))
    for senders in 1 100 1000; do
      run_test "${engine}" "e2e" "${workload}" "${senders}" "${e2e_txs}" 200 0
    done

    # Mode C: sustained — senders=1,100, txs=inflection*0.5, blocks=1000, warmup=0
    sustained_txs=$(( inflection * 5 / 10 ))
    for senders in 1 100; do
      run_test "${engine}" "sustained" "${workload}" "${senders}" "${sustained_txs}" 1000 0
    done
  done
done

# ─── Phase 3: State Degradation ─────────────────────────────────────────────

echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "║  Phase 3: State Degradation                             ║"
echo "╚══════════════════════════════════════════════════════════╝"

for engine in ${ENGINES}; do
  for workload in ${WORKLOADS}; do
    inflection="$(get_inflection_txs "${engine}" "${workload}")"
    sustained_txs=$(( inflection * 5 / 10 ))

    # Mode C: sustained — senders=100, txs=inflection*0.5, blocks=1000, warmup=500
    run_test "${engine}" "sustained" "${workload}" 100 "${sustained_txs}" 1000 500
  done
done

# ─── Phase 4: Summarize + Plot ──────────────────────────────────────────────

echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "║  Phase 4: Summarize + Plot                              ║"
echo "╚══════════════════════════════════════════════════════════╝"

echo "Generating summary..."
"${BENCH_BIN}" summarize \
  --results-dir "${RESULTS_DIR}" \
  --output "${RESULTS_DIR}/summary.tsv" \
  --v2 || true

echo "Generating charts..."
python3 local-test/bench-plot.py \
  --input "${RESULTS_DIR}" \
  --output "${RESULTS_DIR}/charts" \
  --type all || true

# ─── Done ────────────────────────────────────────────────────────────────────

echo ""
echo "============================================================"
echo "  Benchmark complete!"
echo "============================================================"
echo ""
echo "  Results:  ${RESULTS_DIR}"
echo "  Summary:  ${RESULTS_DIR}/summary.tsv"
echo "  Charts:   ${RESULTS_DIR}/charts/"
echo ""
