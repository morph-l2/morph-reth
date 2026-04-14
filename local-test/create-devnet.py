#!/usr/bin/env python3
"""
Generate devnet.json — a custom genesis for local multi-node testing.

Inherits all 85 Morph system contracts from hoodi.json (bytecode + storage)
and adds pre-funded test accounts so we can sign real transactions.

Chain ID: 1337  (local devnet, never conflicts with mainnet/hoodi)
All hardforks activated at timestamp 0.
"""
import json, time, os, sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC  = os.path.join(REPO, "crates/chainspec/res/genesis/hoodi.json")
OUT  = os.path.join(os.path.dirname(__file__), "devnet.json")

# ── well-known Hardhat/Foundry test accounts (private keys public) ───────────
TEST_ACCOUNTS = [
    # (address, private_key) — 10_000 ETH each
    ("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
     "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"),
    ("70997970C51812dc3A010C7d01b50e0d17dc79C8",
     "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"),
    ("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
     "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a"),
    ("90F79bf6EB2c4f870365E785982E1f101E93b906",
     "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6"),
]
BALANCE = hex(10_000 * 10**18)   # 10 000 ETH

with open(SRC) as f:
    hoodi = json.load(f)

# ── config: keep all morph/scroll settings, override IDs and hardfork times ──
cfg = hoodi["config"].copy()
cfg["chainId"] = 1337
# activate every hardfork at timestamp 0 so genesis already has all features
cfg["morph203Time"]  = 0
cfg["viridianTime"]  = 0
cfg["emeraldTime"]   = 0
cfg["jadeForkTime"]  = 0

# ── alloc: system contracts from hoodi + funded test accounts ────────────────
alloc = dict(hoodi["alloc"])   # shallow copy — all 85 system contracts
for addr, _ in TEST_ACCOUNTS:
    alloc[addr.lower()] = {"balance": BALANCE}

genesis = {
    "config":       cfg,
    "nonce":        "0x0",
    "timestamp":    hex(int(time.time())),   # genesis = now → head_timegap stays small
    "extraData":    "0x",
    "gasLimit":     "0x1c9c380",             # 30 M gas (same as hoodi)
    "difficulty":   "0x0",
    "mixHash":      "0x" + "00" * 32,
    "coinbase":     "0x" + "00" * 20,
    "number":       "0x0",
    "gasUsed":      "0x0",
    "parentHash":   "0x" + "00" * 32,
    "baseFeePerGas": "0xf4240",              # 1 000 000 wei = 0.001 Gwei (low base fee)
    "alloc":        alloc,
}

with open(OUT, "w") as f:
    json.dump(genesis, f, indent=2)

print(f"devnet.json written → {OUT}")
print(f"  chainId   : 1337")
print(f"  baseFee   : 1 000 000 wei (0.001 Gwei)")
print(f"  accounts  : {len(TEST_ACCOUNTS)} test accounts × 10 000 ETH")
print(f"  contracts : {sum(1 for v in alloc.values() if v.get('code'))} system contracts")
print()
print("Test accounts:")
for addr, pk in TEST_ACCOUNTS:
    print(f"  0x{addr}  (pk: 0x{pk[:16]}...)")

# Also write accounts.json for the block-producer script
accounts_out = os.path.join(os.path.dirname(__file__), "accounts.json")
with open(accounts_out, "w") as f:
    json.dump([{"address": f"0x{a}", "key": f"0x{k}"} for a, k in TEST_ACCOUNTS], f, indent=2)
print(f"\naccounts.json written → {accounts_out}")
