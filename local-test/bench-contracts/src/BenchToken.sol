// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title BenchToken - Minimal ERC20 for benchmarking
/// @notice Mints entire supply to deployer. Only implements transfer + balanceOf.
contract BenchToken {
    string public constant name = "BenchToken";
    string public constant symbol = "BENCH";
    uint8 public constant decimals = 18;
    uint256 public totalSupply;

    mapping(address => uint256) public balanceOf;

    event Transfer(address indexed from, address indexed to, uint256 value);

    constructor(uint256 _initialSupply) {
        totalSupply = _initialSupply;
        balanceOf[msg.sender] = _initialSupply;
        emit Transfer(address(0), msg.sender, _initialSupply);
    }

    function transfer(address to, uint256 value) external returns (bool) {
        require(balanceOf[msg.sender] >= value, "insufficient balance");
        balanceOf[msg.sender] -= value;
        balanceOf[to] += value;
        emit Transfer(msg.sender, to, value);
        return true;
    }
}
