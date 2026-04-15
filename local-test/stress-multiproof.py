#!/usr/bin/env python3
"""
stress-multiproof.py — high-state-change stress driver.

Unlike produce-blocks.py (4 accounts cycling), this script fans out transfers
to hundreds of fresh random addresses per block. Touching many unique
accounts per block is the workload reth needs to spawn the parallel
multiproof task (vs the sparse-trie fast path), which populates:

  reth_tree_root_multiproof_task_total_duration_histogram
  reth_tree_root_proof_calculation_duration_histogram
  reth_tree_root_active_{account,storage}_workers_histogram
  reth_tree_root_pending_{account,storage}_multiproofs_histogram

Usage:
  cd local-test
  NO_PROXY=127.0.0.1,localhost python3 stress-multiproof.py \
      --txs-per-block 500 --blocks 20

Env overrides:
  ENGINE_URL   (default http://127.0.0.1:8551)
  HTTP_URL     (default http://127.0.0.1:8545)
  METRICS_URL  (default http://127.0.0.1:9001)
  JWT_SECRET   (default ./jwt.hex)
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import json
import os
import secrets
import sys
import time
import urllib.error
import urllib.request

from eth_account import Account
from eth_account.signers.local import LocalAccount
from eth_utils import to_checksum_address

os.environ.setdefault("NO_PROXY", "127.0.0.1,localhost")
os.environ.setdefault("no_proxy", "127.0.0.1,localhost")

ENGINE_URL = os.environ.get("ENGINE_URL", "http://127.0.0.1:8551")
HTTP_URL = os.environ.get("HTTP_URL", "http://127.0.0.1:8545")
METRICS_URL = os.environ.get("METRICS_URL", "http://127.0.0.1:9001")
JWT_FILE = os.environ.get("JWT_SECRET", os.path.join(os.path.dirname(__file__), "jwt.hex"))
ACCOUNTS_FILE = os.path.join(os.path.dirname(__file__), "accounts.json")

# Full node engine endpoints — each node has its own JWT secret auto-generated in datadir.
# We mirror each sequencer-built block to the full nodes via engine_newL2Block so that
# reth's engine tree on the full nodes spawns the StateRootTask (which is what populates
# the reth_tree_root_multiproof_task_* / _proof_calculation_* / _active_*_workers_*
# metric series). This simulates what morph-node would do in a full deployment.
FULL_NODES = [
    {"name": "full0", "url": "http://127.0.0.1:8552", "jwt": "/tmp/morph-devnet/node2/jwt.hex", "metrics": "http://127.0.0.1:9002"},
    # full1 skipped — node3 failed to start because of /tmp/reth.ipc port conflict in start-devnet.sh
]

G = "\033[92m"
Y = "\033[93m"
C = "\033[96m"
R = "\033[91m"
N = "\033[0m"


def _b64url(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def make_jwt(secret_hex: str) -> str:
    secret = bytes.fromhex(secret_hex.strip())
    header = _b64url(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    payload = _b64url(json.dumps({"iat": int(time.time())}).encode())
    msg = f"{header}.{payload}".encode()
    sig = hmac.new(secret, msg, hashlib.sha256).digest()
    return f"{header}.{payload}.{_b64url(sig)}"


def rpc(url: str, method: str, params: list, jwt: str | None = None) -> dict:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    headers = {"Content-Type": "application/json"}
    if jwt:
        headers["Authorization"] = f"Bearer {jwt}"
    req = urllib.request.Request(url, data=body, headers=headers)
    try:
        return json.loads(urllib.request.urlopen(req, timeout=30).read())
    except urllib.error.HTTPError as e:
        return {"error": {"code": e.code, "message": e.read().decode()[:200]}}
    except Exception as e:  # noqa: BLE001
        return {"error": {"code": -1, "message": str(e)}}


def get_head() -> int:
    r = rpc(HTTP_URL, "eth_blockNumber", [])
    return int(r.get("result", "0x0"), 16)


def get_chain_id() -> int:
    r = rpc(HTTP_URL, "eth_chainId", [])
    return int(r.get("result", "0x539"), 16)


def get_nonce(address: str) -> int:
    r = rpc(HTTP_URL, "eth_getTransactionCount", [address, "latest"])
    return int(r.get("result", "0x0"), 16)


def random_address() -> str:
    return to_checksum_address("0x" + secrets.token_bytes(20).hex())


def sign_transfer(
    sender: LocalAccount,
    to: str,
    nonce: int,
    chain_id: int,
    value_wei: int = 10**13,
    gas_price_wei: int = 2_000_000,
) -> str:
    tx = {
        "to": to,
        "value": value_wei,
        "gas": 21000,
        "gasPrice": gas_price_wei,
        "nonce": nonce,
        "chainId": chain_id,
        "data": b"",
    }
    signed = sender.sign_transaction(tx)
    return "0x" + signed.raw_transaction.hex()


def metrics_snapshot(keys: list[str], url: str = METRICS_URL) -> dict[str, int]:
    try:
        raw = urllib.request.urlopen(url, timeout=5).read().decode()
    except Exception as e:  # noqa: BLE001
        print(f"  {R}metrics unreachable {url}: {e}{N}")
        return {}
    out = {}
    for line in raw.splitlines():
        if line.startswith("#") or not line.strip():
            continue
        for k in keys:
            if line.startswith(k + " "):
                try:
                    out[k] = int(float(line.split()[-1]))
                except ValueError:
                    pass
    return out


WATCH_KEYS = [
    "reth_tree_root_multiproof_task_total_duration_histogram_count",
    "reth_tree_root_proof_calculation_duration_histogram_count",
    "reth_tree_root_active_account_workers_histogram_count",
    "reth_tree_root_active_storage_workers_histogram_count",
    "reth_tree_root_pending_account_multiproofs_histogram_count",
    "reth_tree_root_pending_storage_multiproofs_histogram_count",
    "reth_tree_root_sparse_trie_total_duration_histogram_count",
    "reth_tree_root_proofs_processed_histogram_count",
]


def print_snapshot(label: str, snap: dict[str, int]) -> None:
    print(f"{C}  {label} metric snapshot:{N}")
    for k in WATCH_KEYS:
        v = snap.get(k, 0)
        short = k.replace("reth_tree_root_", "").replace("_histogram_count", "")
        color = G if v > 0 else Y
        print(f"    {color}{v:6d}{N} {short}")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--txs-per-block", type=int, default=500)
    ap.add_argument("--blocks", type=int, default=20)
    ap.add_argument("--interval", type=float, default=0.0)
    ap.add_argument("--mirror-to-fulls", action="store_true",
                    help="Also push each built block to full0/full1 to trigger their StateRootTask")
    args = ap.parse_args()

    jwt_secret = open(JWT_FILE).read().strip()
    def J() -> str:
        return make_jwt(jwt_secret)

    # Preload full node JWTs for mirroring
    full_jwts: dict[str, str] = {}
    if args.mirror_to_fulls:
        for f in FULL_NODES:
            try:
                full_jwts[f["name"]] = open(f["jwt"]).read().strip()
                print(f"  {G}✓ {f['name']} jwt loaded from {f['jwt']}{N}")
            except Exception as e:  # noqa: BLE001
                print(f"  {R}✗ {f['name']} jwt failed: {e}{N}")

    accounts = [Account.from_key(a["key"]) for a in json.load(open(ACCOUNTS_FILE))]
    sender = accounts[0]
    print(f"{C}Stress multiproof driver{N}")
    print(f"  sender        : {sender.address}")
    print(f"  target        : {args.txs_per_block} txs/block × {args.blocks} blocks")
    print(f"  mirror to fulls: {args.mirror_to_fulls}")

    chain_id = get_chain_id()
    nonce = get_nonce(sender.address)
    print(f"  chainId : {chain_id}")
    print(f"  nonce0  : {nonce}")

    print(f"\n{C}== initial metrics snapshot =={N}")
    print_snapshot("sequencer", metrics_snapshot(WATCH_KEYS, METRICS_URL))
    if args.mirror_to_fulls:
        for f in FULL_NODES:
            print_snapshot(f["name"], metrics_snapshot(WATCH_KEYS, f["metrics"]))

    for b in range(args.blocks):
        # ── 1. submit N transfers to random addresses ───────────────────────
        t0 = time.time()
        sent = 0
        for _ in range(args.txs_per_block):
            raw = sign_transfer(sender, random_address(), nonce, chain_id)
            r = rpc(HTTP_URL, "eth_sendRawTransaction", [raw])
            if "result" in r:
                sent += 1
                nonce += 1
            else:
                # Pool full or other error — stop filling this batch
                err = r.get("error", {}).get("message", "?")
                print(f"  {Y}sendRawTx error after {sent} txs: {err[:80]}{N}")
                break
        submit_elapsed = time.time() - t0

        # ── 2. ask sequencer to build a block from the pool ─────────────────
        head = get_head()
        t1 = time.time()
        r_asm = rpc(
            ENGINE_URL,
            "engine_assembleL2Block",
            [{"number": hex(head + 1), "transactions": []}],
            J(),
        )
        asm_elapsed = time.time() - t1
        if "error" in r_asm:
            print(f"  {R}assemble failed: {r_asm['error']}{N}")
            break
        data = r_asm["result"]
        n_tx = len(data.get("transactions", []))
        gas = int(data.get("gasUsed", "0x0"), 16)

        # ── 3. import via newL2Block so head advances on sequencer ──────────
        t2 = time.time()
        r_new = rpc(ENGINE_URL, "engine_newL2Block", [data], J())
        new_elapsed = time.time() - t2
        imported = "result" in r_new and r_new["result"] is None

        # ── 4. mirror to full nodes to trigger their engine-tree StateRootTask ─
        full_timings = []
        if args.mirror_to_fulls and imported:
            for f in FULL_NODES:
                jwt_secret_f = full_jwts.get(f["name"], "")
                token = make_jwt(jwt_secret_f) if jwt_secret_f else None
                t3 = time.time()
                r_push = rpc(f["url"], "engine_newL2Block", [data], token)
                mirror_elapsed = time.time() - t3
                ok = "result" in r_push and r_push["result"] is None
                full_timings.append((f["name"], ok, mirror_elapsed))

        status = G + "✓" + N if imported else R + "✗" + N
        base = (
            f"{status} block #{head+1:4d}  sent={sent:4d}  included={n_tx:4d}  "
            f"gas={gas:9d}  submit={submit_elapsed*1000:6.0f}ms  "
            f"asm={asm_elapsed*1000:5.0f}ms  new={new_elapsed*1000:5.0f}ms"
        )
        if full_timings:
            mirrors = "  ".join(
                f"{name}={'ok' if ok else 'FAIL'}({ms*1000:.0f}ms)"
                for name, ok, ms in full_timings
            )
            base += f"  | mirror: {mirrors}"
        print(base)

        if args.interval > 0:
            time.sleep(args.interval)

    # ── Final metrics snapshot ─────────────────────────────────────────────
    print(f"\n{C}== final metrics snapshot =={N}")
    seq_final = metrics_snapshot(WATCH_KEYS, METRICS_URL)
    print_snapshot("sequencer", seq_final)
    full_finals = {}
    if args.mirror_to_fulls:
        for f in FULL_NODES:
            snap = metrics_snapshot(WATCH_KEYS, f["metrics"])
            full_finals[f["name"]] = snap
            print_snapshot(f["name"], snap)

    # Summary
    total_mp = seq_final.get("reth_tree_root_multiproof_task_total_duration_histogram_count", 0)
    for snap in full_finals.values():
        total_mp += snap.get("reth_tree_root_multiproof_task_total_duration_histogram_count", 0)
    if total_mp > 0:
        print(f"\n{G}SUCCESS: multiproof task fired {total_mp} times across all nodes{N}")
    else:
        print(f"\n{Y}multiproof task still at 0 across all nodes{N}")


if __name__ == "__main__":
    main()
