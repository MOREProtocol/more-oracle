# more-oracle

Spoke vault oracle system for ayUSD on Flow EVM. Enables direct hub deposits without LayerZero by pushing spoke `totalAssets()` on-chain via a keeper.

## Architecture

```
Keeper wallet
    └── OracleBatchUpdater (single tx)
            ├── SpokeVaultOracle[Arbitrum].update()
            ├── SpokeVaultOracle[Ethereum].update()
            └── ...
                    └── OracleRegistry.getAssetPrice() → USD value
                                └── Hub Vault.totalAssets() ← oracle mode on
```

- **SpokeVaultOracle** — Chainlink-compatible aggregator. Stores raw `totalAssets()` pushed by the keeper, computes USD value on-chain at read time using the hub OracleRegistry price.
- **OracleBatchUpdater** — Batch contract. Keeper calls one tx here; it forwards to all oracles. Has its own whitelist so adding a new keeper = one call, not six.

## Deployed contracts (Flow EVM mainnet)

| Contract | Address |
|---|---|
| OracleBatchUpdater | `0x88C62602c10D80FE04fc81c3B9368E770F93F650` |
| Oracle — Arbitrum (EID 30110) | `0xCaB4c73db4DE1f945B4425fB86449dEAa83f526A` |
| Oracle — Ethereum (EID 30101) | `0x7fB19a56325cf2D4f9Afb15C45bBab5E71b91635` |
| Oracle — Base (EID 30184) | `0x134BAacE6a2b05C6e744E60b789c48dE9B209922` |
| Oracle — Avalanche (EID 30106) | `0xc61A5aFEc4A6e0755DF9Fb2B1f0F4D9BE6139309` |
| Oracle — Hyperliquid (EID 30367) | `0x751FA01f341De8f7248bE98762675F20e02c5cD7` |
| Oracle — Plasma (EID 30383) | `0x8feE1936f0841C1F3eD363f0576A34059f069380` |

## Protocol team setup (one-time)

Register the oracles in the OracleRegistry and enable oracle mode on the vault.

**1. OracleRegistry admin** — call `setSpokeOracleInfos`:

```bash
cast send 0xA7b968ca75eb0224a396cA5cD482d18D4ca2041a \
  "setSpokeOracleInfos(address[],address[],uint256[])" \
  "[0xCaB4c73db4DE1f945B4425fB86449dEAa83f526A,0x7fB19a56325cf2D4f9Afb15C45bBab5E71b91635,0x134BAacE6a2b05C6e744E60b789c48dE9B209922,0xc61A5aFEc4A6e0755DF9Fb2B1f0F4D9BE6139309,0x751FA01f341De8f7248bE98762675F20e02c5cD7,0x8feE1936f0841C1F3eD363f0576A34059f069380]" \
  "[0x99aF3EeA856556646C98c8B9b2548Fe815240750,0x99aF3EeA856556646C98c8B9b2548Fe815240750,0x99aF3EeA856556646C98c8B9b2548Fe815240750,0x99aF3EeA856556646C98c8B9b2548Fe815240750,0x99aF3EeA856556646C98c8B9b2548Fe815240750,0x99aF3EeA856556646C98c8B9b2548Fe815240750]" \
  "[30110,30101,30184,30106,30367,30383]" \
  --private-key $ORACLE_REGISTRY_ADMIN_PK \
  --rpc-url https://mainnet.evm.nodes.onflow.org
```

**2. Vault owner** — call `setOraclesCrossChainAccounting(true)`:

```bash
cast send 0xaf46A54208CE9924B7577AFf146dfD65eB193861 \
  "setOraclesCrossChainAccounting(bool)" true \
  --private-key $VAULT_OWNER_PK \
  --rpc-url https://mainnet.evm.nodes.onflow.org
```

> Step 1 must be done before step 2, otherwise the vault reverts with `NoOracleForSpoke`.

## Adding a keeper

1. Oracle owner whitelists the new keeper wallet on `OracleBatchUpdater`:

```bash
cast send 0x88C62602c10D80FE04fc81c3B9368E770F93F650 \
  "setWhitelisted(address,bool)" <NEW_KEEPER_WALLET> true \
  --private-key $ORACLE_OWNER_PK \
  --rpc-url https://mainnet.evm.nodes.onflow.org
```

2. Follow the keeper setup in [`keeper/README.md`](keeper/README.md).

## Oracle staleness

The vault reverts with `OraclePriceIsOld` if the oracle value is older than **3 hours**. The keeper pushes every hour by default, leaving a 2-hour safety margin.
