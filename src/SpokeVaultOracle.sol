// SPDX-License-Identifier: MIT
pragma solidity ^0.8.10;

import {IAggregatorV2V3Interface} from "./interfaces/IAggregatorV2V3Interface.sol";

interface IOracleRegistry {
    function getAssetPrice(address asset) external view returns (uint256);
}

/**
 * @title SpokeVaultOracle
 * @notice Chainlink-compatible aggregator that reports the USD value of a
 *         spoke vault to the hub chain's OracleRegistry.
 *
 * The keeper pushes the raw totalAssets() of the spoke vault (in the spoke's
 * underlying token decimals). The USD conversion is done on-chain at read time
 * using the hub OracleRegistry price for the hub asset — so the price is always
 * fresh without the keeper needing to read it.
 *
 * Value reported = storedTotalAssets * oracleRegistry.getAssetPrice(hubAsset)
 *                  / 10^spokeAssetDecimals
 *
 * Security:
 * - Only whitelisted addresses can push values (owner manages whitelist)
 * - Circuit breaker: value cannot change more than maxChangeBps per update
 *   (set to 0 to disable). If tripped, updates revert and the vault goes stale
 *   blocking deposits/redeems until owner intervenes.
 * - Two-step ownership transfer.
 */
contract SpokeVaultOracle is IAggregatorV2V3Interface {
    // ── Errors ───────────────────────────────────────────────────────────────

    error NotOwner();
    error NotPendingOwner();
    error NotWhitelisted();
    error ValueNotPositive();
    error ZeroAddress();
    error CircuitBreakerTripped(int256 submitted, int256 current, uint256 maxChangeBps);

    // ── Events ───────────────────────────────────────────────────────────────

    event ValueUpdated(address indexed updater, uint256 totalAssets, uint80 roundId);
    event UpdaterWhitelisted(address indexed updater, bool status);
    event CircuitBreakerUpdated(uint256 maxChangeBps);
    event OwnershipTransferStarted(address indexed currentOwner, address indexed pendingOwner);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);

    // ── Ownership ────────────────────────────────────────────────────────────

    address public owner;
    address public pendingOwner;

    // ── Whitelist ────────────────────────────────────────────────────────────

    mapping(address => bool) public isWhitelisted;

    // ── Circuit breaker ──────────────────────────────────────────────────────

    /// @notice Max allowed change per update in basis points (0 = disabled).
    ///         e.g. 500 = max 5% change up or down per update.
    uint256 public maxChangeBps;

    // ── Oracle state ─────────────────────────────────────────────────────────

    uint80 private _roundId;
    /// @notice Last raw totalAssets() pushed by the keeper (in spokeAssetDecimals)
    uint256 public storedTotalAssets;
    uint256 private _latestTimestamp;

    mapping(uint80 => uint256) private _rawAnswers;
    mapping(uint80 => uint256) private _timestamps;

    // ── Immutable config ─────────────────────────────────────────────────────

    /// @notice OracleRegistry on hub chain — used to read hubAsset price at read time
    IOracleRegistry public immutable ORACLE_REGISTRY;
    /// @notice Depositable asset of the hub vault (e.g. PYUSD0 on Flow EVM)
    address public immutable HUB_ASSET;
    /// @notice Decimals of the spoke vault's underlying token (e.g. 6 for PYUSD)
    uint8 public immutable SPOKE_ASSET_DECIMALS;
    /// @notice Address of the spoke vault being tracked (informational)
    address public immutable SPOKE_VAULT;
    /// @notice EID of the spoke chain (informational)
    uint32 public immutable SPOKE_EID;

    string public desc;

    // ── Modifiers ────────────────────────────────────────────────────────────

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier onlyWhitelisted() {
        if (!isWhitelisted[msg.sender]) revert NotWhitelisted();
        _;
    }

    // ── Constructor ──────────────────────────────────────────────────────────

    constructor(
        address _oracleRegistry,
        address _hubAsset,
        uint8 _spokeAssetDecimals,
        address _spokeVault,
        uint32 _spokeEid,
        string memory _description,
        address _owner
    ) {
        if (_oracleRegistry == address(0) || _hubAsset == address(0) || _owner == address(0)) revert ZeroAddress();

        ORACLE_REGISTRY = IOracleRegistry(_oracleRegistry);
        HUB_ASSET = _hubAsset;
        SPOKE_ASSET_DECIMALS = _spokeAssetDecimals;
        SPOKE_VAULT = _spokeVault;
        SPOKE_EID = _spokeEid;
        desc = _description;
        owner = _owner;

        emit OwnershipTransferred(address(0), _owner);
    }

    // ── Owner: whitelist ─────────────────────────────────────────────────────

    function setWhitelisted(address updater, bool status) external onlyOwner {
        if (updater == address(0)) revert ZeroAddress();
        isWhitelisted[updater] = status;
        emit UpdaterWhitelisted(updater, status);
    }

    // ── Owner: circuit breaker ───────────────────────────────────────────────

    function setMaxChangeBps(uint256 _maxChangeBps) external onlyOwner {
        maxChangeBps = _maxChangeBps;
        emit CircuitBreakerUpdated(_maxChangeBps);
    }

    // ── Owner: two-step ownership ────────────────────────────────────────────

    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        pendingOwner = newOwner;
        emit OwnershipTransferStarted(owner, newOwner);
    }

    function acceptOwnership() external {
        if (msg.sender != pendingOwner) revert NotPendingOwner();
        emit OwnershipTransferred(owner, msg.sender);
        owner = msg.sender;
        pendingOwner = address(0);
    }

    // ── Write ────────────────────────────────────────────────────────────────

    /**
     * @notice Push the spoke vault's totalAssets() value.
     * @param totalAssets Raw totalAssets() from the spoke vault, in spoke
     *                    underlying decimals (e.g. 1_000_000e6 for 1M PYUSD).
     */
    function update(uint256 totalAssets) external onlyWhitelisted {
        if (totalAssets == 0) revert ValueNotPositive();

        // Circuit breaker: check max allowed change from stored value
        if (maxChangeBps > 0 && storedTotalAssets > 0) {
            uint256 current = storedTotalAssets;
            uint256 delta = totalAssets > current ? totalAssets - current : current - totalAssets;
            if (delta * 10000 > current * maxChangeBps) {
                revert CircuitBreakerTripped(int256(totalAssets), int256(current), maxChangeBps);
            }
        }

        uint80 newRoundId = _roundId + 1;
        _roundId = newRoundId;
        storedTotalAssets = totalAssets;
        _latestTimestamp = block.timestamp;

        _rawAnswers[newRoundId] = totalAssets;
        _timestamps[newRoundId] = block.timestamp;

        emit AnswerUpdated(_computeUsdValue(totalAssets), newRoundId, block.timestamp);
        emit NewRound(newRoundId, msg.sender, block.timestamp);
        emit ValueUpdated(msg.sender, totalAssets, newRoundId);
    }

    // ── Internal ─────────────────────────────────────────────────────────────

    /// @dev Converts raw totalAssets to USD using hub OracleRegistry price.
    ///      result = totalAssets * assetPrice / 10^spokeAssetDecimals
    function _computeUsdValue(uint256 totalAssets) internal view returns (int256) {
        uint256 price = ORACLE_REGISTRY.getAssetPrice(HUB_ASSET);
        return int256(totalAssets * price / (10 ** SPOKE_ASSET_DECIMALS));
    }

    // ── IAggregatorV2V3Interface ─────────────────────────────────────────────

    function decimals() external pure override returns (uint8) {
        return 8;
    }

    function description() external view override returns (string memory) {
        return desc;
    }

    function version() external pure override returns (uint256) {
        return 1;
    }

    function latestAnswer() external view override returns (int256) {
        return _computeUsdValue(storedTotalAssets);
    }

    function latestTimestamp() external view override returns (uint256) {
        return _latestTimestamp;
    }

    function latestRound() external view override returns (uint256) {
        return _roundId;
    }

    function getAnswer(uint256 roundId) external view override returns (int256) {
        return _computeUsdValue(_rawAnswers[uint80(roundId)]);
    }

    function getTimestamp(uint256 roundId) external view override returns (uint256) {
        return _timestamps[uint80(roundId)];
    }

    function getRoundData(uint80 roundId_)
        external
        view
        override
        returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound)
    {
        uint256 ts = _timestamps[roundId_];
        return (roundId_, _computeUsdValue(_rawAnswers[roundId_]), ts, ts, roundId_);
    }

    function latestRoundData()
        external
        view
        override
        returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound)
    {
        int256 usdValue = _computeUsdValue(storedTotalAssets);
        return (_roundId, usdValue, _latestTimestamp, _latestTimestamp, _roundId);
    }
}
