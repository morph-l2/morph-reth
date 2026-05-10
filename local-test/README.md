# Local Test Scripts

Scripts for running a morph-reth + morphnode full node locally, supporting both **mainnet** and **Hoodi testnet**.

## Prerequisites

- **morph-reth**: `cargo build --release --bin morph-reth`
- **morphnode**: `cd ../morph/node && make build`
- **pm2**: `npm install -g pm2`
- **jq**, **curl**, **unzip**

## Quick Start

```bash
# Mainnet (default)
./local-test/start-all.sh

# Hoodi testnet
./local-test/start-all.sh hoodi
```

All scripts accept an optional network argument as the first positional parameter. If omitted, defaults to `mainnet`.

## Scripts

| Script | Description |
|--------|-------------|
| `start-all.sh [network]` | Prepare config, start morph-reth, wait for RPC, start morphnode |
| `stop-all.sh [network]` | Stop morphnode and morph-reth |
| `status.sh [network]` | Show process status, RPC info, and morphnode sync progress |
| `reset.sh [network] [--yes]` | Wipe chain data (keeps config/keys), requires confirmation |
| `prepare.sh [network]` | Download config bundle and generate JWT secret if missing |
| `reth-start.sh [network]` | Start morph-reth only |
| `reth-stop.sh [network]` | Stop morph-reth only |
| `node-start.sh [network]` | Start morphnode only |
| `node-stop.sh [network]` | Stop morphnode only |

## Data Directory Layout

Each network gets its own isolated data directory:

```
local-test/
  jwt-secret.txt              # Shared JWT secret (auto-generated)
  mainnet/
    reth-data/                 # morph-reth chain database
    node-data/config/          # genesis.json, config.toml, keys
    node-data/data/            # Tendermint state
    reth.log, node.log
  hoodi/
    reth-data/
    node-data/config/
    node-data/data/
    reth.log, node.log
```

Switching networks does not require a reset — data is fully isolated.

## Configuration

All defaults can be overridden via environment variables. Common ones:

| Variable | Default | Description |
|----------|---------|-------------|
| `MORPH_NETWORK` | `mainnet` | Network selection (`mainnet` or `hoodi`) |
| `RETH_BIN` | `./target/release/morph-reth` | Path to morph-reth binary |
| `MORPHNODE_BIN` | `../morph/node/build/bin/morphnode` | Path to morphnode binary |
| `RETH_HTTP_PORT` | `8545` | HTTP RPC port |
| `RETH_AUTHRPC_PORT` | `8551` | Engine API auth RPC port |
| `MORPH_NODE_L1_RPC` | *(per-network default)* | L1 Ethereum RPC endpoint |

Example with overrides:

```bash
RETH_HTTP_PORT=9545 ./local-test/start-all.sh hoodi
```

> **Note**: Morph bootnodes are hardcoded in the chainspec — no need to pass `--bootnodes` manually.

## Monitoring

```bash
pm2 list                    # Process status
pm2 logs                    # All logs (live)
pm2 logs morph-reth         # morph-reth logs only
pm2 logs morph-node         # morphnode logs only
pm2 monit                   # Real-time resource monitoring
./local-test/status.sh      # RPC status + morphnode sync info
```

## Reset

To wipe chain data and start syncing from scratch:

```bash
./local-test/reset.sh                # Reset mainnet (interactive)
./local-test/reset.sh hoodi --yes    # Reset hoodi (no confirmation)
```

This removes reth's canonical DB, RocksDB history indices, static files, ExEx state, Morph reference index state, and `node-data/data/` for the specified network. Config files (genesis, keys), `reth.toml`, discovery secrets, and logs are preserved.

## Storage & engine tuning

- **Storage V2 is reth's default from v2.0.0.** Hot/cold layout: MDBX for state/trie, RocksDB for history indices, static files for changesets. V1 and V2 databases are **not interchangeable** — upgrading from a V1 data directory requires a full re-sync via `reset.sh`.

- `reth-start.sh` passes `--engine.persistence-threshold 256` / `--engine.memory-block-buffer-target 16` / `--engine.persistence-backpressure-threshold 512` (upstream defaults are 8 / 4 / 16). These batch MDBX writes so they do not compete with the morphnode Tendermint LevelDB fsyncs when both run on the same host.

  - **Backpressure semantics**: the engine stops executing new payloads once `canonical_tip - last_persisted_block` exceeds the backpressure threshold. We raised it to 512 to absorb fsync spikes; reth enforces that it must be `>` `--engine.persistence-threshold`.
  - **When to revert**: if morphnode is moved to a separate host (or its fsyncs no longer contend with reth), these flags can be dropped back to defaults.
