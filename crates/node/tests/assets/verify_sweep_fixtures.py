#!/usr/bin/env python3
"""Verify embedded sweep runtimes against their Solidity source.

This is a developer-only reproducibility check; Rust CI does not need Forge:

    python3 crates/node/tests/assets/verify_sweep_fixtures.py
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[4]
ASSETS = Path(__file__).resolve().parent
SOURCE = ASSETS / "SweepFixtures.sol"
MANIFEST = ASSETS / "SweepFixtures.hashes.json"
RUST_TEST = ROOT / "crates/node/tests/it/sweep.rs"
PROD_DELEGATE_RUNTIME = ASSETS / "SweepDeposit.deployed.hex"
PROD_REGISTRY_RUNTIME = ASSETS / "SweepRegistry.deployed.hex"
DEFAULT_MORPH_CONTRACTS = ROOT.parent / "morph" / "contracts"


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


def command_output(command: list[str], cwd: Path) -> str:
    result = subprocess.run(command, cwd=cwd, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(
            f"command failed ({' '.join(command)}):\n{result.stdout}{result.stderr}"
        )
    return result.stdout.strip()


def free_local_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait_for_anvil(rpc_url: str, process: subprocess.Popen[bytes]) -> None:
    for _ in range(100):
        if process.poll() is not None:
            raise RuntimeError("anvil exited before accepting requests")
        result = subprocess.run(
            ["cast", "chain-id", "--rpc-url", rpc_url],
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            return
        time.sleep(0.1)
    raise RuntimeError("timed out waiting for anvil")


def verify_production_runtimes(manifest: dict[str, object]) -> None:
    production = manifest["production"]
    assert isinstance(production, dict)
    morph_contracts = Path(
        os.environ.get("MORPH_CONTRACTS_DIR", DEFAULT_MORPH_CONTRACTS)
    ).resolve()
    if not (morph_contracts / "foundry.toml").is_file():
        raise RuntimeError(
            "morph contracts checkout not found; set MORPH_CONTRACTS_DIR "
            f"(looked in {morph_contracts})"
        )

    sources = production["sources"]
    assert isinstance(sources, dict)
    for relative_path, expected_hash in sources.items():
        source = morph_contracts / str(relative_path)
        actual_hash = sha256(source.read_bytes())
        if actual_hash != expected_hash:
            raise RuntimeError(
                f"production source hash mismatch for {source}: "
                f"{actual_hash} != {expected_hash}"
            )

    deployment = production["deployment"]
    assert isinstance(deployment, dict)
    runtime_specs = production["runtimes"]
    assert isinstance(runtime_specs, dict)
    required_tools = ("anvil", "cast", "forge")
    missing = [tool for tool in required_tools if shutil.which(tool) is None]
    if missing:
        raise RuntimeError(f"production fixture verification requires: {', '.join(missing)}")

    port = free_local_port()
    rpc_url = f"http://127.0.0.1:{port}"
    anvil = subprocess.Popen(
        [
            "anvil",
            "--silent",
            "--host",
            "127.0.0.1",
            "--port",
            str(port),
            "--chain-id",
            str(deployment["chain_id"]),
            "--mnemonic",
            "test test test test test test test test test test test junk",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        wait_for_anvil(rpc_url, anvil)
        with tempfile.TemporaryDirectory(prefix="morph-sweep-fixtures-") as temp:
            temporary = Path(temp)
            shared = [
                "--rpc-url",
                rpc_url,
                "--private-key",
                str(deployment["deployer_private_key"]),
                "--broadcast",
                "--json",
                "--out",
                str(temporary / "out"),
                "--cache-path",
                str(temporary / "cache"),
            ]
            delegate_result = json.loads(
                command_output(
                    [
                        "forge",
                        "create",
                        "contracts/l2/system/SweepDeposit.sol:SweepDeposit",
                        *shared,
                    ],
                    morph_contracts,
                )
            )
            delegate = delegate_result["deployedTo"]
            if delegate.lower() != str(deployment["delegate"]).lower():
                raise RuntimeError(
                    f"production delegate address mismatch: "
                    f"{delegate} != {deployment['delegate']}"
                )
            delegate_runtime = command_output(
                ["cast", "code", "--rpc-url", rpc_url, delegate],
                morph_contracts,
            )
            delegate_code_hash = command_output(
                ["cast", "codehash", "--rpc-url", rpc_url, delegate],
                morph_contracts,
            )

            registry_result = json.loads(
                command_output(
                    [
                        "forge",
                        "create",
                        "contracts/l2/system/SweepRegistry.sol:SweepRegistry",
                        *shared,
                        "--constructor-args",
                        str(deployment["owner"]),
                        delegate,
                        delegate_code_hash,
                    ],
                    morph_contracts,
                )
            )
            registry = registry_result["deployedTo"]
            if registry.lower() != str(deployment["registry"]).lower():
                raise RuntimeError(
                    f"production Registry address mismatch: "
                    f"{registry} != {deployment['registry']}"
                )
            registry_runtime = command_output(
                ["cast", "code", "--rpc-url", rpc_url, registry],
                morph_contracts,
            )
            registry_code_hash = command_output(
                ["cast", "codehash", "--rpc-url", rpc_url, registry],
                morph_contracts,
            )

        actual = {
            "SWEEP_DEPOSIT_RUNTIME": (
                PROD_DELEGATE_RUNTIME,
                delegate_runtime,
                delegate_code_hash,
            ),
            "SWEEP_REGISTRY_RUNTIME": (
                PROD_REGISTRY_RUNTIME,
                registry_runtime,
                registry_code_hash,
            ),
        }
        for name, (fixture_path, deployed_runtime, deployed_code_hash) in actual.items():
            spec = runtime_specs[name]
            fixture_bytes = fixture_path.read_bytes()
            fixture_runtime = fixture_bytes.decode().strip()
            if fixture_runtime != deployed_runtime:
                raise RuntimeError(f"{name} differs from a fresh production deployment")
            if sha256(fixture_bytes) != spec["file_sha256"]:
                raise RuntimeError(f"{name} file SHA-256 mismatch")
            runtime_bytes = bytes.fromhex(fixture_runtime.removeprefix("0x"))
            if sha256(runtime_bytes) != spec["runtime_sha256"]:
                raise RuntimeError(f"{name} runtime SHA-256 mismatch")
            if deployed_code_hash.lower() != spec["code_hash"].lower():
                raise RuntimeError(f"{name} runtime code hash mismatch")
            print(f"verified {name} ({deployed_code_hash})")
    finally:
        anvil.terminate()
        try:
            anvil.wait(timeout=2)
        except subprocess.TimeoutExpired:
            anvil.kill()
            anvil.wait()


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

    verify_production_runtimes(manifest)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
