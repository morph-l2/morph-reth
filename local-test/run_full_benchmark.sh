#!/usr/bin/env bash
# Reproducible, reth-only benchmark runner.
set -Eeuo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT_DIR"

PROFILE=${PROFILE:-release}
RETH_BIN=${RETH_BIN:-"$ROOT_DIR/target/$PROFILE/morph-reth"}
BENCH_BIN=${BENCH_BIN:-"$ROOT_DIR/target/$PROFILE/bench-block-exec"}
JWT_SECRET=${JWT_SECRET:-"$ROOT_DIR/local-test/jwt-secret.txt"}
CONTRACT_DIR=${CONTRACT_DIR:-"$ROOT_DIR/local-test/bench-contracts"}
RESULTS_DIR=${RESULTS_DIR:-"/tmp/morph-reth-bench/$(date -u +%Y%m%dT%H%M%SZ)"}
GENESIS=${GENESIS:-}
DATA_DIR=${DATA_DIR:-"$RESULTS_DIR/reth-data"}
HTTP_PORT=${HTTP_PORT:-18545}
AUTHRPC_PORT=${AUTHRPC_PORT:-18551}
P2P_PORT=${P2P_PORT:-30313}
CHAIN_ID=${CHAIN_ID:-99999}
SENDERS=${SENDERS:-2000}
MODES=${MODES:-"exec sustained openloop"}
WORKLOADS=${WORKLOADS:-"eth-transfer erc20-transfer"}
RUNS=${RUNS:-3}
EXEC_BLOCK_SIZES=${EXEC_BLOCK_SIZES:-"1000 10000 50000 100000"}
EXEC_BLOCKS=${EXEC_BLOCKS:-10}
SUSTAINED_TXS_PER_BLOCK=${SUSTAINED_TXS_PER_BLOCK:-50000}
SUSTAINED_BLOCKS=${SUSTAINED_BLOCKS:-100}
SUSTAINED_WARMUP_BLOCKS=${SUSTAINED_WARMUP_BLOCKS:-5}
OPENLOOP_TARGET_TPS=${OPENLOOP_TARGET_TPS:-200000}
OPENLOOP_DURATION_SECS=${OPENLOOP_DURATION_SECS:-120}
OPENLOOP_DRAIN_SECS=${OPENLOOP_DRAIN_SECS:-600}
RECEIVER_MODE=${RECEIVER_MODE:-unique}
BENCHMARK_DISABLE_TX_PAYLOAD_LIMIT=${BENCHMARK_DISABLE_TX_PAYLOAD_LIMIT:-1}
BENCHMARK_GENESIS_MAX_TX_PAYLOAD_BYTES=${BENCHMARK_GENESIS_MAX_TX_PAYLOAD_BYTES:-1073741824}
BENCHMARK_BUILDER_DEADLINE_SECS=${BENCHMARK_BUILDER_DEADLINE_SECS:-12}
BENCHMARK_TXPOOL_MAX_COUNT=${BENCHMARK_TXPOOL_MAX_COUNT:-30000000}
START_TIMEOUT_SECS=${START_TIMEOUT_SECS:-30}
BUILD=${BUILD:-1}

NODE_PID=""
NODE_LOG=""

timestamp() { date "+%H:%M:%S"; }

cleanup_node() {
    if [[ -n "$NODE_PID" ]] && kill -0 "$NODE_PID" 2>/dev/null; then
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
    NODE_PID=""
}
trap cleanup_node EXIT INT TERM

require_command() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing required command: $1" >&2
        exit 1
    }
}

normalize_and_validate_paths() {
    mkdir -p "$RESULTS_DIR"
    RESULTS_DIR=$(cd "$RESULTS_DIR" && pwd -P)

    local data_parent data_name
    data_parent=$(dirname "$DATA_DIR")
    data_name=$(basename "$DATA_DIR")
    mkdir -p "$data_parent"
    data_parent=$(cd "$data_parent" && pwd -P)
    DATA_DIR="$data_parent/$data_name"

    case "$DATA_DIR" in
        "$RESULTS_DIR"/*) ;;
        *)
            echo "DATA_DIR must be a child of RESULTS_DIR before it can be reset: $DATA_DIR" >&2
            exit 1
            ;;
    esac

    if find "$RESULTS_DIR" -maxdepth 1 -type f -name '*.jsonl' -print -quit | grep -q .; then
        echo "RESULTS_DIR already contains benchmark JSONL files; choose a fresh directory: $RESULTS_DIR" >&2
        exit 1
    fi

    if [[ -z "$GENESIS" ]]; then
        GENESIS="$RESULTS_DIR/genesis.json"
    fi
}

prepare() {
    require_command cargo
    require_command curl
    require_command forge
    require_command jq
    require_command openssl

    case "$BENCHMARK_DISABLE_TX_PAYLOAD_LIMIT" in
        0|1) ;;
        *)
            echo "BENCHMARK_DISABLE_TX_PAYLOAD_LIMIT must be 0 or 1" >&2
            exit 1
            ;;
    esac

    case "$RECEIVER_MODE" in
        unique|legacy-small-set) ;;
        *)
            echo "RECEIVER_MODE must be unique or legacy-small-set" >&2
            exit 1
            ;;
    esac

    normalize_and_validate_paths

    if [[ "$BUILD" == "1" ]]; then
        echo "[$(timestamp)] Building morph-reth and benchmark tool ($PROFILE profile)"
        cargo build --profile "$PROFILE" --bin morph-reth --bin bench-block-exec
    fi
    [[ -x "$RETH_BIN" ]] || { echo "reth binary not found: $RETH_BIN" >&2; exit 1; }
    [[ -x "$BENCH_BIN" ]] || { echo "benchmark binary not found: $BENCH_BIN" >&2; exit 1; }

    if [[ ! -s "$JWT_SECRET" ]]; then
        mkdir -p "$(dirname "$JWT_SECRET")"
        openssl rand -hex 32 > "$JWT_SECRET"
        chmod 600 "$JWT_SECRET"
    fi

    echo "[$(timestamp)] Building benchmark contracts and genesis"
    forge build --root "$CONTRACT_DIR" >/dev/null
    local token_code swap_code
    token_code=$(jq -er '.deployedBytecode.object' "$CONTRACT_DIR/out/BenchToken.sol/BenchToken.json")
    swap_code=$(jq -er '.deployedBytecode.object' "$CONTRACT_DIR/out/BenchSwap.sol/BenchSwap.json")
    "$BENCH_BIN" write-genesis \
        --output "$GENESIS" \
        --senders "$SENDERS" \
        --max-tx-per-block 1000000 \
        --max-tx-payload-bytes "$BENCHMARK_GENESIS_MAX_TX_PAYLOAD_BYTES" \
        --bench-token-code "$token_code" \
        --bench-swap-code "$swap_code"

    local payload_limit_disabled=false
    local enforced_max_tx_payload_bytes=737280
    if [[ "$BENCHMARK_DISABLE_TX_PAYLOAD_LIMIT" == "1" ]]; then
        payload_limit_disabled=true
        enforced_max_tx_payload_bytes=null
    fi

    jq -n \
        --arg created_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --arg git_commit "$(git rev-parse HEAD)" \
        --arg git_dirty "$(git status --porcelain | wc -l | tr -d ' ')" \
        --arg profile "$PROFILE" \
        --arg reth_version "$($RETH_BIN --version | tr '\n' ' ')" \
        --arg modes "$MODES" \
        --arg workloads "$WORKLOADS" \
        --arg receiver_mode "$RECEIVER_MODE" \
        --argjson senders "$SENDERS" \
        --argjson runs "$RUNS" \
        --arg exec_block_sizes "$EXEC_BLOCK_SIZES" \
        --argjson exec_blocks "$EXEC_BLOCKS" \
        --argjson sustained_txs_per_block "$SUSTAINED_TXS_PER_BLOCK" \
        --argjson sustained_blocks "$SUSTAINED_BLOCKS" \
        --argjson sustained_warmup_blocks "$SUSTAINED_WARMUP_BLOCKS" \
        --argjson openloop_target_tps "$OPENLOOP_TARGET_TPS" \
        --argjson openloop_duration_secs "$OPENLOOP_DURATION_SECS" \
        --argjson openloop_drain_secs "$OPENLOOP_DRAIN_SECS" \
        --argjson payload_limit_disabled "$payload_limit_disabled" \
        --argjson enforced_max_tx_payload_bytes "$enforced_max_tx_payload_bytes" \
        --argjson genesis_max_tx_payload_bytes "$(jq -er '.config.morph.maxTxPayloadBytesPerBlock' "$GENESIS")" \
        --argjson builder_deadline_secs "$BENCHMARK_BUILDER_DEADLINE_SECS" \
        --argjson txpool_max_count "$BENCHMARK_TXPOOL_MAX_COUNT" \
        --arg node_log_level "info" \
        --arg uname "$(uname -a)" \
        '{created_at:$created_at,git_commit:$git_commit,dirty_files:($git_dirty|tonumber),profile:$profile,reth_version:$reth_version,uname:$uname,modes:$modes,workloads:$workloads,receiver_mode:$receiver_mode,senders:$senders,runs:$runs,consensus:{tx_payload_limit_disabled:$payload_limit_disabled,enforced_max_tx_payload_bytes:$enforced_max_tx_payload_bytes,genesis_compatibility_max_tx_payload_bytes:$genesis_max_tx_payload_bytes},node:{builder_deadline_secs:$builder_deadline_secs,txpool_max_count:$txpool_max_count,log_level:$node_log_level},exec:{block_sizes:$exec_block_sizes,blocks:$exec_blocks},sustained:{txs_per_block:$sustained_txs_per_block,blocks:$sustained_blocks,warmup_blocks:$sustained_warmup_blocks},openloop:{target_tps:$openloop_target_tps,duration_secs:$openloop_duration_secs,drain_secs:$openloop_drain_secs}}' \
        > "$RESULTS_DIR/metadata.json"
}

wait_for_rpc() {
    local deadline=$((SECONDS + START_TIMEOUT_SECS))
    while (( SECONDS < deadline )); do
        if ! kill -0 "$NODE_PID" 2>/dev/null; then
            echo "morph-reth exited during startup" >&2
            tail -100 "$NODE_LOG" >&2 || true
            return 1
        fi
        if curl -fsS -H 'content-type: application/json' \
            --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
            "http://127.0.0.1:$HTTP_PORT" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.25
    done
    echo "RPC did not become ready within ${START_TIMEOUT_SECS}s" >&2
    tail -100 "$NODE_LOG" >&2 || true
    return 1
}

start_reth() {
    local run_name=$1
    local benchmark_payload_arg=
    if [[ "$BENCHMARK_DISABLE_TX_PAYLOAD_LIMIT" == "1" ]]; then
        benchmark_payload_arg=--morph.benchmark-disable-tx-payload-limit
    fi
    cleanup_node
    rm -rf -- "$DATA_DIR"
    mkdir -p "$DATA_DIR" "$RESULTS_DIR/logs"
    NODE_LOG="$RESULTS_DIR/logs/${run_name}-node.log"
    "$RETH_BIN" node \
        --chain "$GENESIS" --datadir "$DATA_DIR" \
        --color never --log.stdout.format log-fmt --log.stdout.filter info \
        --log.file.max-files 0 \
        --http --http.addr 127.0.0.1 --http.port "$HTTP_PORT" --http.api "web3,debug,eth,txpool,net" \
        --authrpc.addr 127.0.0.1 --authrpc.port "$AUTHRPC_PORT" --authrpc.jwtsecret "$JWT_SECRET" \
        --port "$P2P_PORT" --disable-discovery --nat none \
        --builder.deadline "$BENCHMARK_BUILDER_DEADLINE_SECS" \
        ${benchmark_payload_arg:+"$benchmark_payload_arg"} \
        --engine.persistence-threshold 2 --engine.memory-block-buffer-target 2 \
        --txpool.pending-max-count "$BENCHMARK_TXPOOL_MAX_COUNT" --txpool.pending-max-size 8192 \
        --txpool.basefee-max-count "$BENCHMARK_TXPOOL_MAX_COUNT" --txpool.basefee-max-size 8192 \
        --txpool.queued-max-count "$BENCHMARK_TXPOOL_MAX_COUNT" --txpool.queued-max-size 8192 \
        --txpool.max-account-slots 1000000 --txpool.additional-validation-tasks 12 \
        --txpool.max-batch-size 128 --txpool.disable-transactions-backup \
        --rpc.max-request-size 1024 --rpc.max-response-size 1024 --rpc.max-connections 1000 \
        >"$NODE_LOG" 2>&1 &
    NODE_PID=$!
    wait_for_rpc
}

COMMON_ARGS=(
    --engine-rpc "http://127.0.0.1:$AUTHRPC_PORT"
    --jwt-secret "$JWT_SECRET"
    --http-rpc "http://127.0.0.1:$HTTP_PORT"
    --senders "$SENDERS"
    --chain-id "$CHAIN_ID"
    --receiver-mode "$RECEIVER_MODE"
    --submit-batch-size 500
    --submit-concurrency 32
)

run_exec() {
    local workload=$1 block_size=$2 run=$3
    local name="exec-${workload}-${block_size}-run${run}"
    local output="$RESULTS_DIR/${name}.jsonl"
    echo "[$(timestamp)] $name"
    start_reth "$name"
    "$BENCH_BIN" run exec "${COMMON_ARGS[@]}" --workload "$workload" \
        --txs-per-block "$block_size" --blocks "$EXEC_BLOCKS" --output "$output" \
        2>&1 | tee "$RESULTS_DIR/logs/${name}-bench.log"
}

run_sustained() {
    local workload=$1 run=$2
    local name="sustained-${workload}-run${run}"
    local output="$RESULTS_DIR/${name}.jsonl"
    echo "[$(timestamp)] $name"
    start_reth "$name"
    "$BENCH_BIN" run sustained "${COMMON_ARGS[@]}" --workload "$workload" \
        --txs-per-block "$SUSTAINED_TXS_PER_BLOCK" --blocks "$SUSTAINED_BLOCKS" \
        --warmup-blocks "$SUSTAINED_WARMUP_BLOCKS" --output "$output" \
        2>&1 | tee "$RESULTS_DIR/logs/${name}-bench.log"
}

run_openloop() {
    local workload=$1 run=$2
    local name="openloop-${workload}-run${run}"
    local output="$RESULTS_DIR/${name}.jsonl"
    echo "[$(timestamp)] $name"
    start_reth "$name"
    "$BENCH_BIN" run openloop "${COMMON_ARGS[@]}" --workload "$workload" \
        --target-tps "$OPENLOOP_TARGET_TPS" --duration-secs "$OPENLOOP_DURATION_SECS" \
        --submit-buffer-ticks 64 --submit-tick-ms 50 --producer-idle-ms 5 \
        --drain-secs "$OPENLOOP_DRAIN_SECS" --output "$output" \
        2>&1 | tee "$RESULTS_DIR/logs/${name}-bench.log"
}

contains_mode() { [[ " $MODES " == *" $1 "* ]]; }

prepare
echo "Results: $RESULTS_DIR"

for workload in $WORKLOADS; do
    if contains_mode exec; then
        for block_size in $EXEC_BLOCK_SIZES; do
            for run in $(seq 1 "$RUNS"); do run_exec "$workload" "$block_size" "$run"; done
        done
    fi
    if contains_mode sustained; then
        for run in $(seq 1 "$RUNS"); do run_sustained "$workload" "$run"; done
    fi
    if contains_mode openloop; then
        for run in $(seq 1 "$RUNS"); do run_openloop "$workload" "$run"; done
    fi
done

cleanup_node
"$BENCH_BIN" summarize --v2 --results-dir "$RESULTS_DIR" --output "$RESULTS_DIR/summary.tsv"
echo "[$(timestamp)] Benchmark complete: $RESULTS_DIR"
