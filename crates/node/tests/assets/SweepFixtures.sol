// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.30;

/// Test-only Registry fixture for morph-node integration tests.
/// This is not the production SweepRegistry contract.
contract TestSweepRegistry {
    mapping(address token => mapping(address deposit => address master)) private masters;
    mapping(address token => bool enabled) public tokenWhitelist;

    event SweepRequested(address indexed token, address indexed deposit);

    // V1 resolver ABI: returns a single 32-byte `address master`, matching the
    // production SweepRegistry and the EL's `decode_address` (exactly 32 bytes).
    function resolveSweep(address token, address deposit) external view returns (address master) {
        if (!tokenWhitelist[token]) return address(0);
        return masters[token][deposit];
    }

    function setSweep(address token, address deposit, address master) external {
        tokenWhitelist[token] = true;
        masters[token][deposit] = master;
    }

    function pokeSweep(address token, address deposit) external {
        emit SweepRequested(token, deposit);
    }
}

interface ITestSweepRegistry {
    function setSweep(address token, address deposit, address master) external;
}

interface ITestErc20 {
    function transfer(address to, uint256 amount) external returns (bool);
}

/// Test-only router used to mutate Registry state and emit an inflow in one transaction.
///
/// The pinned REGISTRY address must equal the morph-reth SWEEP_REGISTRY_ADDRESS
/// constant (crates/chainspec/src/constants.rs) — it is the address the EL test
/// deploys the fixture to. Re-derived for the controller model; see
/// contracts/scripts/lib/onyx-sweep-common.sh (ONYX_EXPECTED_REGISTRY).
contract TestSweepRouter {
    ITestSweepRegistry private constant REGISTRY =
        ITestSweepRegistry(0x0fF2Ea62eBca29E70aE2b0551a54eFFa4ea7DeEa);

    function enableThenTransfer(address token, address deposit, address master, uint256 amount) external {
        REGISTRY.setSweep(token, deposit, master);
        require(ITestErc20(token).transfer(deposit, amount));
    }

    function transferThenDisable(address token, address deposit, uint256 amount) external {
        require(ITestErc20(token).transfer(deposit, amount));
        REGISTRY.setSweep(token, deposit, address(0));
    }
}

/// Emits sixteen exact Transfer-shaped candidates to exercise bounded preflight filtering.
contract TestCandidateEmitter {
    event Transfer(address indexed from, address indexed to, uint256 amount);

    fallback() external {
        for (uint160 i = 1; i <= 16; ++i) {
            emit Transfer(msg.sender, address(i), 1);
        }
    }
}
