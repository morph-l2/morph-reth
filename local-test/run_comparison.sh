#!/bin/bash
# Full reth vs geth comparison benchmark
# Runs each (engine, workload) combination 3 times for statistical significance
set -e

RETH_BIN="./target/release/morph-reth"
GETH_BIN="/Users/panos/workspace/go-ethereum/build/bin/geth"
BENCH_BIN="./target/release/bench-block-exec"
JWT_SECRET="./local-test/jwt-secret.txt"
GENESIS="/tmp/bench-genesis-erc20-2k.json"
RESULTS_DIR="/tmp/bench-results/comparison"

TARGET_TPS=200000   # High enough to saturate both engines
DURATION=60         # Seconds
SENDERS=2000
RUNS=3
# geth can't handle 256 concurrent HTTP connections; use conservative settings
SUBMIT_CONCURRENCY=32
SUBMIT_BATCH_SIZE=500

mkdir -p "$RESULTS_DIR"

start_reth() {
    pm2 delete bench-node 2>/dev/null || true
    sleep 1
    rm -rf /tmp/bench-reth-data; mkdir -p /tmp/bench-reth-data
    pm2 start "$RETH_BIN" --name "bench-node" -- \
        node --chain "$GENESIS" --datadir /tmp/bench-reth-data \
        --http --http.addr 127.0.0.1 --http.port 8545 --http.api "web3,debug,eth,txpool,net" \
        --authrpc.addr 127.0.0.1 --authrpc.port 8551 --authrpc.jwtsecret "$JWT_SECRET" \
        --nat none --engine.persistence-threshold 2 --engine.memory-block-buffer-target 2 \
        --morph.max-tx-payload-bytes 1073741824 \
        --txpool.pending-max-count 1000000 --txpool.pending-max-size 8192 \
        --txpool.basefee-max-count 1000000 --txpool.basefee-max-size 8192 \
        --txpool.queued-max-count 1000000 --txpool.queued-max-size 8192 \
        --txpool.max-account-slots 1000000 --txpool.additional-validation-tasks 12 \
        --txpool.max-batch-size 128 --txpool.disable-transactions-backup \
        --rpc.max-request-size 1024 --rpc.max-response-size 1024 --rpc.max-connections 1000
    sleep 4
}

start_geth() {
    pm2 delete bench-node 2>/dev/null || true
    sleep 1
    rm -rf /tmp/bench-geth-data; mkdir -p /tmp/bench-geth-data
    "$GETH_BIN" init --datadir /tmp/bench-geth-data "$GENESIS" 2>/dev/null
    pm2 start "$GETH_BIN" --name "bench-node" -- \
        --datadir /tmp/bench-geth-data --networkid 99999 --nodiscover --maxpeers 0 \
        --http --http.addr 127.0.0.1 --http.port 8545 --http.api "web3,debug,eth,txpool,net" \
        --authrpc.addr 127.0.0.1 --authrpc.port 8551 --authrpc.jwtsecret "$JWT_SECRET" \
        --gcmode archive \
        --txpool.globalslots 1000000 --txpool.accountslots 1000000 \
        --txpool.globalqueue 1000000 --txpool.accountqueue 1000000 \
        --rpc.txfeecap 0 --rpc.gascap 0 \
        --rpc.batch-request-limit 0 --rpc.batch-response-max-size 0
    sleep 4
}

run_bench() {
    local engine=$1
    local workload=$2
    local run=$3
    local output="$RESULTS_DIR/${engine}-${workload}-run${run}.jsonl"

    echo ">>> [$engine] [$workload] run $run/$RUNS"

    "$BENCH_BIN" run openloop \
        --engine-rpc "http://127.0.0.1:8551" \
        --jwt-secret "$JWT_SECRET" \
        --http-rpc "http://127.0.0.1:8545" \
        --workload "$workload" \
        --target-tps "$TARGET_TPS" \
        --duration-secs "$DURATION" \
        --senders "$SENDERS" \
        --output "$output" \
        --engine-name "$engine" \
        --chain-id 99999 \
        --submit-batch-size "$SUBMIT_BATCH_SIZE" \
        --submit-concurrency "$SUBMIT_CONCURRENCY" \
        --submit-buffer-ticks 64 \
        --submit-tick-ms 50 \
        --producer-idle-ms 5 \
        --drain-secs 120 2>&1 | grep -E "(Pre-gen|Openloop complete)"

    echo ""
}

echo "========================================"
echo "  reth vs geth comparison benchmark"
echo "  Target: ${TARGET_TPS} TPS, ${DURATION}s, ${RUNS} runs"
echo "========================================"
echo ""

for workload in eth-transfer erc20-transfer; do
    for run in $(seq 1 $RUNS); do
        start_reth
        run_bench reth "$workload" "$run"
    done
    for run in $(seq 1 $RUNS); do
        start_geth
        run_bench geth "$workload" "$run"
    done
done

pm2 delete bench-node 2>/dev/null || true

echo ""
echo "========================================"
echo "  Analysis"
echo "========================================"
python3 local-test/analyze_openloop.py "$RESULTS_DIR"/*.jsonl
