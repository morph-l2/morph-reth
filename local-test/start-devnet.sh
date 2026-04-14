#!/usr/bin/env bash
# =============================================================================
# start-devnet.sh — Launch a 3-node local Morph devnet
#
# Topology
#   node1 (sequencer-execution): Engine API + HTTP + P2P:30303 + metrics:9001
#   node2 (sync):                          HTTP + P2P:30304 + metrics:9002
#   node3 (sync):                          HTTP + P2P:30305 + metrics:9003
#
# Prerequisites:
#   - devnet.json and jwt.hex must exist in this directory (run create-devnet.py first)
#   - morph-reth binary at BINARY (default: repo/target/release/morph-reth)
#
# Usage:
#   cd local-test && ./start-devnet.sh          # start
#   cd local-test && ./start-devnet.sh stop     # stop all
#   cd local-test && ./start-devnet.sh logs 1   # tail node1 logs
#   cd local-test && ./start-devnet.sh status   # show running nodes
# =============================================================================
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARY="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release/morph-reth"
DATA_ROOT="/tmp/morph-devnet"
JWT_FILE="$SCRIPT_DIR/jwt.hex"
GENESIS="$SCRIPT_DIR/devnet.json"
PID_FILE="$DATA_ROOT/pids"

G="\033[92m"; Y="\033[93m"; C="\033[96m"; R="\033[91m"; N="\033[0m"
log()  { printf "${C}==> %s${N}\n" "$*"; }
ok()   { printf "${G}✓  %s${N}\n" "$*"; }
warn() { printf "${Y}!  %s${N}\n" "$*"; }
die()  { printf "${R}✗  %s${N}\n" "$*"; exit 1; }

# ── helpers ──────────────────────────────────────────────────────────────────
stop_all() {
    if [[ -f "$PID_FILE" ]]; then
        while IFS= read -r pid; do
            kill "$pid" 2>/dev/null && echo "  killed $pid" || true
        done < "$PID_FILE"
        rm -f "$PID_FILE"
    fi
    # Also kill by name pattern
    pkill -f "morph-reth node --chain $GENESIS" 2>/dev/null || true
    ok "All nodes stopped"
}

tail_logs() {
    local n="${1:-1}"
    tail -f "$DATA_ROOT/node${n}/morph-reth.log"
}

show_status() {
    echo "Data root: $DATA_ROOT"
    for i in 1 2 3; do
        local d="$DATA_ROOT/node$i"
        if [[ -f "$d/morph-reth.pid" ]]; then
            local pid; pid=$(cat "$d/morph-reth.pid")
            if kill -0 "$pid" 2>/dev/null; then
                printf "  ${G}node%d${N}: running (pid=%s)\n" "$i" "$pid"
            else
                printf "  ${R}node%d${N}: dead (pid=%s)\n" "$i" "$pid"
            fi
        else
            printf "  ${Y}node%d${N}: not started\n" "$i"
        fi
    done
}

case "${1:-start}" in
    stop)   stop_all; exit 0 ;;
    logs)   tail_logs "${2:-1}"; exit 0 ;;
    status) show_status; exit 0 ;;
esac

# ── pre-flight checks ─────────────────────────────────────────────────────────
[[ -x "$BINARY"  ]] || die "morph-reth not found at $BINARY"
[[ -f "$GENESIS" ]] || die "devnet.json not found; run: python3 create-devnet.py"
[[ -f "$JWT_FILE" ]] || {
    log "Generating JWT secret..."
    openssl rand -hex 32 | tr -d '\n' > "$JWT_FILE"
    ok "JWT written to $JWT_FILE"
}

# Stop any previous run
stop_all 2>/dev/null || true

# ── prepare data dirs ─────────────────────────────────────────────────────────
for i in 1 2 3; do
    rm -rf "$DATA_ROOT/node$i"
    mkdir -p "$DATA_ROOT/node$i"
done
> "$PID_FILE"
ok "Data directories ready under $DATA_ROOT"

# ── start node 1 (sequencer execution layer) ─────────────────────────────────
log "Starting node1 (sequencer, engine API on 8551)..."
"$BINARY" node \
    --chain "$GENESIS" \
    --datadir "$DATA_ROOT/node1" \
    --metrics 127.0.0.1:9001 \
    --authrpc.addr 127.0.0.1 --authrpc.port 8551 --authrpc.jwtsecret "$JWT_FILE" \
    --http --http.addr 127.0.0.1 --http.port 8545 \
    --http.api "eth,net,web3,txpool,debug" \
    --port 30303 \
    --log.stdout.format terminal \
    2>&1 | tee "$DATA_ROOT/node1/morph-reth.log" &
NODE1_PID=$!
echo "$NODE1_PID" >> "$PID_FILE"
echo "$NODE1_PID" > "$DATA_ROOT/node1/morph-reth.pid"
ok "node1 started (pid=$NODE1_PID)"

# ── wait for node1 metrics + enode ───────────────────────────────────────────
log "Waiting for node1 to initialize..."
for i in $(seq 1 30); do
    sleep 1
    if curl -s http://127.0.0.1:9001 >/dev/null 2>&1; then
        ok "node1 metrics endpoint ready (${i}s)"
        break
    fi
    [[ $i -eq 30 ]] && die "node1 didn't start in 30s. Check: $DATA_ROOT/node1/morph-reth.log"
done

# Extract enode from logs
ENODE=""
for i in $(seq 1 10); do
    ENODE=$(grep -m1 "enode://" "$DATA_ROOT/node1/morph-reth.log" 2>/dev/null \
            | python3 -c "
import sys, re
line = sys.stdin.read()
m = re.search(r'enode://([a-f0-9]+)@', line)
if m: print('enode://{}@127.0.0.1:30303'.format(m.group(1)))
" 2>/dev/null || true)
    [[ -n "$ENODE" ]] && break
    sleep 1
done

if [[ -z "$ENODE" ]]; then
    warn "Could not extract enode; node2/3 will use --trusted-peers workaround"
    ENODE=""
else
    ok "node1 enode: $ENODE"
    echo "$ENODE" > "$DATA_ROOT/node1/enode.txt"
fi

# ── start node2 (sync) ────────────────────────────────────────────────────────
log "Starting node2 (sync, HTTP on 8546, P2P 30304)..."
PEER_ARGS=()
[[ -n "$ENODE" ]] && PEER_ARGS=(--trusted-peers "$ENODE")

"$BINARY" node \
    --chain "$GENESIS" \
    --datadir "$DATA_ROOT/node2" \
    --metrics 127.0.0.1:9002 \
    --authrpc.port 8552 \
    --http --http.addr 127.0.0.1 --http.port 8546 \
    --http.api "eth,net,web3,txpool" \
    --port 30304 --discovery.port 30304 \
    --log.stdout.format terminal \
    "${PEER_ARGS[@]}" \
    2>&1 | tee "$DATA_ROOT/node2/morph-reth.log" &
NODE2_PID=$!
echo "$NODE2_PID" >> "$PID_FILE"
echo "$NODE2_PID" > "$DATA_ROOT/node2/morph-reth.pid"
ok "node2 started (pid=$NODE2_PID)"

# ── start node3 (sync) ────────────────────────────────────────────────────────
log "Starting node3 (sync, HTTP on 8547, P2P 30305)..."
"$BINARY" node \
    --chain "$GENESIS" \
    --datadir "$DATA_ROOT/node3" \
    --metrics 127.0.0.1:9003 \
    --authrpc.port 8553 \
    --http --http.addr 127.0.0.1 --http.port 8547 \
    --http.api "eth,net,web3,txpool" \
    --port 30305 --discovery.port 30305 \
    --log.stdout.format terminal \
    "${PEER_ARGS[@]}" \
    2>&1 | tee "$DATA_ROOT/node3/morph-reth.log" &
NODE3_PID=$!
echo "$NODE3_PID" >> "$PID_FILE"
echo "$NODE3_PID" > "$DATA_ROOT/node3/morph-reth.pid"
ok "node3 started (pid=$NODE3_PID)"

# ── wait for node2/3 metrics ─────────────────────────────────────────────────
log "Waiting for node2 and node3..."
for port in 9002 9003; do
    for i in $(seq 1 20); do
        sleep 1
        curl -s "http://127.0.0.1:$port" >/dev/null 2>&1 && break
        [[ $i -eq 20 ]] && warn "node on port $port not ready"
    done
    ok "  :$port ready"
done

# ── start monitoring stack ────────────────────────────────────────────────────
log "Starting prometheus + grafana..."
cd "$SCRIPT_DIR"
docker compose -f docker-compose.monitoring.yml up -d 2>&1 | grep -E "Started|Running|Error" || true
ok "Grafana: http://localhost:3000 (admin/admin)"
ok "Prometheus: http://localhost:9090"

# ── summary ──────────────────────────────────────────────────────────────────
echo ""
printf "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${N}\n"
printf "${G}  Morph Devnet Running (3 nodes)${N}\n"
printf "${G}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${N}\n"
echo ""
echo "  node1 (sequencer): http://127.0.0.1:8545  engine:8551  metrics:9001"
echo "  node2 (sync):      http://127.0.0.1:8546             metrics:9002"
echo "  node3 (sync):      http://127.0.0.1:8547             metrics:9003"
echo ""
echo "  Grafana:    http://localhost:3000/d/morph-engine-api"
echo "  Prometheus: http://localhost:9090"
echo ""
echo "  Logs:"
echo "    tail -f $DATA_ROOT/node1/morph-reth.log"
echo "    tail -f $DATA_ROOT/node2/morph-reth.log"
echo ""
echo "  Next: python3 produce-blocks.py"
echo "  Stop: ./start-devnet.sh stop"
echo ""
