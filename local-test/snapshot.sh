#!/usr/bin/env bash
# Package a morph-reth datadir into a portable .tar.zst snapshot.
#
# Usage:
#   ./local-test/snapshot.sh <mainnet|hoodi> [--with-exex] [--output DIR] [--level N]
#
# Defaults:
#   - exclude exex/, discovery-secret, reth.toml, known-peers.json, *.log
#   - zstd -9 -T0 (multi-threaded; bump --level for smaller, slower)
#   - output to local-test/snapshots/

set -euo pipefail

NETWORK="${1:-}"
shift || true

WITH_EXEX=0
OUTPUT_DIR=""
COMPRESS_LEVEL=9

while [[ $# -gt 0 ]]; do
  case "$1" in
    --with-exex) WITH_EXEX=1; shift ;;
    --output)    OUTPUT_DIR="$2"; shift 2 ;;
    --level)     COMPRESS_LEVEL="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "Unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ "$NETWORK" != "mainnet" && "$NETWORK" != "hoodi" ]]; then
  echo "Usage: $0 <mainnet|hoodi> [--with-exex] [--output DIR] [--level N]" >&2
  exit 2
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NETWORK_DIR="$REPO_ROOT/local-test/$NETWORK"
DATA_DIR="$NETWORK_DIR/reth-data"
OUTPUT_DIR="${OUTPUT_DIR:-$REPO_ROOT/local-test/snapshots}"

[[ -d "$DATA_DIR" ]] || { echo "Data dir not found: $DATA_DIR" >&2; exit 1; }

# Refuse to pack a live datadir — files would be inconsistent.
if pgrep -f 'target/release/morph-reth node' >/dev/null 2>&1 \
   || pgrep -f 'target/debug/morph-reth node'   >/dev/null 2>&1; then
  echo "ERROR: morph-reth appears to be running. Stop it (./local-test/reth-stop.sh) first." >&2
  exit 1
fi

# Tip hint: take the largest end-block from transactions static-file segments.
# This is approximate — it lags the true canonical tip by at most one segment
# range (~500k blocks). Used only for the snapshot filename.
TIP=$(ls "$DATA_DIR/static_files" 2>/dev/null \
        | grep -E '^static_file_transactions_[0-9]+_[0-9]+$' \
        | sed -E 's/.*_([0-9]+)$/\1/' \
        | sort -n | tail -1 || true)
TIP=${TIP:-unknown}

mkdir -p "$OUTPUT_DIR"
TS=$(date -u +%Y%m%dT%H%M%SZ)
SUFFIX=""; [[ $WITH_EXEX -eq 1 ]] && SUFFIX="-withexex"
OUT_NAME="morph-reth-${NETWORK}-tip${TIP}-${TS}${SUFFIX}.tar.zst"
OUT_PATH="$OUTPUT_DIR/$OUT_NAME"

EXCLUDES=(
  --exclude='reth-data/discovery-secret'
  --exclude='reth-data/reth.toml'
  --exclude='reth-data/known-peers.json'
  --exclude='reth-data/*.log'
  --exclude='reth-data/node-*.log'
)
[[ $WITH_EXEX -eq 0 ]] && EXCLUDES+=( --exclude='reth-data/exex' )

echo "Snapshot config:"
echo "  network:   $NETWORK"
echo "  data dir:  $DATA_DIR"
echo "  with-exex: $WITH_EXEX"
echo "  tip hint:  $TIP"
echo "  level:     zstd -${COMPRESS_LEVEL} -T0"
echo "  output:    $OUT_PATH"
echo

# `tar -C $NETWORK_DIR reth-data` so the archive's top-level path is `reth-data/`,
# which restores cleanly into any parent dir.
time tar --use-compress-program "zstd -${COMPRESS_LEVEL} -T0" \
  "${EXCLUDES[@]}" \
  -cf "$OUT_PATH" \
  -C "$NETWORK_DIR" \
  reth-data

echo
ls -lh "$OUT_PATH"
echo
echo "Restore (use pipe form — bsdtar on macOS fails with --use-compress-program):"
echo "  mkdir -p <target>"
echo "  zstd -dc $OUT_PATH | tar -xf - -C <target>"
echo "  # results in <target>/reth-data/{db,rocksdb,static_files,morph,...}"
