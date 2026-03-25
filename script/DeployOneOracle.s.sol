// SPDX-License-Identifier: MIT
pragma solidity ^0.8.10;

import {Script, console} from "forge-std/Script.sol";
import {SpokeVaultOracle} from "../src/SpokeVaultOracle.sol";

/**
 * @title DeployOneOracle
 * @notice Deploys a single SpokeVaultOracle for one spoke.
 *
 * Usage:
 *   SPOKE=arbitrum  forge script script/DeployOneOracle.s.sol --rpc-url flow --broadcast -vv
 *   SPOKE=ethereum  forge script script/DeployOneOracle.s.sol --rpc-url flow --broadcast -vv
 *   SPOKE=base      forge script script/DeployOneOracle.s.sol --rpc-url flow --broadcast -vv
 *   SPOKE=avalanche forge script script/DeployOneOracle.s.sol --rpc-url flow --broadcast -vv
 *   SPOKE=hyperliquid forge script script/DeployOneOracle.s.sol --rpc-url flow --broadcast -vv
 *   SPOKE=plasma    forge script script/DeployOneOracle.s.sol --rpc-url flow --broadcast -vv
 *
 * Env vars:
 *   PRIVATE_KEY      — deployer key (becomes oracle owner)
 *   KEEPER_ADDRESS   — whitelisted updater
 *   SPOKE            — one of: arbitrum, ethereum, base, avalanche, hyperliquid, plasma
 */
contract DeployOneOracle is Script {
    address constant ORACLE_REGISTRY     = 0xA7b968ca75eb0224a396cA5cD482d18D4ca2041a;
    address constant HUB_ASSET           = 0x99aF3EeA856556646C98c8B9b2548Fe815240750;
    address constant SPOKE_VAULT         = 0xaf46A54208CE9924B7577AFf146dfD65eB193861;
    uint8   constant SPOKE_ASSET_DECIMALS = 6;

    function run() public {
        uint256 deployerKey = vm.envUint("PRIVATE_KEY");
        address deployer    = vm.addr(deployerKey);
        address keeper      = vm.envAddress("KEEPER_ADDRESS");
        string memory spoke = vm.envString("SPOKE");

        (uint32 eid, string memory label) = _spokeInfo(spoke);

        console.log("Deploying oracle for spoke:", label);
        console.log("  EID:    ", eid);
        console.log("  Owner:  ", deployer);
        console.log("  Keeper: ", keeper);

        vm.startBroadcast(deployerKey);

        SpokeVaultOracle oracle = new SpokeVaultOracle(
            ORACLE_REGISTRY,
            HUB_ASSET,
            SPOKE_ASSET_DECIMALS,
            SPOKE_VAULT,
            eid,
            string(abi.encodePacked("SpokeVaultOracle/", label, "/PYUSD")),
            deployer
        );

        oracle.setWhitelisted(keeper, true);
        oracle.setMaxChangeBps(1500);
        oracle.update(1);

        vm.stopBroadcast();

        console.log("\n[deployed]", label, "oracle:", address(oracle));
        console.log(string(abi.encodePacked(
            "ORACLE_", _upperCase(spoke), "=", vm.toString(address(oracle))
        )));
    }

    function _spokeInfo(string memory spoke) internal pure returns (uint32 eid, string memory label) {
        bytes32 h = keccak256(bytes(spoke));
        if (h == keccak256("arbitrum"))    return (30110, "Arbitrum");
        if (h == keccak256("ethereum"))    return (30101, "Ethereum");
        if (h == keccak256("base"))        return (30184, "Base");
        if (h == keccak256("avalanche"))   return (30106, "Avalanche");
        if (h == keccak256("hyperliquid")) return (30367, "Hyperliquid");
        if (h == keccak256("plasma"))      return (30383, "Plasma");
        revert("Unknown SPOKE. Use: arbitrum, ethereum, base, avalanche, hyperliquid, plasma");
    }

    function _upperCase(string memory s) internal pure returns (string memory) {
        bytes memory b = bytes(s);
        bytes memory r = new bytes(b.length);
        for (uint256 i = 0; i < b.length; i++) {
            r[i] = (b[i] >= 0x61 && b[i] <= 0x7a) ? bytes1(uint8(b[i]) - 32) : b[i];
        }
        return string(r);
    }
}
