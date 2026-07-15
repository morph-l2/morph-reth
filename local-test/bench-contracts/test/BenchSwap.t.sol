// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "../src/BenchSwap.sol";

interface Vm {
    function store(address target, bytes32 slot, bytes32 value) external;
    function prank(address sender) external;
    function expectRevert(bytes calldata reason) external;
}

contract BenchSwapTest {
    Vm private constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));
    BenchSwap swap;
    address alice = address(0xA11CE);

    function assertEq(uint256 actual, uint256 expected) private pure {
        require(actual == expected, "values are not equal");
    }

    function assertGt(uint256 actual, uint256 expected) private pure {
        require(actual > expected, "value is not greater");
    }

    function assertLt(uint256 actual, uint256 expected) private pure {
        require(actual < expected, "value is not less");
    }

    function setUp() public {
        swap = new BenchSwap();
        // Set reserves via storage manipulation
        vm.store(address(swap), bytes32(uint256(0)), bytes32(uint256(1e24))); // reserve0
        vm.store(address(swap), bytes32(uint256(1)), bytes32(uint256(1e24))); // reserve1
        // Set alice balance0
        bytes32 slot = keccak256(abi.encode(alice, uint256(2))); // balance0 mapping at slot 2
        vm.store(address(swap), slot, bytes32(uint256(1e24)));
    }

    function test_swap_gas() public {
        vm.prank(alice);
        uint256 gasBefore = gasleft();
        swap.swap0For1(1000);
        uint256 gasUsed = gasBefore - gasleft();
        assertGt(gasUsed, 30_000);
        assertLt(gasUsed, 200_000);
    }

    function test_swap_updates_state() public {
        vm.prank(alice);
        swap.swap0For1(1000);
        assertEq(swap.reserve0(), 1e24 + 1000);
        assertLt(swap.reserve1(), 1e24);
        assertEq(swap.balance0(alice), 1e24 - 1000);
        assertGt(swap.balance1(alice), 0);
    }

    function test_swap_insufficient_balance_reverts() public {
        vm.prank(address(0xDEAD));
        vm.expectRevert("insufficient balance");
        swap.swap0For1(1);
    }
}
