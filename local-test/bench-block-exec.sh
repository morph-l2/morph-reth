#!/usr/bin/env bash
#
# bench-block-exec.sh — Multi-round geth vs reth block-execution benchmark.
#
# This script is STANDALONE: it does NOT source common.sh because it uses a
# custom genesis (not mainnet/hoodi) and manages its own data directories.
#
# Usage:
#   ./local-test/bench-block-exec.sh
#
# Override defaults via environment variables, e.g.:
#   ROUNDS=1 BLOCKS=100 SKIP_GETH=1 ./local-test/bench-block-exec.sh

set -euo pipefail

# ─── Resolve repo root ───────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

# ─── Configuration (all overridable via environment) ─────────────────────────

: "${ROUNDS:=3}"
: "${BLOCKS:=500}"
: "${SKIP_GETH:=0}"
: "${SKIP_RETH:=0}"
: "${RESULTS_DIR:=bench-results/$(date +%Y%m%d-%H%M%S)}"
: "${RETH_BIN:=./target/release/morph-reth}"
: "${GETH_BIN:=../go-ethereum/build/bin/geth}"
: "${BENCH_BIN:=./target/release/bench-block-exec}"
: "${JWT_SECRET:=./local-test/jwt-secret.txt}"
: "${SENDER_KEY:=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
: "${SENDER_ADDR:=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266}"
: "${CONTRACT_ARTIFACT:=./local-test/erc20-bench-contracts/out/BenchToken.sol/BenchToken.json}"
: "${CHAIN_ID:=99999}"

: "${ETH_TX_SIZES:=500 1000 2000 3000 5000 8000}"
: "${ERC20_TX_SIZES:=500 1000 2000 3000 5000}"

HTTP_PORT=8545;  AUTH_PORT=8551
HTTP_PORT_B=9545; AUTH_PORT_B=9551

# ─── Helper functions ────────────────────────────────────────────────────────

check_binary() {
  local bin_path="$1"
  local hint="$2"
  if [[ ! -x "${bin_path}" ]]; then
    echo "ERROR: Missing executable: ${bin_path}"
    echo "Build hint: ${hint}"
    return 1
  fi
}

pm2_check() {
  if ! command -v pm2 &>/dev/null; then
    echo "ERROR: pm2 is not installed"
    echo "Install with: npm install -g pm2"
    return 1
  fi
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

wait_for_rpc() {
  local name="$1"
  local port="$2"
  local url="http://127.0.0.1:${port}"
  local timeout=120
  local elapsed=0

  echo "Waiting for ${name} RPC on port ${port} (timeout ${timeout}s)..."
  while (( elapsed < timeout )); do
    if curl -s -X POST "${url}" \
         -H "Content-Type: application/json" \
         -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' \
         2>/dev/null | grep -q '"result"'; then
      echo "${name} RPC is ready (${elapsed}s)."
      return 0
    fi
    sleep 1
    (( elapsed++ )) || true
  done

  echo "ERROR: ${name} RPC not ready after ${timeout}s"
  return 1
}

fresh_datadir() {
  local dir="$1"
  rm -rf "${dir}"
  mkdir -p "${dir}"
}

generate_genesis() {
  local output="$1"
  mkdir -p "$(dirname "${output}")"
  "${BENCH_BIN}" write-genesis \
    --output "${output}" \
    --sender "${SENDER_ADDR}" \
    --sender-balance "1000000000000000000000000000"
}

start_reth() {
  local datadir="$1"
  local genesis="$2"
  local http_port="$3"
  local auth_port="$4"
  local name="$5"

  local args=(
    node
    --chain "${genesis}"
    --datadir "${datadir}"
    --http
    --http.addr 127.0.0.1
    --http.port "${http_port}"
    --http.api "web3,debug,eth,txpool,net"
    --authrpc.addr 127.0.0.1
    --authrpc.port "${auth_port}"
    --authrpc.jwtsecret "${JWT_SECRET}"
    --nat none
    --morph.max-tx-payload-bytes 131072000
    --engine.persistence-threshold 2048
    --engine.memory-block-buffer-target 2048
  )

  pm2 start "${RETH_BIN}" --name "${name}" -- "${args[@]}"
  echo "Started ${name} (reth) on http=${http_port} auth=${auth_port}"
}

start_geth() {
  local datadir="$1"
  local genesis="$2"
  local http_port="$3"
  local auth_port="$4"
  local name="$5"

  # Initialize geth datadir with genesis
  "${GETH_BIN}" init --datadir "${datadir}" "${genesis}"

  local args=(
    --datadir "${datadir}"
    --gcmode archive
    --syncmode full
    --http
    --http.addr 127.0.0.1
    --http.port "${http_port}"
    --http.corsdomain "*"
    --http.vhosts "*"
    --http.api "web3,eth,debug,txpool,net,morph,engine"
    --authrpc.addr 127.0.0.1
    --authrpc.port "${auth_port}"
    --authrpc.vhosts "*"
    --authrpc.jwtsecret "${JWT_SECRET}"
    --nodiscover
    --maxpeers 0
  )

  pm2 start "${GETH_BIN}" --name "${name}" -- "${args[@]}"
  echo "Started ${name} (geth) on http=${http_port} auth=${auth_port}"
}

run_workload() {
  local engine_name="$1"
  local layer="$2"
  local txs_per_block="$3"
  local output="$4"
  local auth_port="$5"
  local http_port="$6"

  mkdir -p "$(dirname "${output}")"

  local args=(
    run-workload
    --engine-rpc "http://127.0.0.1:${auth_port}"
    --jwt-secret "${JWT_SECRET}"
    --http-rpc "http://127.0.0.1:${http_port}"
    --layer "${layer}"
    --txs-per-block "${txs_per_block}"
    --blocks "${BLOCKS}"
    --output "${output}"
    --engine-name "${engine_name}"
    --sender-key "${SENDER_KEY}"
    --chain-id "${CHAIN_ID}"
  )

  # Add contract artifact only for erc20-transfer layer
  if [[ "${layer}" == "erc20-transfer" ]]; then
    args+=(--contract-artifact "${CONTRACT_ARTIFACT}")
  fi

  "${BENCH_BIN}" "${args[@]}" || true
}

# ─── Banner ──────────────────────────────────────────────────────────────────

echo "============================================================"
echo "  bench-block-exec  --  Geth vs Reth Block Execution Bench"
echo "============================================================"
echo ""
echo "  ROUNDS:           ${ROUNDS}"
echo "  BLOCKS:           ${BLOCKS}"
echo "  SKIP_GETH:        ${SKIP_GETH}"
echo "  SKIP_RETH:        ${SKIP_RETH}"
echo "  RESULTS_DIR:      ${RESULTS_DIR}"
echo "  RETH_BIN:         ${RETH_BIN}"
echo "  GETH_BIN:         ${GETH_BIN}"
echo "  BENCH_BIN:        ${BENCH_BIN}"
echo "  ETH_TX_SIZES:     ${ETH_TX_SIZES}"
echo "  ERC20_TX_SIZES:   ${ERC20_TX_SIZES}"
echo "  CHAIN_ID:         ${CHAIN_ID}"
echo ""

# ─── Prerequisites ───────────────────────────────────────────────────────────

pm2_check
check_binary "${BENCH_BIN}" "cargo build --release --bin bench-block-exec"
if [[ "${SKIP_RETH}" != "1" ]]; then
  check_binary "${RETH_BIN}" "cargo build --release --bin morph-reth"
fi
if [[ "${SKIP_GETH}" != "1" ]]; then
  check_binary "${GETH_BIN}" "cd ../morph/go-ethereum && make geth"
fi

# Generate JWT secret if missing
if [[ ! -f "${JWT_SECRET}" ]]; then
  echo "Generating JWT secret at ${JWT_SECRET}..."
  mkdir -p "$(dirname "${JWT_SECRET}")"
  openssl rand -hex 32 > "${JWT_SECRET}"
fi

# Build contract artifact if missing
if [[ ! -f "${CONTRACT_ARTIFACT}" ]]; then
  echo "Building ERC20 contract artifact..."
  (cd local-test/erc20-bench-contracts && forge build)
fi

mkdir -p "${RESULTS_DIR}"

# ─── Main benchmark loop ────────────────────────────────────────────────────

for round in $(seq 1 "${ROUNDS}"); do
  echo ""
  echo "============================================================"
  echo "  Round ${round}/${ROUNDS}"
  echo "============================================================"

  # Alternate engine order: odd rounds geth first, even rounds reth first
  if (( round % 2 == 1 )); then
    engines=("geth" "reth")
  else
    engines=("reth" "geth")
  fi

  for engine in "${engines[@]}"; do
    # Skip logic
    if [[ "${engine}" == "geth" && "${SKIP_GETH}" == "1" ]]; then
      echo "Skipping geth (SKIP_GETH=1)"
      continue
    fi
    if [[ "${engine}" == "reth" && "${SKIP_RETH}" == "1" ]]; then
      echo "Skipping reth (SKIP_RETH=1)"
      continue
    fi

    echo ""
    echo "--- Round ${round} / Engine: ${engine} ---"

    # ── Layer 1: ETH transfers ──
    for txs in ${ETH_TX_SIZES}; do
      echo ""
      echo "[${engine}] eth-transfer  txs_per_block=${txs}  blocks=${BLOCKS}"

      datadir="bench-data/${engine}-eth-${txs}"
      genesis_file="bench-data/${engine}-eth-${txs}-genesis.json"
      output_file="${RESULTS_DIR}/round${round}-${engine}-eth-${txs}.json"

      fresh_datadir "${datadir}"
      generate_genesis "${genesis_file}"

      if [[ "${engine}" == "reth" ]]; then
        start_reth "${datadir}" "${genesis_file}" "${HTTP_PORT}" "${AUTH_PORT}" "morph-reth-bench"
      else
        start_geth "${datadir}" "${genesis_file}" "${HTTP_PORT}" "${AUTH_PORT}" "morph-geth-bench"
      fi

      wait_for_rpc "${engine}" "${HTTP_PORT}"
      run_workload "${engine}" "eth-transfer" "${txs}" "${output_file}" "${AUTH_PORT}" "${HTTP_PORT}"

      if [[ "${engine}" == "reth" ]]; then
        pm2_stop "morph-reth-bench" || true
      else
        pm2_stop "morph-geth-bench" || true
      fi
      sleep 2
    done

    # ── Layer 2: ERC20 transfers ──
    for txs in ${ERC20_TX_SIZES}; do
      echo ""
      echo "[${engine}] erc20-transfer  txs_per_block=${txs}  blocks=${BLOCKS}"

      datadir="bench-data/${engine}-erc20-${txs}"
      genesis_file="bench-data/${engine}-erc20-${txs}-genesis.json"
      output_file="${RESULTS_DIR}/round${round}-${engine}-erc20-${txs}.json"

      fresh_datadir "${datadir}"
      generate_genesis "${genesis_file}"

      if [[ "${engine}" == "reth" ]]; then
        start_reth "${datadir}" "${genesis_file}" "${HTTP_PORT}" "${AUTH_PORT}" "morph-reth-bench"
      else
        start_geth "${datadir}" "${genesis_file}" "${HTTP_PORT}" "${AUTH_PORT}" "morph-geth-bench"
      fi

      wait_for_rpc "${engine}" "${HTTP_PORT}"
      run_workload "${engine}" "erc20-transfer" "${txs}" "${output_file}" "${AUTH_PORT}" "${HTTP_PORT}"

      if [[ "${engine}" == "reth" ]]; then
        pm2_stop "morph-reth-bench" || true
      else
        pm2_stop "morph-geth-bench" || true
      fi
      sleep 2
    done
  done
done

# ─── State Consistency Verification ─────────────────────────────────────────

if [[ "${SKIP_GETH}" != "1" && "${SKIP_RETH}" != "1" ]]; then
  echo ""
  echo "============================================================"
  echo "  State Consistency Verification"
  echo "============================================================"

  verify_datadir_reth="bench-data/verify-reth"
  verify_datadir_geth="bench-data/verify-geth"
  verify_genesis="bench-data/verify-genesis.json"

  fresh_datadir "${verify_datadir_reth}"
  fresh_datadir "${verify_datadir_geth}"
  generate_genesis "${verify_genesis}"

  # Start both nodes on different ports simultaneously
  start_reth "${verify_datadir_reth}" "${verify_genesis}" "${HTTP_PORT}" "${AUTH_PORT}" "morph-reth-verify"
  start_geth "${verify_datadir_geth}" "${verify_genesis}" "${HTTP_PORT_B}" "${AUTH_PORT_B}" "morph-geth-verify"

  wait_for_rpc "reth-verify" "${HTTP_PORT}"
  wait_for_rpc "geth-verify" "${HTTP_PORT_B}"

  # Run identical ERC20 workload on each engine
  echo "Running verification workload on reth..."
  run_workload "reth" "erc20-transfer" "1000" \
    "${RESULTS_DIR}/verify-reth.json" "${AUTH_PORT}" "${HTTP_PORT}"

  echo "Running verification workload on geth..."
  run_workload "geth" "erc20-transfer" "1000" \
    "${RESULTS_DIR}/verify-geth.json" "${AUTH_PORT_B}" "${HTTP_PORT_B}"

  # Verify state consistency
  echo "Verifying state consistency..."
  "${BENCH_BIN}" verify-state \
    --rpc-a "http://127.0.0.1:${HTTP_PORT}" \
    --rpc-b "http://127.0.0.1:${HTTP_PORT_B}" \
    --check-balances 100 \
    --output "${RESULTS_DIR}/verify-state.json" || true

  pm2_stop "morph-reth-verify" || true
  pm2_stop "morph-geth-verify" || true
fi

# ─── Generate Summary ────────────────────────────────────────────────────────

echo ""
echo "Generating summary..."
"${BENCH_BIN}" summarize \
  --results-dir "${RESULTS_DIR}" \
  --output "${RESULTS_DIR}/summary.tsv" || true

# ─── Done ────────────────────────────────────────────────────────────────────

echo ""
echo "============================================================"
echo "  Benchmark complete!"
echo "============================================================"
echo ""
echo "  Results:  ${RESULTS_DIR}"
echo "  Summary:  ${RESULTS_DIR}/summary.tsv"
echo ""
