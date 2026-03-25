# ayUSD Vault Topology

## Hub Vault (Flow EVM)

| Field | Value |
|---|---|
| Address | `0xaf46A54208CE9924B7577AFf146dfD65eB193861` |
| Chain | Flow EVM (chainId: 747) |
| LZ EID | 30336 |
| Deposit token | PYUSD0 `0x99af3eea856556646c98c8b9b2548fe815240750` (6 decimals) |
| totalAssets (hub) | ~7.48 PYUSD (as of 2026-03-25) |

## Spoke Vaults (same address on all chains — deterministic CREATE2)

All spokes deployed at: `0xaf46A54208CE9924B7577AFf146dfD65eB193861`

| LZ EID | Chain | ChainId | totalAssets | Active? |
|---|---|---|---|---|
| 30367 | Hyperliquid | - | 0 | ❌ |
| 30101 | Ethereum | 1 | 0 | ❌ |
| 30110 | Arbitrum | 42161 | 0 | ❌ (target) |
| 30106 | Avalanche | 43114 | 0 | ❌ |
| 30383 | Plasma | - | 0 | ❌ |
| 30184 | Base | 8453 | 0 | ❌ |

> Only Arbitrum (30110) is the intended active spoke. The rest were registered
> in the factory at deploy time for future expansion but have no funds.

## Protocol Contracts (Flow EVM)

| Contract | Address |
|---|---|
| OmniFactory (VaultsFactory) | `0x7bDB8B17604b03125eFAED33cA0c55FBf856BB0C` |
| MORE Vaults Registry | `0x6a0b3724af49ce6f14669d07823650ec26553890` |
| OracleRegistry | `0xA7b968ca75eb0224a396cA5cD482d18D4ca2041a` |
| OracleRegistry DEFAULT_ADMIN | `0x9224d8544526752cc0C63c8877a5c0F7fC53f1ad` |
| Vault Owner | `0x6A66AeB125Ad05c3d35B4E26CD1033963cE0bA5C` |
| PYUSD0 Aggregator (existing) | `0x69427cDe8c085c3cB44F2666784ecE8721298A7C` |

## Oracle Setup Required

To enable `oraclesCrossChainAccounting = true` on the hub vault, the OracleRegistry
needs a `SpokeVaultOracle` registered for **all 6 spokes**.

| LZ EID | Chain | Oracle value strategy |
|---|---|---|
| 30367 | Hyperliquid | Push `spoke.totalAssets()` (currently 0) |
| 30101 | Ethereum | Push `spoke.totalAssets()` (currently 0) |
| 30110 | Arbitrum | Push `spoke.totalAssets()` — **main active spoke** |
| 30106 | Avalanche | Push `spoke.totalAssets()` (currently 0) |
| 30383 | Plasma | Push `spoke.totalAssets()` (currently 0) |
| 30184 | Base | Push `spoke.totalAssets()` (currently 0) |

Inactive spokes can be initialized with value `1` (minimum positive) and
updated to 0 once the keeper confirms they remain empty — or left at their
last pushed value since `totalAssets() = 0` would make the oracle revert
(`ValueNotPositive`). Consider initializing them with `1` wei equivalent.

## Keeper Responsibility

For each deployed `SpokeVaultOracle`:
1. Read `spokeVault.totalAssets()` on the spoke chain
2. Call `spokeOracle.update(totalAssets)` on Flow EVM
3. Recommended update frequency: daily (15% APY = ~0.04%/day, well within any circuit breaker)

> Note: For inactive spokes with `totalAssets() = 0`, the keeper should push `1`
> to satisfy `ValueNotPositive` guard, since 0 assets is a valid empty vault state.
