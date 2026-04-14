#!/usr/bin/env python3
"""
produce-blocks.py — Simulates morphnode driving node1's engine API.

What it does (continuously):
  1. Send real ETH-transfer transactions from funded test accounts → txpool
  2. Call engine_assembleL2Block  — includes pool txs in the block
  3. Call engine_validateL2Block  — validates the assembled block
  4. Call engine_newL2Block       — imports the block (triggers P2P broadcast)
  5. Repeat for the next block

Every ~10 blocks also:
  - Call newSafeL2Block (derivation path)
  - Call with bad params to trigger failure counters

This exercises ALL metrics defined in crates/engine-api/src/metrics.rs
and crates/payload/builder/src/metrics.rs.

Run:
    cd local-test && python3 produce-blocks.py
    cd local-test && python3 produce-blocks.py --blocks 50 --interval 0.5
"""
import argparse, json, time, hmac as hmac_mod, hashlib, base64
import urllib.request, urllib.error, sys, os

# ── engine / http endpoints ───────────────────────────────────────────────────
ENGINE_URL  = os.environ.get("ENGINE_URL", "http://127.0.0.1:8551")
HTTP_URL    = os.environ.get("HTTP_URL",   "http://127.0.0.1:8545")
METRICS_URL = os.environ.get("METRICS_URL","http://127.0.0.1:9001")
JWT_FILE    = os.environ.get("JWT_SECRET", os.path.join(os.path.dirname(__file__), "jwt.hex"))
ACCOUNTS_FILE = os.path.join(os.path.dirname(__file__), "accounts.json")

# ── colours ───────────────────────────────────────────────────────────────────
G="\033[92m"; Y="\033[93m"; C="\033[96m"; R="\033[91m"; N="\033[0m"

# ── JWT ───────────────────────────────────────────────────────────────────────
def _b64url(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()

def make_jwt(secret_hex: str) -> str:
    secret  = bytes.fromhex(secret_hex.strip())
    header  = _b64url(json.dumps({"alg":"HS256","typ":"JWT"}).encode())
    payload = _b64url(json.dumps({"iat": int(time.time())}).encode())
    msg     = f"{header}.{payload}".encode()
    sig     = hmac_mod.new(secret, msg, hashlib.sha256).digest()
    return f"{header}.{payload}.{_b64url(sig)}"

_jwt_secret: str = ""

def J() -> str:
    return make_jwt(_jwt_secret)

# ── RPC ───────────────────────────────────────────────────────────────────────
def rpc(url: str, method: str, params: list, jwt: str | None = None) -> dict:
    body    = json.dumps({"jsonrpc":"2.0","id":1,"method":method,"params":params}).encode()
    headers = {"Content-Type": "application/json"}
    if jwt:
        headers["Authorization"] = f"Bearer {jwt}"
    req = urllib.request.Request(url, data=body, headers=headers)
    try:
        return json.loads(urllib.request.urlopen(req, timeout=10).read())
    except urllib.error.HTTPError as e:
        return {"error": {"code": e.code, "message": e.read().decode()[:200]}}
    except Exception as e:
        return {"error": {"code": -1, "message": str(e)}}

def get_head() -> int:
    r = rpc(HTTP_URL, "eth_blockNumber", [])
    return int(r.get("result", "0x0"), 16)

# ── transaction signing (pure Python, no web3 dep required) ──────────────────
from eth_account import Account
from eth_account.signers.local import LocalAccount

_accounts: list[LocalAccount] = []

def load_accounts() -> list[LocalAccount]:
    with open(ACCOUNTS_FILE) as f:
        data = json.load(f)
    return [Account.from_key(a["key"]) for a in data]

def sign_transfer(sender: LocalAccount, to: str, nonce: int, chain_id: int,
                  value_wei: int = 10**15,
                  gas_price_wei: int = 2_000_000) -> str:
    """Sign a simple ETH transfer. Returns '0x...' raw tx hex."""
    tx = {
        "to":       to,
        "value":    value_wei,
        "gas":      21000,
        "gasPrice": gas_price_wei,
        "nonce":    nonce,
        "chainId":  chain_id,
        "data":     b"",
    }
    signed = sender.sign_transaction(tx)
    return "0x" + signed.raw_transaction.hex()

def send_tx(raw_hex: str) -> str | None:
    """Send raw tx to mempool. Returns txhash or None on failure."""
    r = rpc(HTTP_URL, "eth_sendRawTransaction", [raw_hex])
    return r.get("result")          # None if error

def get_nonce(address: str) -> int:
    r = rpc(HTTP_URL, "eth_getTransactionCount", [address, "latest"])
    return int(r.get("result", "0x0"), 16)

def get_chain_id() -> int:
    r = rpc(HTTP_URL, "eth_chainId", [])
    return int(r.get("result", "0x539"), 16)   # default 1337

# ── block production loop ─────────────────────────────────────────────────────
def wait_for_node(timeout: int = 60):
    print(f"{C}Waiting for node1 metrics endpoint...{N}")
    for i in range(timeout):
        try:
            urllib.request.urlopen(METRICS_URL, timeout=2)
            print(f"  {G}Ready ({i+1}s){N}")
            return
        except:
            time.sleep(1)
    sys.exit(f"{R}node1 not ready after {timeout}s{N}")

def produce_blocks(total: int, interval: float):
    chain_id = get_chain_id()
    print(f"{C}ChainId: {chain_id}{N}")
    n_accounts = len(_accounts)
    # Cache nonces
    nonces = {a.address: get_nonce(a.address) for a in _accounts}
    print(f"{G}Loaded {n_accounts} accounts — nonces: {nonces}{N}")

    block = 0
    bad_params_interval = 5    # trigger failure counters every N blocks
    safe_interval       = 10   # trigger newSafeL2Block every N blocks

    while total == 0 or block < total:
        head = get_head()
        nxt  = head + 1

        # ── 1. submit pool transactions from funded accounts ─────────────────
        tx_count = 0
        for i, sender in enumerate(_accounts):
            # Each account sends to the next account (circular)
            recipient = _accounts[(i + 1) % n_accounts].address
            raw = sign_transfer(
                sender, recipient, nonces[sender.address],
                chain_id,
                value_wei   = (i + 1) * 10**15,     # 0.001–0.004 ETH
                gas_price_wei = 2_000_000,           # 2x baseFee (0.001 Gwei)
            )
            txhash = send_tx(raw)
            if txhash:
                nonces[sender.address] += 1
                tx_count += 1

        # ── 2. assemble block (picks up pool transactions) ───────────────────
        r_asm = rpc(ENGINE_URL, "engine_assembleL2Block",
                    [{"number": hex(nxt), "transactions": []}], J())
        if "error" in r_asm and "result" not in r_asm:
            err = r_asm["error"]
            print(f"  {R}assemble failed: {err}{N}")
            time.sleep(0.5)
            continue

        data = r_asm["result"]
        n_tx = len(data.get("transactions", []))
        gas  = int(data.get("gasUsed", "0x0"), 16)

        # ── 3. validate ───────────────────────────────────────────────────────
        r_val = rpc(ENGINE_URL, "engine_validateL2Block", [data], J())
        val_ok = r_val.get("result", {}).get("success", False) \
                 if "result" in r_val else False

        # ── 4. import (triggers P2P broadcast to node2/node3) ────────────────
        r_new = rpc(ENGINE_URL, "engine_newL2Block", [data], J())
        imported = "result" in r_new and r_new["result"] is None

        # ── log ───────────────────────────────────────────────────────────────
        status = f"{G}✓{N}" if imported else f"{R}✗{N}"
        print(f"  {status} block #{nxt:4d} | {n_tx:2d} txs | "
              f"{gas:7d} gas | validate={'ok' if val_ok else 'fail'} | "
              f"import={'ok' if imported else 'fail'}")

        # ── 5. every safe_interval: newSafeL2Block ───────────────────────────
        if nxt % safe_interval == 0:
            new_head = get_head()
            safe_nxt = new_head + 1
            r_safe = rpc(ENGINE_URL, "engine_newSafeL2Block", [{
                "number":       hex(safe_nxt),
                "gasLimit":     data.get("gasLimit", "0x1c9c380"),
                "baseFeePerGas": data.get("baseFeePerGas"),
                "timestamp":    hex(int(time.time())),
                "transactions": [],
            }], J())
            safe_ok = "result" in r_safe
            print(f"  {G if safe_ok else Y}  └─ newSafeL2Block #{safe_nxt}: "
                  f"{'ok' if safe_ok else r_safe.get('error','?')}{N}")

        # ── 6. every bad_params_interval: trigger failure counters ───────────
        if nxt % bad_params_interval == 0:
            # assemble with wrong number → assemble_failures_total++
            rpc(ENGINE_URL, "engine_assembleL2Block",
                [{"number": "0xfffff", "transactions": []}], J())
            # validate with wrong parent → validate_failures_total++
            cur = get_head()
            rpc(ENGINE_URL, "engine_validateL2Block", [{
                "parentHash":"0x" + "00" * 32,
                "miner":     "0x" + "00" * 20,
                "number":    hex(cur + 1),
                "gasLimit":  "0x1c9c380",
                "timestamp": hex(int(time.time())),
                "transactions": [],
                "stateRoot":  "0x" + "00" * 32,
                "gasUsed":    "0x0",
                "receiptsRoot":"0x" + "00" * 32,
                "logsBloom":  "0x" + "00" * 256,
                "withdrawTrieRoot":"0x" + "00" * 32,
                "nextL1MessageIndex": 0,
                "hash":       "0x" + "00" * 31 + "01",
            }], J())
            # newSafeL2Block with wrong number → newSafe_failures_total++
            rpc(ENGINE_URL, "engine_newSafeL2Block", [{
                "number":       "0xfffff",
                "gasLimit":     "0x1c9c380",
                "timestamp":    hex(int(time.time())),
                "transactions": [],
            }], J())
            print(f"  {Y}  └─ failure counters triggered{N}")

        block += 1
        time.sleep(interval)

# ── check P2P sync on node2/node3 ─────────────────────────────────────────────
def check_sync_status():
    print(f"\n{C}P2P sync status:{N}")
    for i, (port, label) in enumerate([(8545,"node1"), (8546,"node2"), (8547,"node3")]):
        r = rpc(f"http://127.0.0.1:{port}", "eth_blockNumber", [])
        if "result" in r:
            n = int(r["result"], 16)
            print(f"  {label}: block #{n}")
        else:
            print(f"  {Y}{label}: offline{N}")

    # Peer counts
    for i, (port, label) in enumerate([(8545,"node1"), (8546,"node2"), (8547,"node3")]):
        r = rpc(f"http://127.0.0.1:{port}", "net_peerCount", [])
        if "result" in r:
            n = int(r["result"], 16)
            sym = G if n > 0 else Y
            print(f"  {sym}{label}: {n} peer(s){N}")

# ── metrics summary ───────────────────────────────────────────────────────────
def print_metrics():
    print(f"\n{C}Key metrics (node1):{N}")
    try:
        raw = urllib.request.urlopen(METRICS_URL, timeout=5).read().decode()
    except:
        print(f"  {R}metrics endpoint unreachable{N}")
        return

    want = [
        "reth_morph_engine_assemble_l2_block_duration_seconds_count",
        "reth_morph_engine_assemble_l2_block_failures_total",
        "reth_morph_engine_validate_l2_block_duration_seconds_count",
        "reth_morph_engine_validate_l2_block_failures_total",
        "reth_morph_engine_new_l2_block_duration_seconds_count",
        "reth_morph_engine_new_safe_l2_block_duration_seconds_count",
        "reth_morph_engine_head_block_timegap_seconds",
        "reth_morph_payload_builder_payload_build_duration_seconds_count",
        "reth_morph_payload_builder_block_transactions",
        "reth_morph_payload_builder_pool_transactions_skipped_total",
    ]
    for line in raw.split("\n"):
        for w in want:
            if line.startswith(w + " ") or line.startswith(w + "{"):
                print(f"  {line}")

# ── main ──────────────────────────────────────────────────────────────────────
def main():
    global _jwt_secret, _accounts

    parser = argparse.ArgumentParser()
    parser.add_argument("--blocks",   type=int,   default=0,   help="0 = run forever")
    parser.add_argument("--interval", type=float, default=1.0, help="seconds between blocks")
    args = parser.parse_args()

    _jwt_secret = open(JWT_FILE).read().strip()
    _accounts   = load_accounts()

    wait_for_node()
    print(f"\n{G}{'='*55}{N}")
    print(f"{G}  Morph Devnet Block Producer{N}")
    print(f"{G}{'='*55}{N}")
    print(f"  Interval : {args.interval}s")
    print(f"  Blocks   : {'∞' if args.blocks == 0 else args.blocks}")
    print(f"  Accounts : {len(_accounts)}")
    print()

    try:
        produce_blocks(args.blocks, args.interval)
    except KeyboardInterrupt:
        print(f"\n{Y}Stopped.{N}")

    print_metrics()
    check_sync_status()
    print(f"\n{C}Grafana: http://localhost:3000/d/morph-engine-api{N}\n")

if __name__ == "__main__":
    main()
