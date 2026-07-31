// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// The classic educational Greeter — stores one message on-chain.
contract Greeter {
    string public greeting;
    address public owner;

    event GreetingChanged(address indexed by, string greeting);

    constructor(string memory _greeting) {
        greeting = _greeting;
        owner = msg.sender;
        emit GreetingChanged(msg.sender, _greeting);
    }

    function setGreeting(string memory _greeting) public {
        greeting = _greeting;
        emit GreetingChanged(msg.sender, _greeting);
    }

    function greet() public view returns (string memory) {
        return greeting;
    }
}
