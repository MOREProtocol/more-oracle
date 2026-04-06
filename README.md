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

## Keeper API

The keeper exposes an HTTP API. All sensitive endpoints require EIP-191 challenge-response authentication — there are no API keys. The wallet must be whitelisted on `OracleBatchUpdater`.

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET` | `/health` | none | Liveness probe |
| `GET` | `/challenge` | none | Issue a one-time challenge (120s TTL) |
| `GET` | `/peers/challenge` | none | Issue a one-time challenge for peer endpoints |
| `POST` | `/status` | challenge-response | Full keeper state: spokes, last push, peers |
| `POST` | `/update` | challenge-response (curator only) | Trigger an immediate update cycle |
| `POST` | `/peers/register` | challenge-response | Register a peer keeper |
| `POST` | `/peers/verify` | challenge-response | Backward-compat alias for `/peers/register` |
| `POST` | `/peers/spoke-values` | challenge-response | Cached spoke readings from last cycle |
| `POST` | `/peers/spoke-values/live` | challenge-response | Fresh spoke readings from RPC |
| `POST` | `/peers/notify` | challenge-response | Notify of a new peer in the mesh |

**Auth flow:** `GET /challenge?wallet=0x<address>` → sign the returned `challenge` string with EIP-191 (`personal_sign`) → POST target endpoint with `{ wallet, signature }`. Challenges are single-use and expire after 120 seconds.

## Keeper security

- Unified EIP-191 challenge-response on all sensitive endpoints — no static secrets
- On-chain whitelist (`OracleBatchUpdater.isWhitelisted`) is the source of truth; cached 5 minutes per wallet
- SSRF protection: peer URLs are validated before any outbound request — rejects non-http/s schemes, loopback, RFC 1918, link-local, and unique-local addresses (including DNS rebinding via hostname resolution)
- HTTP security headers on every response: `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Content-Security-Policy: default-src 'none'`, `Cache-Control: no-store`, `Strict-Transport-Security`
- Request body limit: 64 KiB; request timeout: 30s; concurrency cap: 256
- Structured audit log on every auth attempt (`auth_ok` / `auth_fail` with wallet, endpoint, timestamp, reason)
- Off-chain drift circuit breaker: suppresses pushes if cumulative value change exceeds threshold within a sliding window
- On-chain circuit breaker: `SpokeVaultOracle.maxChangeBps` reverts individual updates that exceed the per-update change limit

## Oracle staleness

The vault reverts with `OraclePriceIsOld` if the oracle value is older than the configured `stalenessThreshold` — recommended **6 hours**. The keeper pushes every hour by default, leaving a 5-hour safety margin. The monitor loop pushes immediately on any spoke change > 25 bps.

If all oracles go stale and keeper recovery is not immediate, the vault owner can call `setOraclesCrossChainAccounting(false)` to unblock the vault.
