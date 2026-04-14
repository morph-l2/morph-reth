#!/usr/bin/env python3
"""
Trigger all morph-reth custom Prometheus metrics via engine API calls.

Covers:
  morph.engine:
    assemble_l2_block_duration_seconds, assemble_l2_block_failures_total
    validate_l2_block_duration_seconds, validate_l2_block_failures_total
    new_l2_block_duration_seconds, new_l2_block_failures_total
    new_safe_l2_block_duration_seconds, new_safe_l2_block_failures_total
    head_block_timegap_seconds

  morph.payload_builder:
    commit_txs_all_duration_seconds, commit_tx_apply_duration_seconds
    payload_build_duration_seconds, block_transactions
    l1_tx_gas_limit_exceeded_total, l1_tx_strange_err_total
    pool_transactions_skipped_total (reason label)
"""

import json
import struct
import time
import hmac
import hashlib
import base64
import urllib.request
import urllib.error
import os
import sys

# ── config ──────────────────────────────────────────────────────────────────
ENGINE_URL = os.environ.get("ENGINE_URL", "http://127.0.0.1:8551")
HTTP_URL   = os.environ.get("HTTP_URL",   "http://127.0.0.1:8545")
METRICS_URL= os.environ.get("METRICS_URL","http://127.0.0.1:9001")
JWT_SECRET_FILE = os.environ.get("JWT_SECRET", "jwt.hex")

# Colours
GREEN  = "\033[92m"
RED    = "\033[91m"
YELLOW = "\033[93m"
CYAN   = "\033[96m"
RESET  = "\033[0m"

# ── helpers ──────────────────────────────────────────────────────────────────
def _b64url(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()

def make_jwt(secret_hex: str) -> str:
    """HS256 JWT with {iat: now} — Ethereum engine API standard."""
    secret = bytes.fromhex(secret_hex.strip())
    header  = _b64url(json.dumps({"alg":"HS256","typ":"JWT"}).encode())
    payload = _b64url(json.dumps({"iat": int(time.time())}).encode())
    msg     = f"{header}.{payload}".encode()
    sig     = hmac.new(secret, msg, hashlib.sha256).digest()
    return f"{header}.{payload}.{_b64url(sig)}"

def load_jwt_secret() -> str:
    if os.path.exists(JWT_SECRET_FILE):
        return open(JWT_SECRET_FILE).read().strip()
    raise FileNotFoundError(f"JWT secret not found: {JWT_SECRET_FILE}")

def rpc(url: str, method: str, params: list, jwt: str | None = None) -> dict:
    body = json.dumps({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }).encode()
    headers = {"Content-Type": "application/json"}
    if jwt:
        headers["Authorization"] = f"Bearer {jwt}"
    req = urllib.request.Request(url, data=body, headers=headers)
    try:
        resp = urllib.request.urlopen(req, timeout=10)
        return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        return {"error": {"code": e.code, "message": e.read().decode()[:200]}}
    except Exception as e:
        return {"error": {"code": -1, "message": str(e)}}

def show(label: str, result: dict):
    if "error" in result and "result" not in result:
        print(f"  {RED}✗ {label}: {result['error']}{RESET}")
    else:
        print(f"  {GREEN}✓ {label}: ok{RESET}")
    return result

def wait_for_node(max_wait=60):
    """Poll the metrics endpoint until the node is ready."""
    print(f"{CYAN}Waiting for morph-reth to start...{RESET}")
    for i in range(max_wait):
        try:
            urllib.request.urlopen(METRICS_URL, timeout=2)
            print(f"  {GREEN}Node ready ({i+1}s){RESET}")
            return True
        except:
            time.sleep(1)
    print(f"  {RED}Node did not start within {max_wait}s{RESET}")
    return False

def check_metrics(*names: str) -> dict[str, bool]:
    """Fetch /metrics and check which metric names appear."""
    try:
        raw = urllib.request.urlopen(METRICS_URL, timeout=5).read().decode()
    except Exception as e:
        print(f"  {RED}Cannot reach metrics endpoint: {e}{RESET}")
        return {n: False for n in names}
    found = {}
    for name in names:
        found[name] = name in raw
    return found

# ── main ─────────────────────────────────────────────────────────────────────
def main():
    if not wait_for_node():
        sys.exit(1)

    jwt_secret = load_jwt_secret()
    def J() -> str:
        return make_jwt(jwt_secret)

    # ── Get current head block number via eth_blockNumber ──────────────────
    print(f"\n{CYAN}[1] Getting current chain head{RESET}")
    r = rpc(HTTP_URL, "eth_blockNumber", [])
    if "result" in r:
        head_num = int(r["result"], 16)
        print(f"  Current head: block #{head_num}")
    else:
        print(f"  {YELLOW}Warning: can't get head via HTTP ({r.get('error')}), assuming block 0{RESET}")
        head_num = 0

    next_block = head_num + 1

    # ── engine_assembleL2Block: success path ───────────────────────────────
    print(f"\n{CYAN}[2] engine_assembleL2Block — empty block #{next_block} (expects success){RESET}")
    assemble_params = {
        "number": hex(next_block),
        "transactions": [],
    }
    r_assemble = show(
        f"assembleL2Block#{next_block}",
        rpc(ENGINE_URL, "engine_assembleL2Block", [assemble_params], J())
    )

    # ── engine_assembleL2Block: failure path (wrong block number) ──────────
    print(f"\n{CYAN}[3] engine_assembleL2Block — wrong number (expects failure){RESET}")
    show(
        "assembleL2Block#9999 (wrong)",
        rpc(ENGINE_URL, "engine_assembleL2Block", [{"number": "0x270f", "transactions": []}], J())
    )

    # ── engine_validateL2Block: early-failure (wrong parent hash) ─────────
    print(f"\n{CYAN}[4] engine_validateL2Block — zero parent hash (expects early failure){RESET}")
    wrong_block = {
        "parentHash":          "0x0000000000000000000000000000000000000000000000000000000000000000",
        "miner":               "0x0000000000000000000000000000000000000000",
        "number":              hex(next_block),
        "gasLimit":            "0x5f5e100",
        "baseFeePerGas":       "0x1",
        "timestamp":           hex(int(time.time())),
        "transactions":        [],
        "stateRoot":           "0x0000000000000000000000000000000000000000000000000000000000000000",
        "gasUsed":             "0x0",
        "receiptsRoot":        "0x0000000000000000000000000000000000000000000000000000000000000000",
        "logsBloom":           "0x" + "00" * 256,
        "withdrawTrieRoot":    "0x0000000000000000000000000000000000000000000000000000000000000000",
        "nextL1MessageIndex":  0,
        "hash":                "0x0000000000000000000000000000000000000000000000000000000000000001",
    }
    show(
        "validateL2Block (wrong parent)",
        rpc(ENGINE_URL, "engine_validateL2Block", [wrong_block], J())
    )

    # ── engine_newL2Block: failure path (wrong block number) ──────────────
    print(f"\n{CYAN}[5] engine_newL2Block — wrong number (expects failure){RESET}")
    show(
        "newL2Block#9999 (wrong)",
        rpc(ENGINE_URL, "engine_newL2Block", [{**wrong_block, "number": "0x270f"}], J())
    )

    # ── engine_newSafeL2Block: failure path (wrong number) ────────────────
    print(f"\n{CYAN}[6] engine_newSafeL2Block — wrong number (expects failure){RESET}")
    show(
        "newSafeL2Block#9999 (wrong)",
        rpc(ENGINE_URL, "engine_newSafeL2Block", [{
            "number":       "0x270f",
            "gasLimit":     "0x5f5e100",
            "timestamp":    hex(int(time.time())),
            "transactions": [],
        }], J())
    )

    # ── Use assembleL2Block result for success paths ───────────────────────
    if "result" in r_assemble:
        assembled = r_assemble["result"]
        print(f"\n{CYAN}[7] engine_validateL2Block — assembled block (expects success or INVALID){RESET}")
        show(
            "validateL2Block (assembled)",
            rpc(ENGINE_URL, "engine_validateL2Block", [assembled], J())
        )

        print(f"\n{CYAN}[8] engine_newL2Block — assembled block (expects success or INVALID){RESET}")
        r_new = show(
            "newL2Block (assembled)",
            rpc(ENGINE_URL, "engine_newL2Block", [assembled], J())
        )

        # If new_l2_block succeeded, the head advanced; try new_safe_l2_block for block+1
        if "result" in r_new and r_new.get("result") is None:  # result: null = OK(())
            new_next = next_block + 1
            print(f"\n{CYAN}[9] engine_newSafeL2Block — block #{new_next} after successful newL2Block{RESET}")
            show(
                f"newSafeL2Block#{new_next}",
                rpc(ENGINE_URL, "engine_newSafeL2Block", [{
                    "number":       hex(new_next),
                    "gasLimit":     assembled.get("gasLimit", "0x5f5e100"),
                    "baseFeePerGas": assembled.get("baseFeePerGas"),
                    "timestamp":    hex(int(time.time())),
                    "transactions": [],
                }], J())
            )
    else:
        print(f"\n  {YELLOW}assemble failed — skipping success-path tests{RESET}")

        # Still try new_safe_l2_block success path directly
        print(f"\n{CYAN}[9] engine_newSafeL2Block — block #{next_block} direct{RESET}")
        show(
            f"newSafeL2Block#{next_block}",
            rpc(ENGINE_URL, "engine_newSafeL2Block", [{
                "number":       hex(next_block),
                "gasLimit":     "0x5f5e100",
                "timestamp":    hex(int(time.time())),
                "transactions": [],
            }], J())
        )

    # ── Repeat calls to accumulate histogram observations ─────────────────
    print(f"\n{CYAN}[10] Repeating calls to accumulate histogram data...{RESET}")
    for i in range(5):
        rpc(ENGINE_URL, "engine_assembleL2Block",
            [{"number": hex(9999 + i), "transactions": []}], J())
        time.sleep(0.2)
    print(f"  Done (5 extra assemble calls)")

    # ── Check which metrics are present ───────────────────────────────────
    print(f"\n{CYAN}[11] Checking metrics endpoint...{RESET}")
    ALL_METRICS = [
        # Engine API
        "reth_morph_engine_assemble_l2_block_duration_seconds",
        "reth_morph_engine_assemble_l2_block_failures_total",
        "reth_morph_engine_validate_l2_block_duration_seconds",
        "reth_morph_engine_validate_l2_block_failures_total",
        "reth_morph_engine_new_l2_block_duration_seconds",
        "reth_morph_engine_new_l2_block_failures_total",
        "reth_morph_engine_new_safe_l2_block_duration_seconds",
        "reth_morph_engine_new_safe_l2_block_failures_total",
        "reth_morph_engine_head_block_timegap_seconds",
        # Payload builder
        "reth_morph_payload_builder_commit_txs_all_duration_seconds",
        "reth_morph_payload_builder_commit_tx_apply_duration_seconds",
        "reth_morph_payload_builder_payload_build_duration_seconds",
        "reth_morph_payload_builder_block_transactions",
        "reth_morph_payload_builder_l1_tx_gas_limit_exceeded_total",
        "reth_morph_payload_builder_l1_tx_strange_err_total",
        "reth_morph_payload_builder_pool_transactions_skipped_total",
    ]

    found = check_metrics(*ALL_METRICS)
    ok = sum(found.values())
    total = len(found)

    for name, present in found.items():
        icon = f"{GREEN}✓{RESET}" if present else f"{RED}✗{RESET}"
        print(f"  {icon} {name}")

    print(f"\n  {GREEN if ok == total else YELLOW}Result: {ok}/{total} metrics present{RESET}")
    print(f"\n{CYAN}Open Grafana: http://localhost:3000{RESET}")
    print(f"{CYAN}Dashboard: Dashboards → morph-engine{RESET}\n")

    # ── Note about payload builder metrics ──────────────────────────────────
    missing_payload = [n for n, v in found.items() if not v and "payload_builder" in n]
    if missing_payload:
        print(f"{YELLOW}Note: payload_builder metrics appear after a successful assemble_l2_block.{RESET}")
        print(f"{YELLOW}They are initialized lazily; if assemble failed, re-run once the node is synced.{RESET}\n")

if __name__ == "__main__":
    main()
