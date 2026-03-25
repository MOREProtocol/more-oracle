// SPDX-License-Identifier: MIT
pragma solidity ^0.8.10;

import {Script, console} from "forge-std/Script.sol";
import {SpokeVaultOracle} from "../src/SpokeVaultOracle.sol";

/**
 * @title DeploySpokeOracles
 * @notice Deploys 6 SpokeVaultOracle contracts on Flow EVM and whitelists the keeper.
 *         After deployment, send the printed addresses to the protocol team so they can:
 *           1. Call OracleRegistry.setSpokeOracleInfos(oracles, assets, eids)
 *           2. Call vault.setOraclesCrossChainAccounting(true)
 *
 * Usage:
 *   forge script script/DeploySpokeOracles.s.sol \
 *     --rpc-url flow \
 *     --private-key $PRIVATE_KEY \
 *     --broadcast \
 *     -vvvv
 *
 * Required env vars:
 *   PRIVATE_KEY     — deployer key (you become owner of all 6 oracle contracts)
 *   KEEPER_ADDRESS  — address to whitelist as updater (can be same as deployer)
 */
contract DeploySpokeOracles is Script {
    // ── Protocol addresses (Flow EVM) ────────────────────────────────────────

    address constant ORACLE_REGISTRY    = 0xA7b968ca75eb0224a396cA5cD482d18D4ca2041a;
    address constant VAULT              = 0xaf46A54208CE9924B7577AFf146dfD65eB193861;
    address constant HUB_ASSET          = 0x99aF3EeA856556646C98c8B9b2548Fe815240750; // PYUSD0

    uint8  constant SPOKE_ASSET_DECIMALS = 6; // PYUSD on all spokes

    // ── Spoke registry ───────────────────────────────────────────────────────

    struct Spoke {
        string  name;
        uint32  eid;
        address spokeVault; // same address on all chains (CREATE2)
        bool    active;
    }

    Spoke[] internal spokes;

    function setUp() public {
        address spoke_vault = 0xaf46A54208CE9924B7577AFf146dfD65eB193861;

        spokes.push(Spoke("Arbitrum",    30110, spoke_vault, true));
        spokes.push(Spoke("Ethereum",    30101, spoke_vault, false));
        spokes.push(Spoke("Base",        30184, spoke_vault, false));
        spokes.push(Spoke("Avalanche",   30106, spoke_vault, false));
        spokes.push(Spoke("Hyperliquid", 30367, spoke_vault, false));
        spokes.push(Spoke("Plasma",      30383, spoke_vault, false));
    }

    function run() public {
        uint256 deployerKey = vm.envUint("PRIVATE_KEY");
        address deployer    = vm.addr(deployerKey);
        address keeper      = vm.envAddress("KEEPER_ADDRESS");

        console.log("Deployer / oracle owner:", deployer);
        console.log("Keeper (whitelisted):   ", keeper);
        console.log("");

        address[] memory oracleAddrs = new address[](spokes.length);
        address[] memory assets      = new address[](spokes.length);
        uint256[] memory eids        = new uint256[](spokes.length);

        vm.startBroadcast(deployerKey);

        for (uint256 i = 0; i < spokes.length; i++) {
            Spoke memory s = spokes[i];

            SpokeVaultOracle oracle = new SpokeVaultOracle(
                ORACLE_REGISTRY,
                HUB_ASSET,
                SPOKE_ASSET_DECIMALS,
                s.spokeVault,
                s.eid,
                string(abi.encodePacked("SpokeVaultOracle/", s.name, "/PYUSD")),
                deployer
            );

            oracle.setWhitelisted(keeper, true);
            oracle.setMaxChangeBps(1500); // 15% max change per push
            oracle.update(1);             // satisfy ValueNotPositive guard

            oracleAddrs[i] = address(oracle);
            assets[i]      = HUB_ASSET;
            eids[i]        = uint256(s.eid);

            console.log(string(abi.encodePacked("[deployed] ", s.name, ":")), address(oracle));
        }

        vm.stopBroadcast();

        // ── Summary for keeper .env ───────────────────────────────────────────

        console.log("\n=== keeper/.env ===");
        for (uint256 i = 0; i < spokes.length; i++) {
            console.log(string(abi.encodePacked(
                "ORACLE_", toUpper(spokes[i].name), "=", vm.toString(oracleAddrs[i])
            )));
        }

        // ── Calldata for protocol team ────────────────────────────────────────

        console.log("\n=== SEND TO PROTOCOL TEAM ===");
        console.log("OracleRegistry:", ORACLE_REGISTRY);
        console.log("Call setSpokeOracleInfos with:");
        console.log("  oracles:");
        for (uint256 i = 0; i < spokes.length; i++) {
            console.log(string(abi.encodePacked(
                "    [", vm.toString(i), "] ", spokes[i].name, " => ", vm.toString(oracleAddrs[i])
            )));
        }
        console.log("  assets: all HUB_ASSET (", vm.toString(HUB_ASSET), ")");
        console.log("  eids:   30110, 30101, 30184, 30106, 30367, 30383");
        console.log("");
        console.log("Vault:", VAULT);
        console.log("Call setOraclesCrossChainAccounting(true)");
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    function toUpper(string memory s) internal pure returns (string memory) {
        bytes memory b = bytes(s);
        bytes memory result = new bytes(b.length);
        for (uint256 i = 0; i < b.length; i++) {
            result[i] = (b[i] >= 0x61 && b[i] <= 0x7a)
                ? bytes1(uint8(b[i]) - 32)
                : b[i];
        }
        return string(result);
    }
}
