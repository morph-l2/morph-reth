#!/usr/bin/env bash
# ============================================================================
# run-test.sh — Local metrics smoke test for morph-reth
#
# Usage: ./local-test/run-test.sh [hoodi|morph]
#        Chain defaults to 'hoodi' (testnet — lighter genesis).
#
# What it does:
#   1. Generates a JWT secret
#   2. Starts morph-reth with --metrics 9001 (empty chain, from genesis)
#   3. Starts prometheus + grafana via docker-compose
#   4. Runs the Python script that calls all engine API methods
#   5. Shows a summary of which metrics appeared
#
# Once running, open: http://localhost:3000
# ============================================================================
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARY="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release/morph-reth"

CHAIN="${1:-hoodi}"
DATA_DIR="/tmp/morph-reth-metrics-test"
JWT_FILE="$SCRIPT_DIR/jwt.hex"

log() { printf "\033[96m==> %s\033[0m\n" "$*"; }
ok()  { printf "\033[92m✓  %s\033[0m\n" "$*"; }
die() { printf "\033[91m✗  %s\033[0m\n" "$*"; exit 1; }

# ── 0. Check binary ────────────────────────────────────────────────────────
[ -x "$BINARY" ] || die "morph-reth binary not found at $BINARY. Run: CARGO_TARGET_DIR=$REPO_ROOT/target cargo build --release --bin morph-reth"
ok "Binary: $BINARY"

# ── 1. Generate JWT secret ─────────────────────────────────────────────────
log "Generating JWT secret..."
openssl rand -hex 32 | tr -d '\n' > "$JWT_FILE"
ok "JWT secret: $JWT_FILE"

# ── 2. Prepare data directory ──────────────────────────────────────────────
log "Preparing data directory: $DATA_DIR"
rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"

# ── 3. Start morph-reth ────────────────────────────────────────────────────
log "Starting morph-reth (chain=$CHAIN)..."
"$BINARY" node \
  --chain "$CHAIN" \
  --datadir "$DATA_DIR" \
  --metrics 127.0.0.1:9001 \
  --authrpc.addr 127.0.0.1 \
  --authrpc.port 8551 \
  --authrpc.jwtsecret "$JWT_FILE" \
  --http --http.addr 127.0.0.1 --http.port 8545 \
  --http.api "eth,net,web3" \
  --p2p.disable \
  --log.stdout.format terminal \
  > "$DATA_DIR/morph-reth.log" 2>&1 &
RETH_PID=$!
ok "morph-reth started (PID=$RETH_PID)"

cleanup() {
  log "Stopping morph-reth and monitoring stack..."
  kill "$RETH_PID" 2>/dev/null || true
  cd "$SCRIPT_DIR" && docker compose -f docker-compose.monitoring.yml down --remove-orphans 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# ── 4. Start monitoring stack ──────────────────────────────────────────────
log "Starting prometheus + grafana..."
cd "$SCRIPT_DIR"
docker compose -f docker-compose.monitoring.yml up -d
ok "Grafana: http://localhost:3000  (admin/admin)"
ok "Prometheus: http://localhost:9090"

# ── 5. Trigger all metrics ─────────────────────────────────────────────────
log "Triggering metrics via engine API..."
cd "$SCRIPT_DIR"
JWT_SECRET="$JWT_FILE" python3 trigger-metrics.py

# ── 6. Keep running for inspection ────────────────────────────────────────
log "Done. Press Ctrl+C to stop."
log "Grafana: http://localhost:3000"
log "Logs: $DATA_DIR/morph-reth.log"
wait "$RETH_PID"
