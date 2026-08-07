#!/usr/bin/env python3
"""Rebuild and verify every runtime used by the node sweep integration tests.

Usage from the ``morph-reth`` root::

    python3 crates/node/tests/assets/verify_sweep_fixtures.py

The test-only router/emitter are rebuilt from this repository. The mock
Registry, production Registry implementation, and OpenZeppelin transparent
proxy are rebuilt from the sibling ``morph/contracts`` checkout. Set
``MORPH_CONTRACTS_ROOT`` when that checkout is elsewhere.

Compiler version, optimizer settings, EVM version, and metadata settings are
pinned in ``SweepFixtures.hashes.json``. Every Forge invocation disables the
cache, so this check compares the Rust constants/assets with a fresh build.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[4]
ASSETS = Path(__file__).resolve().parent
MANIFEST = ASSETS / "SweepFixtures.hashes.json"
RUST_TEST = ROOT / "crates/node/tests/it/sweep.rs"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def morph_contracts_root() -> Path:
    override = os.environ.get("MORPH_CONTRACTS_ROOT")
    if override:
        return Path(override).expanduser().resolve()
    return ROOT.parent / "morph" / "contracts"


def project_roots() -> dict[str, Path]:
    return {"local": ROOT, "morph": morph_contracts_root()}


def embedded_runtime(source: str, constant: str) -> str:
    match = re.search(rf'const {re.escape(constant)}: &str = "(0x[0-9a-f]+)";', source)
    if match is None:
        raise RuntimeError(f"missing Rust runtime constant {constant}")
    return match.group(1)


def runtime_bytes(runtime: str, label: str) -> bytes:
    normalized = runtime.strip()
    if re.fullmatch(r"0x(?:[0-9a-fA-F]{2})+", normalized) is None:
        raise RuntimeError(f"{label} is not non-empty 0x-prefixed bytecode")
    return bytes.fromhex(normalized.removeprefix("0x"))


def pinned_environment(profile: dict[str, Any]) -> dict[str, str]:
    # Inherited Foundry/Dapp variables can silently change optimizer details,
    # profiles, remappings, or metadata. Preserve ordinary process state (PATH,
    # compiler caches, etc.) but rebuild the toolchain environment from a known
    # profile below.
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("FOUNDRY_", "DAPP_"))
    }
    environment.update(
        {
            "FOUNDRY_PROFILE": "default",
            "FOUNDRY_OPTIMIZER": "true",
            "FOUNDRY_OPTIMIZER_RUNS": str(profile["optimizer_runs"]),
            "FOUNDRY_EVM_VERSION": profile["evm_version"],
            "FOUNDRY_VIA_IR": str(profile["via_ir"]).lower(),
            "FOUNDRY_REVERT_STRINGS": profile["revert_strings"],
        }
    )
    if "bytecode_hash" in profile:
        environment["FOUNDRY_BYTECODE_HASH"] = profile["bytecode_hash"]
    if "cbor_metadata" in profile:
        environment["FOUNDRY_CBOR_METADATA"] = str(profile["cbor_metadata"]).lower()
    return environment


def compiled_runtime(
    root: Path,
    profile: dict[str, Any],
    contract: str,
    temporary_root: Path,
) -> str:
    safe_name = re.sub(r"[^a-zA-Z0-9]+", "-", contract).strip("-")
    command = [
        "forge",
        "inspect",
        contract,
        "deployedBytecode",
        "--root",
        str(root),
        "--contracts",
        profile["contracts"],
        "--use",
        profile["compiler"],
        "--no-auto-detect",
        "--evm-version",
        profile["evm_version"],
        "--optimize",
        "--optimizer-runs",
        str(profile["optimizer_runs"]),
        "--no-cache",
        "--out",
        str(temporary_root / f"out-{safe_name}"),
        "--cache-path",
        str(temporary_root / f"cache-{safe_name}"),
    ]
    if profile.get("metadata") is False:
        command.append("--no-metadata")
    return subprocess.check_output(
        command,
        cwd=root,
        env=pinned_environment(profile),
        text=True,
    ).strip()


def stored_runtime(fixture: dict[str, Any], rust_source: str) -> str:
    locations = [key for key in ("rust_constant", "asset") if key in fixture]
    if len(locations) != 1:
        raise RuntimeError("each runtime must declare exactly one rust_constant or asset")
    if "rust_constant" in fixture:
        return embedded_runtime(rust_source, fixture["rust_constant"])
    return (ASSETS / fixture["asset"]).read_text().strip()


def main() -> int:
    if shutil.which("forge") is None:
        print("forge is required for this developer check", file=sys.stderr)
        return 2

    manifest = json.loads(MANIFEST.read_text())
    profiles = manifest["profiles"]
    roots = project_roots()
    for name, root in roots.items():
        if not root.is_dir():
            raise RuntimeError(f"missing {name} project root: {root}")

    sources = manifest["sources"]
    for name, source in sources.items():
        source_path = roots[source["profile"]] / source["path"]
        actual_hash = sha256(source_path.read_bytes())
        if actual_hash != source["sha256"]:
            raise RuntimeError(
                f"{name} source hash mismatch: {actual_hash} != {source['sha256']}"
            )
        print(f"verified source {name} ({actual_hash})")

    rust_source = RUST_TEST.read_text()
    with tempfile.TemporaryDirectory(prefix="morph-sweep-fixtures-") as temporary:
        temporary_root = Path(temporary)
        for name, fixture in manifest["runtimes"].items():
            profile_name = fixture["profile"]
            source = sources[fixture["source"]]
            if source["profile"] != profile_name:
                raise RuntimeError(f"{name} source and compiler profiles differ")

            stored = stored_runtime(fixture, rust_source)
            stored_hash = sha256(runtime_bytes(stored, name))
            if stored_hash != fixture["sha256"]:
                raise RuntimeError(
                    f"{name} hash mismatch: {stored_hash} != {fixture['sha256']}"
                )

            compiled = compiled_runtime(
                roots[profile_name],
                profiles[profile_name],
                fixture["contract"],
                temporary_root,
            )
            if runtime_bytes(compiled, f"compiled {name}") != runtime_bytes(stored, name):
                raise RuntimeError(f"{name} differs from freshly compiled deployedBytecode")

            print(f"verified runtime {name} ({stored_hash})")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
