# `forge`

Forge is a fast and flexible Ethereum testing framework, inspired by
[Dapp](https://github.com/dapphub/dapptools/tree/master/src/dapp)

If you are looking into how to consume the software as an end user, check the
[CLI README](../cli/README.md).

For more context on how the package works under the hood, look in the
[code docs](./src/lib.rs).

## Why?

### Write your tests in Solidity to minimize context switching

Writing tests in Javascript/Typescript while writing your smart contracts in
Solidity can be confusing. Forge lets you write your tests in Solidity, so you
can focus on what matters.

```solidity
contract Foo {
    uint256 public x = 1;
    function set(uint256 _x) external {
        x = _x;
    }

    function double() external {
        x = 2 * x;
    }
}

contract FooTest {
    Foo foo;

    // The state of the contract gets reset before each
    // test is run, with the `setUp()` function being called
    // each time after deployment.
    function setUp() public {
        foo = new Foo();
    }

    // A simple unit test
    function testDouble() public {
        require(foo.x() == 1);
        foo.double();
        require(foo.x() == 2);
    }
}
```

### Fuzzing: Go beyond unit testing

When testing smart contracts, fuzzing can uncover edge cases which would be hard
to manually detect with manual unit testing. We support fuzzing natively, where
any test function that takes >0 arguments will be fuzzed, using the
[proptest](https://docs.rs/proptest/1.0.0/proptest/) crate.

An example of how a fuzzed test would look like can be seen below:

```solidity
function testDoubleWithFuzzing(uint256 x) public {
    foo.set(x);
    require(foo.x() == x);
    foo.double();
    require(foo.x() == 2 * x);
}
```

## Features

- [ ] test
  - [x] Simple unit tests
    - [x] Gas costs
    - [x] DappTools style test output
    - [x] JSON test output
    - [x] Matching on regex
    - [x] DSTest-style assertions support
  - [x] Fuzzing
  - [ ] Symbolic execution
  - [ ] Coverage
  - [x] HEVM-style Solidity cheatcodes
  - [ ] Structured tracing with abi decoding
  - [ ] Per-line gas profiling
  - [x] Forking mode
  - [x] Automatic solc selection
- [x] build
  - [x] Can read DappTools-style .sol.json artifacts
  - [x] Manual remappings
  - [x] Automatic remappings
  - [x] Multiple compiler versions
  - [x] Incremental compilation
  - [ ] Can read Hardhat-style artifacts
  - [ ] Can read Truffle-style artifacts
- [x] install
- [x] update
- [ ] debug
- [x] CLI Tracing with `RUST_LOG=forge=trace`

### Cheat codes

_The below is modified from
[Dapp's README](https://github.com/dapphub/dapptools/blob/master/src/hevm/README.md#cheat-codes)_

We allow modifying blockchain state with "cheat codes". These can be accessed by
calling into a contract at address `0x7109709ECfa91a80626fF3989D68f67F5b1DD12D`,
which implements the following methods:

- `function warp(uint x) public` Sets the block timestamp to `x`.

- `function roll(uint x) public` Sets the block number to `x`.

- `function store(address c, bytes32 loc, bytes32 val) public` Sets the slot
  `loc` of contract `c` to `val`.

- `function load(address c, bytes32 loc) public returns (bytes32 val)` Reads the
  slot `loc` of contract `c`.

- `function sign(uint sk, bytes32 digest) public returns (uint8 v, bytes32 r, bytes32 s)`
  Signs the `digest` using the private key `sk`. Note that signatures produced
  via `hevm.sign` will leak the private key.

- `function addr(uint sk) public returns (address addr)` Derives an ethereum
  address from the private key `sk`. Note that `hevm.addr(0)` will fail with
  `BadCheatCode` as `0` is an invalid ECDSA private key.

- `function ffi(string[] calldata) external returns (bytes memory)` Executes the
  arguments as a command in the system shell and returns stdout. Note that this
  cheatcode means test authors can execute arbitrary code on user machines as
  part of a call to `dapp test`, for this reason all calls to `ffi` will fail
  unless the `--ffi` flag is passed.

- `function deal(address who, uint256 amount)`: Sets an account's balance

- `function etch(address where, bytes memory what)`:` Sets the contract code at
  some address contract code

- `function prank(address from, address to, bytes calldata) (bool success,bytes retdata)`:
  Performs a smart contract call as another address
- `function expectRevert(bytes calldata expectedError)`:
  Tells the evm to expect that the next call reverts with specified error bytes.

The below example uses the `warp` cheatcode to override the timestamp & `expectRevert` to expect a specific revert string:

```solidity
interface Vm {
    function warp(uint256 x) external;
    function expectRevert(bytes calldata) external;
}

contract Foo {
    function bar(uint256 a) public returns (uint256) {
        require(a < 100, "My expected revert string");
        return a;
    }
}

contract MyTest {
    Vm vm = Vm(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D);

    function testWarp() public {
        vm.warp(100);
        require(block.timestamp == 100);
    }

    function testBarExpectedRevert() public {
        vm.expectRevert("My expected revert string");
        // This would fail *if* we didnt expect revert. Since we expect the revert,
        // it doesn't, unless the revert string is wrong.
        foo.bar(101);
    }

    function testFailBar() public {
        // this call would revert, causing this test to pass
        foo.bar(101);
    }
}
```

A full interface for all cheatcodes is here:
```solidity
interface Vm {
    // Set block.timestamp (newTimestamp)
    function warp(uint256) external;
    // Set block.height (newHeight)
    function roll(uint256) external;
    // Loads a storage slot from an address (who, slot)
    function load(address,bytes32) external returns (bytes32);
    // Stores a value to an address' storage slot, (who, slot, value)
    function store(address,bytes32,bytes32) external;
    // Signs data, (privateKey, digest) => (r, v, s)
    function sign(uint256,bytes32) external returns (uint8,bytes32,bytes32);
    // Gets address for a given private key, (privateKey) => (address)
    function addr(uint256) external returns (address);
    // Performs a foreign function call via terminal, (stringInputs) => (result)
    function ffi(string[] calldata) external returns (bytes memory);
    // Calls another contract with a specified `msg.sender`, (newSender, contract, input) => (success, returnData)
    function prank(address, address, bytes calldata) external payable returns (bool, bytes memory);
    // Sets an address' balance, (who, newBalance)
    function deal(address, uint256) external;
    // Sets an address' code, (who, newCode)
    function etch(address, bytes calldata) external;
    // Expects an error on next call
    function expectRevert(bytes calldata) external;
}
```

## Future Features

### Dapptools feature parity

Over the next months, we intend to add the following features which are
available in upstream dapptools:

1. Stack Traces: Currently we do not provide any debug information when a call
   fails. We intend to add a structured printer (something like
   [this](https://twitter.com/gakonst/status/1434337110111182848) which will
   show all the calls, logs and arguments passed across intermediate smart
   contract calls, which should help with debugging.
1. [Invariant Tests](https://github.com/dapphub/dapptools/blob/master/src/dapp/README.md#invariant-testing)
1. [Interactive Debugger](https://github.com/dapphub/dapptools/blob/master/src/hevm/README.md#interactive-debugger-key-bindings)
1. [Code coverage](https://twitter.com/dapptools/status/1435973810545729536)
1. [Gas snapshots](https://github.com/dapphub/dapptools/pull/850/files)
1. [Symbolic EVM](https://fv.ethereum.org/2020/07/28/symbolic-hevm-release/)

### Unique features?

We also intend to add features which are not available in dapptools:

1. Even faster tests with parallel EVM execution that produces state diffs
   instead of modifying the state
1. Improved UX for assertions:
   1. Check revert error or reason on a Solidity call
   1. Check that an event was emitted with expected arguments
1. Support more EVM backends ([revm](https://github.com/bluealloy/revm/), geth's
   evm, hevm etc.) & benchmark performance across them
1. Declarative deployment system based on a config file
1. Formatting & Linting (maybe powered by
   [Solang](https://github.com/hyperledger-labs/solang))
   1. `dapp fmt`, an automatic code formatter according to standard rules (like
      [`prettier-plugin-solidity`](https://github.com/prettier-solidity/prettier-plugin-solidity))
   1. `dapp lint`, a linter + static analyzer, like a combination of
      [`solhint`](https://github.com/protofire/solhint) and
      [slither](https://github.com/crytic/slither/)
1. Flamegraphs for gas profiling
