// SPDX-License-Identifier: MIT
pragma solidity ^0.8.10;

interface ISpokeVaultOracle {
    function update(uint256 totalAssets) external;
}

/**
 * @title OracleBatchUpdater
 * @notice Batches multiple SpokeVaultOracle.update() calls in a single tx.
 *
 * Two-layer security:
 *   - Only whitelisted keeper wallets can call batchUpdate() on this contract.
 *   - The oracle contracts only whitelist this contract's address.
 *   Adding a new keeper = one call here (setWhitelisted), not 6 oracle calls.
 */
contract OracleBatchUpdater {
    error NotOwner();
    error NotWhitelisted();
    error LengthMismatch();
    error ZeroAddress();

    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);
    event KeeperWhitelisted(address indexed keeper, bool status);

    address public owner;
    address public pendingOwner;
    mapping(address => bool) public isWhitelisted;

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier onlyWhitelisted() {
        if (!isWhitelisted[msg.sender]) revert NotWhitelisted();
        _;
    }

    constructor(address _owner) {
        if (_owner == address(0)) revert ZeroAddress();
        owner = _owner;
        emit OwnershipTransferred(address(0), _owner);
    }

    function setWhitelisted(address keeper, bool status) external onlyOwner {
        if (keeper == address(0)) revert ZeroAddress();
        isWhitelisted[keeper] = status;
        emit KeeperWhitelisted(keeper, status);
    }

    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        pendingOwner = newOwner;
    }

    function acceptOwnership() external {
        if (msg.sender != pendingOwner) revert NotOwner();
        emit OwnershipTransferred(owner, msg.sender);
        owner = msg.sender;
        pendingOwner = address(0);
    }

    /**
     * @notice Push totalAssets to multiple oracles in one tx.
     *         Only whitelisted keeper wallets can call this.
     */
    function batchUpdate(
        address[] calldata oracles,
        uint256[] calldata values
    ) external onlyWhitelisted {
        if (oracles.length != values.length) revert LengthMismatch();
        for (uint256 i = 0; i < oracles.length; i++) {
            ISpokeVaultOracle(oracles[i]).update(values[i]);
        }
    }
}
