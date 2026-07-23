#!/usr/bin/env python3
"""Verify embedded sweep runtimes against their Solidity source.

This is a developer-only reproducibility check; Rust CI does not need Forge:

    python3 crates/node/tests/assets/verify_sweep_fixtures.py
"""

from __future__ import annotations

import hashlib
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[4]
ASSETS = Path(__file__).resolve().parent
SOURCE = ASSETS / "SweepFixtures.sol"
MANIFEST = ASSETS / "SweepFixtures.hashes.json"
RUST_TEST = ROOT / "crates/node/tests/it/sweep.rs"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def embedded_runtime(source: str, constant: str) -> str:
    match = re.search(rf'const {constant}: &str = "(0x[0-9a-f]+)";', source)
    if match is None:
        raise RuntimeError(f"missing Rust runtime constant {constant}")
    return match.group(1)


def compiled_runtime(contract: str, compiler: str) -> str:
    command = [
        "forge",
        "inspect",
        f"crates/node/tests/assets/SweepFixtures.sol:{contract}",
        "deployedBytecode",
        "--root",
        str(ROOT),
        "--contracts",
        "crates/node/tests/assets",
        "--use",
        compiler,
        "--optimize",
        "--optimizer-runs",
        "200",
        "--no-metadata",
        "--no-cache",
        "--out",
        "/tmp/morph-reth-sweep-out",
        "--cache-path",
        "/tmp/morph-reth-sweep-cache",
    ]
    return subprocess.check_output(command, cwd=ROOT, text=True).strip()


def main() -> int:
    if shutil.which("forge") is None:
        print("forge is required for this developer check", file=sys.stderr)
        return 2

    manifest = json.loads(MANIFEST.read_text())
    actual_source_hash = sha256(SOURCE.read_bytes())
    if actual_source_hash != manifest["source_sha256"]:
        raise RuntimeError(
            f"source hash mismatch: {actual_source_hash} != {manifest['source_sha256']}"
        )

    rust_source = RUST_TEST.read_text()
    for constant, fixture in manifest["runtimes"].items():
        embedded = embedded_runtime(rust_source, constant)
        embedded_hash = sha256(bytes.fromhex(embedded.removeprefix("0x")))
        if embedded_hash != fixture["sha256"]:
            raise RuntimeError(
                f"{constant} hash mismatch: {embedded_hash} != {fixture['sha256']}"
            )

        compiled = compiled_runtime(fixture["contract"], manifest["compiler"])
        if compiled != embedded:
            raise RuntimeError(f"{constant} differs from forge deployedBytecode")

        print(f"verified {constant} ({embedded_hash})")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
