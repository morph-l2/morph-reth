#!/bin/bash
# Full benchmark suite: Exec / Sustained / Openloop × ETH/ERC20 × reth/geth
set -e

RETH_BIN="./target/release/morph-reth"
GETH_BIN="/Users/panos/workspace/go-ethereum/build/bin/geth"
BENCH_BIN="./target/release/bench-block-exec"
JWT_SECRET="./local-test/jwt-secret.txt"
GENESIS="/tmp/bench-genesis-erc20-2k.json"
RESULTS_DIR="/tmp/bench-results/full"
SENDERS=2000

mkdir -p "$RESULTS_DIR"

# ── Node lifecycle helpers ──

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
        --rpc.max-request-size 1024 --rpc.max-response-size 1024 --rpc.max-connections 1000 \
        2>/dev/null
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
        --gcmode archive --cache 8192 \
        --txpool.globalslots 1000000 --txpool.accountslots 1000000 \
        --txpool.globalqueue 1000000 --txpool.accountqueue 1000000 \
        --rpc.txfeecap 0 --rpc.gascap 0 \
        --rpc.batch-request-limit 0 --rpc.batch-response-max-size 0 \
        2>/dev/null
    sleep 4
}

start_node() {
    if [ "$1" = "reth" ]; then start_reth; else start_geth; fi
}

timestamp() { date "+%H:%M:%S"; }

# ── Mode A: Exec ──

run_exec() {
    local engine=$1 workload=$2 block_size=$3 run=$4
    local output="$RESULTS_DIR/exec-${engine}-${workload}-${block_size}-run${run}.jsonl"
    echo "[$(timestamp)] Exec: $engine $workload ${block_size}txs run$run"
    start_node "$engine"
    "$BENCH_BIN" run exec \
        --engine-rpc "http://127.0.0.1:8551" \
        --jwt-secret "$JWT_SECRET" \
        --http-rpc "http://127.0.0.1:8545" \
        --workload "$workload" \
        --txs-per-block "$block_size" \
        --blocks 10 \
        --senders "$SENDERS" \
        --output "$output" \
        --engine-name "$engine" \
        --chain-id 99999 \
        --submit-batch-size 500 \
        --submit-concurrency 32 2>&1 | grep "block 10/10"
}

# ── Mode B: Sustained ──

run_sustained() {
    local engine=$1 workload=$2 run=$3
    local output="$RESULTS_DIR/sustained-${engine}-${workload}-run${run}.jsonl"
    echo "[$(timestamp)] Sustained: $engine $workload run$run (5 warmup + 100 blocks × 50k txs)"
    start_node "$engine"
    "$BENCH_BIN" run sustained \
        --engine-rpc "http://127.0.0.1:8551" \
        --jwt-secret "$JWT_SECRET" \
        --http-rpc "http://127.0.0.1:8545" \
        --workload "$workload" \
        --txs-per-block 50000 \
        --blocks 100 \
        --warmup-blocks 5 \
        --senders "$SENDERS" \
        --output "$output" \
        --engine-name "$engine" \
        --chain-id 99999 \
        --submit-batch-size 500 \
        --submit-concurrency 32 2>&1 | grep "block 100/100"
}

# ── Mode C: Openloop ──

run_openloop() {
    local engine=$1 workload=$2 run=$3
    local output="$RESULTS_DIR/openloop-${engine}-${workload}-run${run}.jsonl"
    echo "[$(timestamp)] Openloop: $engine $workload run$run (200k target, 120s)"
    start_node "$engine"
    "$BENCH_BIN" run openloop \
        --engine-rpc "http://127.0.0.1:8551" \
        --jwt-secret "$JWT_SECRET" \
        --http-rpc "http://127.0.0.1:8545" \
        --workload "$workload" \
        --target-tps 200000 \
        --duration-secs 120 \
        --senders "$SENDERS" \
        --output "$output" \
        --engine-name "$engine" \
        --chain-id 99999 \
        --submit-batch-size 500 \
        --submit-concurrency 32 \
        --submit-buffer-ticks 64 \
        --submit-tick-ms 50 \
        --producer-idle-ms 5 \
        --drain-secs 180 2>&1 | grep -E "(Pre-gen|Openloop complete)"
}

# ═══════════════════════════════════════════
#  Execute all benchmarks
# ═══════════════════════════════════════════

echo "════════════════════════════════════════════════════"
echo "  Full Benchmark Suite"
echo "  $(date)"
echo "════════════════════════════════════════════════════"

# ── Mode A: Exec (block-size sweep) ──
echo ""
echo "━━━ Mode A: Exec (pure EVM, block-size sweep) ━━━"
for engine in reth geth; do
    for workload in eth-transfer erc20-transfer; do
        for bs in 1000 10000 50000 100000 200000; do
            for run in 1 2; do
                run_exec "$engine" "$workload" "$bs" "$run"
            done
        done
    done
done

# ── Mode B: Sustained (100 blocks, degradation analysis) ──
echo ""
echo "━━━ Mode B: Sustained (100 blocks × 50k txs, degradation) ━━━"
for engine in reth geth; do
    for workload in eth-transfer erc20-transfer; do
        for run in 1 2 3; do
            run_sustained "$engine" "$workload" "$run"
        done
    done
done

# ── Mode C: Openloop (120s high-pressure) ──
echo ""
echo "━━━ Mode C: Openloop (200k target, 120s) ━━━"
for engine in reth geth; do
    for workload in eth-transfer erc20-transfer; do
        for run in 1 2 3; do
            run_openloop "$engine" "$workload" "$run"
        done
    done
done

pm2 delete bench-node 2>/dev/null || true

echo ""
echo "════════════════════════════════════════════════════"
echo "  All benchmarks complete. $(date)"
echo "  Results in: $RESULTS_DIR"
echo "════════════════════════════════════════════════════"
