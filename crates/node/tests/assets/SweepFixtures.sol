// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.30;

/// Test-only Registry fixture for morph-node integration tests.
/// This is not the production SweepRegistry contract.
contract TestSweepRegistry {
    mapping(address token => mapping(address deposit => address master)) private masters;
    mapping(address token => bool enabled) public tokenWhitelist;
    mapping(address token => uint256 amount) public minimumSweepAmount;

    event SweepRequested(address indexed token, address indexed deposit);

    function resolveSweep(address token, address deposit)
        external
        view
        returns (address master, bytes32 codeHash, uint256 minimumAmount)
    {
        master = masters[token][deposit];
        if (!tokenWhitelist[token] || master == address(0)) return (address(0), bytes32(0), 0);
        uint256 minimumAmount = minimumSweepAmount[token];
        return (master, token.codehash, minimumAmount == 0 ? 1 : minimumAmount);
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

/// Test-only deposit contract for exercising the explicit sweep-executor path.
contract TestSweepDeposit {
    address private constant SWEEP_EXECUTOR = 0x5300000000000000000000000000000000000024;

    function sweep(address token, address master, uint256 amount) external {
        require(msg.sender == SWEEP_EXECUTOR, "only sweep executor");

        (bool success, bytes memory returndata) = token.call(abi.encodeCall(ITestErc20.transfer, (master, amount)));
        require(
            success && token.code.length != 0 && (returndata.length == 0 || abi.decode(returndata, (bool))),
            "token transfer failed"
        );
    }
}

/// Test-only router used to mutate Registry state and emit an inflow in one transaction.
contract TestSweepRouter {
    ITestSweepRegistry private constant REGISTRY = ITestSweepRegistry(0x5300000000000000000000000000000000000023);

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
