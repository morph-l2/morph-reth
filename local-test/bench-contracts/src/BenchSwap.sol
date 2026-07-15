// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title BenchSwap - Simplified constant-product AMM for benchmarking
/// @notice Mirrors Uniswap V2 storage access pattern: 4 SLOAD + 4 SSTORE + arithmetic + LOG
contract BenchSwap {
    uint256 public reserve0;
    uint256 public reserve1;

    mapping(address => uint256) public balance0;
    mapping(address => uint256) public balance1;

    event Swap(
        address indexed sender,
        uint256 amountIn,
        uint256 amountOut,
        uint256 reserve0After,
        uint256 reserve1After
    );

    function swap0For1(uint256 amountIn) external {
        uint256 r0 = reserve0;
        uint256 r1 = reserve1;
        uint256 bal = balance0[msg.sender];

        require(bal >= amountIn, "insufficient balance");
        require(r0 > 0 && r1 > 0, "no liquidity");

        uint256 amountInWithFee = amountIn * 997;
        uint256 amountOut = (amountInWithFee * r1) / (r0 * 1000 + amountInWithFee);

        reserve0 = r0 + amountIn;
        reserve1 = r1 - amountOut;

        balance0[msg.sender] = bal - amountIn;
        balance1[msg.sender] = balance1[msg.sender] + amountOut;

        emit Swap(msg.sender, amountIn, amountOut, r0 + amountIn, r1 - amountOut);
    }
}
