#!/usr/bin/env bash
# Reproducible Contender -> Morph benchmark. Contender generates all measured
# transactions; bench-block-exec only assembles/imports Morph L2 blocks.
set -Eeuo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT_DIR"

CONTENDER_BIN=${CONTENDER_BIN:-contender}
RETH_BIN=${RETH_BIN:-"$ROOT_DIR/target/release/morph-reth"}
BENCH_BIN=${BENCH_BIN:-"$ROOT_DIR/target/release/bench-block-exec"}
JWT_SECRET=${JWT_SECRET:-"$ROOT_DIR/local-test/jwt-secret.txt"}
GENESIS_SOURCE=${GENESIS_SOURCE:-}
RESULTS_DIR=${RESULTS_DIR:-"/tmp/morph-reth-bench/contender-$(date -u +%Y%m%dT%H%M%SZ)"}
WORKLOAD=${WORKLOAD:-hot-eth}
SCENARIO=${SCENARIO:-"$ROOT_DIR/local-test/contender-hot-eth.toml"}
TARGET_TPS=${TARGET_TPS:-200000}
DURATION_SECS=${DURATION_SECS:-120}
ACCOUNTS=${ACCOUNTS:-2000}
RUNS=${RUNS:-3}
SHARDS=${SHARDS:-1}
SHARD_SEED_STRIDE=${SHARD_SEED_STRIDE:-1000000}
RPC_BATCH_SIZE=${RPC_BATCH_SIZE:-500}
OPTIMISTIC_NONCES=${OPTIMISTIC_NONCES:-1}
PRODUCER_IDLE_MS=${PRODUCER_IDLE_MS:-20}
DRAIN_SECS=${DRAIN_SECS:-600}
SEED=${SEED:-0x20260826}
HTTP_PORT=${HTTP_PORT:-18545}
AUTHRPC_PORT=${AUTHRPC_PORT:-18551}
P2P_PORT=${P2P_PORT:-30313}
FUNDER_PRIVATE_KEY=${FUNDER_PRIVATE_KEY:-}
FUNDER_PRIVATE_KEYS=${FUNDER_PRIVATE_KEYS:-}

NODE_PID=""
PRODUCER_PID=""
CONTENDER_PIDS=()

cleanup() {
    local pid
    for pid in "${CONTENDER_PIDS[@]:-}"; do
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if [[ -n "$PRODUCER_PID" ]] && kill -0 "$PRODUCER_PID" 2>/dev/null; then
        kill "$PRODUCER_PID" 2>/dev/null || true
        wait "$PRODUCER_PID" 2>/dev/null || true
    fi
    if [[ -n "$NODE_PID" ]] && kill -0 "$NODE_PID" 2>/dev/null; then
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

for command in curl jq sqlite3; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "missing required command: $command" >&2
        exit 1
    }
done

[[ -x "$CONTENDER_BIN" ]] || { echo "contender binary not found: $CONTENDER_BIN" >&2; exit 1; }
[[ -x "$RETH_BIN" ]] || { echo "morph-reth binary not found: $RETH_BIN" >&2; exit 1; }
[[ -x "$BENCH_BIN" ]] || { echo "benchmark binary not found: $BENCH_BIN" >&2; exit 1; }
[[ -f "$JWT_SECRET" ]] || { echo "JWT secret not found: $JWT_SECRET" >&2; exit 1; }
[[ -n "$GENESIS_SOURCE" && -f "$GENESIS_SOURCE" ]] || {
    echo "GENESIS_SOURCE must name a benchmark genesis file" >&2
    exit 1
}
if [[ "$SHARDS" == "1" && -z "$FUNDER_PRIVATE_KEYS" ]]; then
    FUNDER_PRIVATE_KEYS=$FUNDER_PRIVATE_KEY
fi
IFS=',' read -r -a funder_keys <<< "$FUNDER_PRIVATE_KEYS"
if (( ${#funder_keys[@]} != SHARDS )); then
    echo "FUNDER_PRIVATE_KEYS must contain exactly SHARDS comma-separated funded keys" >&2
    exit 1
fi
(( TARGET_TPS % SHARDS == 0 )) || { echo "TARGET_TPS must be divisible by SHARDS" >&2; exit 1; }
(( ACCOUNTS % SHARDS == 0 )) || { echo "ACCOUNTS must be divisible by SHARDS" >&2; exit 1; }
case "$WORKLOAD" in
    hot-eth) [[ -f "$SCENARIO" ]] || { echo "scenario not found: $SCENARIO" >&2; exit 1; } ;;
    erc20-unique) ;;
    *) echo "WORKLOAD must be hot-eth or erc20-unique" >&2; exit 1 ;;
esac

mkdir -p "$RESULTS_DIR"
RESULTS_DIR=$(cd "$RESULTS_DIR" && pwd -P)
if find "$RESULTS_DIR" -mindepth 1 -print -quit | grep -q .; then
    echo "RESULTS_DIR must be empty: $RESULTS_DIR" >&2
    exit 1
fi
cp "$GENESIS_SOURCE" "$RESULTS_DIR/genesis.json"

wait_for_rpc() {
    local deadline=$((SECONDS + 60))
    while (( SECONDS < deadline )); do
        if ! kill -0 "$NODE_PID" 2>/dev/null; then
            echo "morph-reth exited during startup" >&2
            return 1
        fi
        if curl -fsS -H 'content-type: application/json' \
            --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
            "http://127.0.0.1:$HTTP_PORT" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    echo "RPC did not become ready" >&2
    return 1
}

summarize_run() {
    local run_dir=$1
    local producer_jsonl="$run_dir/producer.jsonl"
    local planned=0 rows=0 confirmed=0 failed=0 pending=0 unique_hashes=0
    local first_start="" last_start="" first_block="" last_block=""
    local shard db run_id value shard_first_start shard_last_start shard_first_block shard_last_block
    for shard in $(seq 1 "$SHARDS"); do
        db="$run_dir/contender-$shard/contender.db"
        run_id=$(sqlite3 "$db" 'select max(id) from runs;')
        value=$(sqlite3 "$db" "select tx_count from runs where id=$run_id;"); planned=$((planned + value))
        value=$(sqlite3 "$db" "select count(*) from run_txs where run_id=$run_id;"); rows=$((rows + value))
        value=$(sqlite3 "$db" "select count(*) from run_txs where run_id=$run_id and block_number is not null and error is null;"); confirmed=$((confirmed + value))
        value=$(sqlite3 "$db" "select count(*) from run_txs where run_id=$run_id and error is not null;"); failed=$((failed + value))
        value=$(sqlite3 "$db" "select count(*) from run_txs where run_id=$run_id and block_number is null and error is null;"); pending=$((pending + value))
        value=$(sqlite3 "$db" "select count(distinct tx_hash) from run_txs where run_id=$run_id;"); unique_hashes=$((unique_hashes + value))
        shard_first_start=$(sqlite3 "$db" "select min(start_timestamp) from run_txs where run_id=$run_id;")
        shard_last_start=$(sqlite3 "$db" "select max(start_timestamp) from run_txs where run_id=$run_id;")
        shard_first_block=$(sqlite3 "$db" "select min(block_number) from run_txs where run_id=$run_id and block_number is not null and error is null;")
        shard_last_block=$(sqlite3 "$db" "select max(block_number) from run_txs where run_id=$run_id and block_number is not null and error is null;")
        [[ -z "$first_start" || "$shard_first_start" -lt "$first_start" ]] && first_start=$shard_first_start
        [[ -z "$last_start" || "$shard_last_start" -gt "$last_start" ]] && last_start=$shard_last_start
        [[ -z "$first_block" || "$shard_first_block" -lt "$first_block" ]] && first_block=$shard_first_block
        [[ -z "$last_block" || "$shard_last_block" -gt "$last_block" ]] && last_block=$shard_last_block
        jq -n \
            --argjson shard "$shard" --argjson planned "$(sqlite3 "$db" "select tx_count from runs where id=$run_id;")" \
            --argjson rows "$(sqlite3 "$db" "select count(*) from run_txs where run_id=$run_id;")" \
            --argjson confirmed "$(sqlite3 "$db" "select count(*) from run_txs where run_id=$run_id and block_number is not null and error is null;")" \
            --argjson failed "$(sqlite3 "$db" "select count(*) from run_txs where run_id=$run_id and error is not null;")" \
            --argjson pending "$(sqlite3 "$db" "select count(*) from run_txs where run_id=$run_id and block_number is null and error is null;")" \
            '{shard:$shard,planned:$planned,db_rows:$rows,confirmed:$confirmed,failed:$failed,pending:$pending}' \
            > "$run_dir/shard-$shard.json"
    done

    # Count hashes across every shard, not just within each DB. Adjacent
    # Contender seeds generate overlapping account ranges (`seed + i`), so a
    # per-shard distinct count alone can falsely report a valid aggregate run.
    if (( SHARDS > 1 )); then
        local attach_sql="" union_sql="select tx_hash from main.run_txs"
        for shard in $(seq 2 "$SHARDS"); do
            attach_sql+="attach '$run_dir/contender-$shard/contender.db' as shard$shard; "
            union_sql+=" union all select tx_hash from shard$shard.run_txs"
        done
        unique_hashes=$(sqlite3 "$run_dir/contender-1/contender.db" \
            "$attach_sql select count(distinct tx_hash) from ($union_sql);")
    fi

    jq -s \
        --argjson first_block "$first_block" \
        --argjson last_block "$last_block" \
        '[.[] | select(.block_number >= $first_block and .block_number <= $last_block)]
         | {producer_block_rows:length,
            producer_chain_txs:(map(.tx_count)|add // 0),
            producer_elapsed_ms:(map(.total_ms)|add // 0),
            producer_realized_tps:((map(.tx_count)|add // 0) / ((map(.total_ms)|add // 0) / 1000)),
            producer_avg_block_tps:((map(.tps)|add // 0) / length),
            producer_nonempty_blocks:(map(select(.tx_count > 0))|length)}' \
        "$producer_jsonl" > "$run_dir/producer-window.json"

    jq -n \
        --arg workload "$WORKLOAD" \
        --argjson target_tps "$TARGET_TPS" \
        --argjson duration_secs "$DURATION_SECS" \
        --argjson accounts "$ACCOUNTS" \
        --argjson shards "$SHARDS" \
        --argjson rpc_batch_size "$RPC_BATCH_SIZE" \
        --argjson optimistic_nonces "$OPTIMISTIC_NONCES" \
        --argjson planned "$planned" \
        --argjson rows "$rows" \
        --argjson confirmed "$confirmed" \
        --argjson failed "$failed" \
        --argjson pending "$pending" \
        --argjson unique_hashes "$unique_hashes" \
        --argjson first_start_ms "$first_start" \
        --argjson last_start_ms "$last_start" \
        --argjson first_block "$first_block" \
        --argjson last_block "$last_block" \
        --slurpfile producer "$run_dir/producer-window.json" \
        '{workload:$workload,target_tps:$target_tps,duration_secs:$duration_secs,accounts:$accounts,
          shards:$shards,
          rpc_batch_size:$rpc_batch_size,optimistic_nonces:($optimistic_nonces == 1),
          planned:$planned,db_rows:$rows,confirmed:$confirmed,
          failed:$failed,pending:$pending,unique_hashes:$unique_hashes,
          first_start_ms:$first_start_ms,last_start_ms:$last_start_ms,
          send_span_secs:(($last_start_ms-$first_start_ms)/1000),
          effective_send_secs:([$duration_secs,(($last_start_ms-$first_start_ms)/1000+1)]|max),
          observed_send_tps:($rows/([$duration_secs,(($last_start_ms-$first_start_ms)/1000+1)]|max)),
          confirmed_per_nominal_sec:($confirmed/$duration_secs),
          first_block:$first_block,last_block:$last_block} + $producer[0]' \
        > "$run_dir/metrics.json"
}

jq -n \
    --arg git_commit "$(git rev-parse HEAD)" \
    --arg contender_version "$($CONTENDER_BIN --version)" \
    --arg workload "$WORKLOAD" \
    --argjson target_tps "$TARGET_TPS" \
    --argjson duration_secs "$DURATION_SECS" \
    --argjson accounts "$ACCOUNTS" \
    --argjson runs "$RUNS" \
    --argjson shards "$SHARDS" \
    --argjson shard_seed_stride "$SHARD_SEED_STRIDE" \
    --argjson rpc_batch_size "$RPC_BATCH_SIZE" \
    --argjson optimistic_nonces "$OPTIMISTIC_NONCES" \
    --arg seed "$SEED" \
    '{git_commit:$git_commit,contender_version:$contender_version,workload:$workload,
      target_tps:$target_tps,duration_secs:$duration_secs,accounts:$accounts,runs:$runs,
      shards:$shards,shard_seed_stride:$shard_seed_stride,rpc_batch_size:$rpc_batch_size,
      optimistic_nonces:($optimistic_nonces == 1),seed:$seed}' > "$RESULTS_DIR/metadata.json"

for run in $(seq 1 "$RUNS"); do
    run_dir="$RESULTS_DIR/run-$run"
    mkdir -p "$run_dir/reth-data"
    stop_file="$run_dir/producer.stop"

    "$RETH_BIN" node \
        --chain "$RESULTS_DIR/genesis.json" --datadir "$run_dir/reth-data" \
        --color never --log.stdout.format log-fmt --log.stdout.filter warn --log.file.max-files 0 \
        --http --http.addr 127.0.0.1 --http.port "$HTTP_PORT" --http.api web3,debug,eth,txpool,net \
        --authrpc.addr 127.0.0.1 --authrpc.port "$AUTHRPC_PORT" --authrpc.jwtsecret "$JWT_SECRET" \
        --port "$P2P_PORT" --disable-discovery --nat none \
        --builder.deadline 12 --morph.benchmark-disable-tx-payload-limit \
        --engine.persistence-threshold 2 --engine.memory-block-buffer-target 2 \
        --txpool.pending-max-count 30000000 --txpool.pending-max-size 8192 \
        --txpool.basefee-max-count 30000000 --txpool.basefee-max-size 8192 \
        --txpool.queued-max-count 30000000 --txpool.queued-max-size 8192 \
        --txpool.max-account-slots 1000000 --txpool.additional-validation-tasks 12 \
        --txpool.max-batch-size 128 --txpool.disable-transactions-backup \
        --rpc.max-request-size 1024 --rpc.max-response-size 1024 --rpc.max-connections 1000 \
        > "$run_dir/node.log" 2>&1 &
    NODE_PID=$!
    wait_for_rpc

    # Initialize fresh DBs without `db reset`: Contender 0.10.3 reset deletes
    # the database while its own connection pool is still open.
    for shard in $(seq 1 "$SHARDS"); do
        mkdir -p "$run_dir/contender-$shard"
        "$CONTENDER_BIN" --data-dir "$run_dir/contender-$shard" db export "$run_dir/empty-$shard.db" \
            > "$run_dir/db-init-$shard.log" 2>&1
    done

    "$BENCH_BIN" run produce \
        --engine-rpc "http://127.0.0.1:$AUTHRPC_PORT" \
        --jwt-secret "$JWT_SECRET" \
        --http-rpc "http://127.0.0.1:$HTTP_PORT" \
        --output "$run_dir/producer.jsonl" \
        --stop-file "$stop_file" \
        --producer-idle-ms "$PRODUCER_IDLE_MS" \
        --drain-secs "$DRAIN_SECS" \
        > "$run_dir/producer.log" 2>&1 &
    PRODUCER_PID=$!

    CONTENDER_PIDS=()
    contender_status=0
    for shard in $(seq 1 "$SHARDS"); do
        shard_seed=$(printf '0x%x' $((SEED + (shard - 1) * SHARD_SEED_STRIDE)))
        contender_args=(
            --data-dir "$run_dir/contender-$shard" spam
            --tps "$((TARGET_TPS / SHARDS))" --duration "$DURATION_SECS"
            --priv-key "${funder_keys[$((shard - 1))]}"
            --rpc-url "http://127.0.0.1:$HTTP_PORT"
            --accounts-per-agent "$((ACCOUNTS / SHARDS))"
            --seed "$shard_seed"
            --pending-timeout 600
            --report-interval 1
            --gas-price 1000000
            --tx-type eip1559
            --rpc-batch-size "$RPC_BATCH_SIZE"
        )
        if [[ "$OPTIMISTIC_NONCES" == "1" ]]; then
            contender_args+=(--optimistic-nonces)
        fi
        case "$WORKLOAD" in
            hot-eth) contender_args+=("$SCENARIO") ;;
            erc20-unique) contender_args+=(erc20 --send-amount 1 --fund-amount 1000000) ;;
        esac
        RUST_LOG=warn "$CONTENDER_BIN" "${contender_args[@]}" > "$run_dir/contender-$shard.log" 2>&1 &
        CONTENDER_PIDS+=("$!")
    done
    set +e
    for contender_pid in "${CONTENDER_PIDS[@]}"; do
        wait "$contender_pid" || contender_status=$?
    done
    set -e
    CONTENDER_PIDS=()
    touch "$stop_file"
    wait "$PRODUCER_PID"
    PRODUCER_PID=""
    if (( contender_status != 0 )); then
        echo "Contender failed in run $run; see $run_dir/contender.log" >&2
        exit "$contender_status"
    fi

    summarize_run "$run_dir"
    jq -c . "$run_dir/metrics.json"

    kill "$NODE_PID" 2>/dev/null || true
    wait "$NODE_PID" 2>/dev/null || true
    NODE_PID=""
done

jq -s \
    '{runs:length,
      confirmed_mean:(map(.confirmed)|add/length),
      confirmed_per_nominal_sec_mean:(map(.confirmed_per_nominal_sec)|add/length),
      producer_realized_tps_mean:(map(.producer_realized_tps)|add/length),
      producer_realized_tps_min:(map(.producer_realized_tps)|min),
      producer_realized_tps_max:(map(.producer_realized_tps)|max),
      observed_send_tps_mean:(map(.observed_send_tps)|add/length),
      send_span_secs_mean:(map(.send_span_secs)|add/length),
      total_failed:(map(.failed)|add),total_pending:(map(.pending)|add),
      all_counts_match:(all(.db_rows == .planned and .confirmed == .planned and
                            .unique_hashes == .planned and .producer_chain_txs == .confirmed))}' \
    "$RESULTS_DIR"/run-*/metrics.json > "$RESULTS_DIR/summary.json"
jq . "$RESULTS_DIR/summary.json"
