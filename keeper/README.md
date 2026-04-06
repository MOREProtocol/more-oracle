# ayUSD Spoke Vault Keeper

Reads `totalAssets()` from spoke vaults on each chain and pushes the values to `SpokeVaultOracle` contracts on Flow EVM. Uses a dual-trigger model: pushes immediately when any spoke changes more than a configurable threshold (default 25 bps), and on a 1-hour heartbeat otherwise.

## Overview

Flow EVM has no native oracle infrastructure (no Chainlink, no Pyth). The keeper fills that gap by acting as a trusted off-chain agent. The hub vault on Flow EVM cannot read spoke TVL directly across chains — it relies on these oracle contracts, updated by the keeper, to compute deposit/redeem values.

Without a running keeper, oracle values go stale after the `stalenessThreshold` configured in `OracleRegistry`. Once stale, hub deposits and redeems revert with `OraclePriceIsOld` until the oracle is refreshed.

## Architecture

```
Spoke chains (Arbitrum, Ethereum, Base, Avalanche, Hyperliquid, Plasma)
    └── vault.totalAssets()  <-- keeper reads via RPC
            |
            v
        Keeper (Rust)
            ├── drift protection (off-chain)
            ├── peer cross-validation
            └── sends tx to OracleBatchUpdater on Flow EVM
                    └── forwards to each SpokeVaultOracle.update(totalAssets)
                                └── OracleRegistry.getAssetPrice(hubAsset) * totalAssets
                                            └── Hub Vault.totalAssets() <- oracle mode
```

- `SpokeVaultOracle` — Chainlink-compatible aggregator. Stores the raw `totalAssets()` pushed by the keeper. USD conversion happens at read time using the hub `OracleRegistry` price, so the price is always current without the keeper needing to read it.
- `OracleBatchUpdater` — Batch relay. The keeper calls `batchUpdate()` once; it forwards to all oracle contracts. Adding a new keeper wallet requires one call (`setWhitelisted`) on this contract, not one per oracle.

## Prerequisites

- Docker + Docker Compose
- A keeper wallet address, whitelisted on `OracleBatchUpdater` by the oracle owner
- Sufficient FLOW on the keeper wallet for gas (~1.5 FLOW/month at 1-hour intervals)

## Setup and run

```bash
cp .env.example .env
# Fill in KEEPER_PRIVATE_KEY and verify oracle addresses
docker compose up -d
docker compose logs -f
```

Logs are structured JSON (tracing). `RUST_LOG=info` is the default; set `RUST_LOG=debug` for verbose output.

## Configuration

### `.env`

| Variable | Required | Description |
|---|---|---|
| `KEEPER_PRIVATE_KEY` | yes | Private key (0x-prefixed) of the whitelisted keeper wallet |
| `ORACLE_ARBITRUM` | yes | `SpokeVaultOracle` address on Flow EVM for the Arbitrum spoke |
| `ORACLE_ETHEREUM` | yes | `SpokeVaultOracle` address on Flow EVM for the Ethereum spoke |
| `ORACLE_BASE` | yes | `SpokeVaultOracle` address on Flow EVM for the Base spoke |
| `ORACLE_AVALANCHE` | yes | `SpokeVaultOracle` address on Flow EVM for the Avalanche spoke |
| `ORACLE_HYPERLIQUID` | yes | `SpokeVaultOracle` address on Flow EVM for the Hyperliquid spoke |
| `ORACLE_PLASMA` | yes | `SpokeVaultOracle` address on Flow EVM for the Plasma spoke |

### `config.toml` — `[hub]` section

| Field | Default | Description |
|---|---|---|
| `oracle_registry` | set | `OracleRegistry` address on Flow EVM |
| `vault_address` | set | Hub vault address on Flow EVM. Curator address is read from this at startup |
| `rpc_url` | Flow EVM mainnet | RPC endpoint for Flow EVM |
| `chain_id` | `747` | Flow EVM chain ID |
| `update_interval_secs` | `3600` | Scheduled push interval in seconds. Must be well below `stalenessThreshold` (recommended 6h = 21600s) |
| `min_update_interval_secs` | `1800` | Skip push if oracle was updated less than this many seconds ago. Prevents double-pushes from concurrent triggers |
| `monitor_interval_secs` | `60` | How often the monitor loop checks for significant spoke value changes |
| `early_push_threshold_bps` | `25` | Trigger an early push if any spoke changes by this many basis points since last push. `0` disables the monitor loop |
| `skip_if_fresh_secs` | `3600` | Scheduled loop skips a spoke if it was pushed by the monitor loop within this window |
| `batch_updater` | set | `OracleBatchUpdater` address on Flow EVM |
| `max_retries` | `3` | Number of retry attempts if all updates fail in a cycle |
| `retry_delay_secs` | `60` | Seconds to wait between retries |
| `api_port` | `8080` | Port the HTTP API listens on |
| `drift_window_secs` | `0` | Sliding window for cumulative drift protection, in seconds. `0` disables. Recommended `86400` (24h) once vault is stable |
| `max_cumulative_drift_bps` | `0` | Maximum cumulative drift from the window anchor before the keeper suppresses a push. `0` disables. Recommended `1500` (15%) once vault is stable |
| `keeper_url` | unset | Public URL of this keeper instance. Required for peer coordination |
| `peers` | `[]` | URLs of other keeper instances to register with on startup |

### `config.toml` — `[[spokes]]` sections

Each spoke defines one chain to read from. All spoke fields:

| Field | Description |
|---|---|
| `name` | Spoke identifier (e.g. `"arbitrum"`). Used to match `ORACLE_<NAME>` env var |
| `eid` | LayerZero endpoint ID for this chain (informational) |
| `chain_id` | EVM chain ID |
| `rpc_url` | RPC endpoint for this spoke chain |
| `vault_address` | Spoke vault address to call `totalAssets()` on |
| `active` | `true` to include this spoke in update cycles |

The oracle address for each spoke is loaded from the corresponding `ORACLE_<NAME>` env var (e.g. `ORACLE_ARBITRUM`). If the env var is unset, the spoke is excluded from pushes with a warning.

## API endpoints

All endpoints return JSON. Unauthenticated endpoints have no rate limiting beyond the global concurrency cap.

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET` | `/health` | none | Liveness probe. Returns `{"status":"ok"}` |
| `GET` | `/challenge` | none | Issue a one-time challenge for authenticated endpoints |
| `GET` | `/peers/challenge` | none | Issue a one-time challenge for peer-endpoint auth |
| `POST` | `/status` | challenge-response | Full keeper state: spoke oracle values, last push, peer list |
| `POST` | `/update` | challenge-response (curator only) | Trigger an immediate update cycle |
| `POST` | `/peers/register` | challenge-response | Register a peer keeper |
| `POST` | `/peers/verify` | challenge-response | Backward-compat alias for `/peers/register` |
| `POST` | `/peers/spoke-values` | challenge-response | Cached spoke readings from the last cycle |
| `POST` | `/peers/spoke-values/live` | challenge-response | Fresh spoke readings read directly from RPC |
| `POST` | `/peers/notify` | challenge-response | Notify this keeper of a new peer in the mesh |

### `GET /challenge`

```
GET /challenge?wallet=0x<address>
```

If `wallet` is provided: checks the on-chain whitelist and issues a per-wallet challenge (120s TTL). Returns the existing challenge if one is still valid (one active challenge per wallet at a time).

If `wallet` is omitted: issues a generic challenge for the curator `POST /update` flow (300s TTL).

Response:
```json
{ "challenge": "keeper-auth:<uuid>", "expires_at": 1712345678 }
```

### `GET /peers/challenge`

```
GET /peers/challenge?wallet=0x<address>
```

Required. Issues a per-wallet challenge for peer-endpoint auth. Checks on-chain whitelist before issuing.

Response:
```json
{ "challenge": "peer-auth:<uuid>", "expires_at": 1712345678 }
```

### `POST /status`

Request:
```json
{ "wallet": "0x<address>", "signature": "0x<eip191-sig>" }
```

Response:
```json
{
  "spokes": [
    {
      "name": "arbitrum",
      "oracle": "0xCaB4c73...",
      "stored_total_assets": "1000000000000",
      "last_updated": 1712345678,
      "seconds_since_update": 312,
      "active": true
    }
  ],
  "last_cycle_at": 1712345678,
  "last_cycle_tx": "0xabc123...",
  "update_interval_secs": 3600,
  "peers": [
    {
      "url": "http://keeper2.example.com:8080",
      "wallet": "0xdef456...",
      "last_seen": 1712345678,
      "last_push_at": 1712345678,
      "healthy": true
    }
  ]
}
```

### `POST /update`

Triggers an immediate update cycle. Wallet must match the vault's `curator()` address.

Request:
```json
{ "wallet": "0x<curator-address>", "signature": "0x<eip191-sig>" }
```

Response:
```json
{ "status": "triggered", "message": "update cycle triggered" }
```

### `POST /peers/register`

Registers a peer keeper. The submitting wallet must be whitelisted on `OracleBatchUpdater`.

Request:
```json
{
  "url": "https://keeper2.example.com:8080",
  "wallet": "0x<peer-wallet>",
  "signature": "0x<eip191-sig>"
}
```

On success, this keeper spawns a background task that notifies all other known peers about the new peer.

### `POST /peers/notify`

Notifies this keeper about a new peer. The caller authenticates with their own wallet (`auth_wallet`, `auth_signature`). The announced peer's wallet is verified on-chain independently.

Request:
```json
{
  "url": "https://keeper3.example.com:8080",
  "wallet": "0x<new-peer-wallet>",
  "auth_wallet": "0x<caller-wallet>",
  "auth_signature": "0x<eip191-sig>"
}
```

### `POST /peers/spoke-values` and `/peers/spoke-values/live`

Both require challenge-response auth. `/spoke-values` returns the cached readings from the last cycle; `/spoke-values/live` reads fresh from RPC.

Request:
```json
{ "wallet": "0x<address>", "signature": "0x<eip191-sig>" }
```

Response:
```json
{
  "readings": [
    { "spoke": "arbitrum", "value": "1000000000000", "source": "rpc", "at": 1712345678 }
  ]
}
```

`source` is `"rpc"`, `"peer_fallback"`, or `"failed"`.

## Authentication model

All authenticated endpoints use a unified EIP-191 challenge-response flow. There are no API keys.

### Flow

1. **Obtain challenge** — `GET /challenge?wallet=0x<your-wallet>` (or `GET /peers/challenge?wallet=` for peer endpoints). The server checks the on-chain whitelist (`OracleBatchUpdater.isWhitelisted`) before issuing. Returns `403` if not whitelisted.

2. **Sign** — Sign the `challenge` string with `eth_sign` / `personal_sign` (EIP-191 prefix applied). The message is the raw challenge string, e.g. `"keeper-auth:550e8400-..."`.

3. **Submit** — POST the target endpoint with `{ "wallet": "0x<address>", "signature": "0x<sig>" }`.

4. **Verification** — The server:
   - Removes the challenge from the store (single-use, `store.remove`)
   - Checks expiry (120s TTL for wallet challenges)
   - Recovers the signer from the EIP-191 signature
   - Verifies the recovered address matches `wallet`
   - Checks on-chain whitelist (5-minute cache)

### Challenge properties

| Property | Value |
|---|---|
| Format | `keeper-auth:<uuid4>` (curator/status/update) or `peer-auth:<uuid4>` (peer endpoints) |
| TTL | 120 seconds |
| Single-use | Yes — consumed on first use regardless of outcome |
| One per wallet | Yes — a second `GET /challenge?wallet=` returns the existing non-expired challenge, not a new one. This bounds the store size to the number of whitelisted wallets |

### Whitelist cache

On-chain whitelist lookups are cached per wallet address for 5 minutes to reduce RPC calls. The cache evicts expired entries on each write. The source of truth is always `OracleBatchUpdater.isWhitelisted(wallet)`.

### Audit log

Every auth attempt emits a structured tracing event:

- `auth_ok` — wallet, endpoint, unix timestamp
- `auth_fail` — wallet, endpoint, unix timestamp, reason (one of: `invalid wallet address`, `no pending challenge`, `challenge expired`, `invalid signature format`, `signer recovery failed`, `signature does not match wallet`, `wallet not whitelisted`)

## Peer coordination

Multiple keeper instances form a mesh. Each peer is identified by its public URL and its whitelisted wallet address.

### Registration flow

When a new keeper starts with `peers` configured:

1. **Startup positioning** — The keeper queries `POST /status` on each configured peer to find their last push timestamp. It calculates the largest gap in the circular push schedule and sleeps until the midpoint of that gap before its first push. This distributes push times evenly across keepers.

2. **Peer registration** — After the startup sleep, the keeper calls `register_with_peer` for each configured peer:
   - `GET {peer}/peers/challenge?wallet={our_wallet}` — whitelist check + challenge
   - Sign challenge with `KEEPER_PRIVATE_KEY` (EIP-191)
   - `POST {peer}/peers/register` with `{ url, wallet, signature }`

3. **Reverse registration** — When a peer accepts a `POST /peers/register`, it spawns a background task to register back with the new peer (bidirectional mesh).

4. **Broadcast** — When a peer accepts registration, it notifies all other known peers via `POST /peers/notify`. Each notified peer then registers with the new peer independently. The full mesh forms without manual configuration of all N×(N-1) peer pairs.

### Peer registry

Peer state is held in-memory and lost on restart. Keepers must re-register with each other on startup — this is by design, as it forces re-authentication after any restart.

Each peer entry tracks:

| Field | Description |
|---|---|
| `url` | Peer's public URL |
| `wallet` | Wallet address recovered from EIP-191 signature at registration time |
| `registered_at` | Unix timestamp of first registration |
| `last_seen` | Unix timestamp of last successful `POST /status` poll |
| `last_push_at` | Peer's `last_cycle_at` from their last `/status` response |
| `active` | `false` after 6 hours without a successful `/status` poll |

### Peer sync loop

Runs every 60 seconds. For each known peer:
- Fetches a challenge and posts to `POST /status`
- Updates `last_seen` and `last_push_at` in the registry

**Inactivity eviction**: peers with no successful `/status` response for more than 6 hours are marked `active = false` and evicted from the registry.

**Failover detection**: if a peer's `last_push_at` is older than `update_interval_secs + 120`, the sync loop triggers an immediate update cycle on this keeper via the shared `Notify`. This covers the case where a peer has gone silent and its scheduled push is overdue.

### Cross-validation

Before each push cycle, the keeper queries `POST /peers/spoke-values/live` on all active peers. For each spoke:
- If the local RPC read failed (`value <= 1`) and a peer has a valid reading, the peer value is used (logged as `peer_fallback`)
- If both the local read and peer reads are valid but diverge by more than 100 bps, a warning is logged

## Security model

### Transport and middleware

| Layer | Detail |
|---|---|
| Body size limit | 64 KiB per request |
| Request timeout | 30 seconds |
| Concurrency cap | 256 in-flight requests (drops excess with `408`) |
| Security headers | See table below |

HTTP security headers set on every response:

| Header | Value |
|---|---|
| `X-Content-Type-Options` | `nosniff` |
| `X-Frame-Options` | `DENY` |
| `Content-Security-Policy` | `default-src 'none'` |
| `Cache-Control` | `no-store` |
| `Strict-Transport-Security` | `max-age=31536000; includeSubDomains` |

### Authentication

All sensitive endpoints (status, update, peer operations) require EIP-191 challenge-response. The on-chain whitelist (`OracleBatchUpdater.isWhitelisted`) is the source of truth. No static secrets or API keys are used.

### SSRF protection (URL validation)

Peer URLs submitted to `/peers/register`, `/peers/notify`, and all outbound peer calls are validated before any HTTP request is made. Rejected:
- Non-`http`/`https` schemes
- IPv4 loopback (`127.x.x.x`)
- IPv6 loopback (`::1`)
- RFC 1918 private ranges (`10.x`, `172.16–31.x`, `192.168.x`)
- IPv4 link-local (`169.254.x.x`)
- IPv6 link-local (`fe80::/10`)
- IPv6 unique-local (`fc00::/7`)
- Hostnames that DNS-resolve to any of the above

### Drift protection (off-chain circuit breaker)

Configured via `drift_window_secs` and `max_cumulative_drift_bps` in `config.toml`. When enabled, the keeper tracks `totalAssets` values per oracle within a sliding time window. If the cumulative change from the anchor (first value in the window) exceeds the threshold, the push for that oracle is suppressed and an `error`-level `DRIFT ALERT` log is emitted.

This is an off-chain safety check. The window resets on keeper restart.

Recommended values once the vault is stable: `drift_window_secs = 86400`, `max_cumulative_drift_bps = 1500` (15% over 24h).

### On-chain circuit breaker

`SpokeVaultOracle` has a `maxChangeBps` parameter (set by the oracle owner). If a push would change the stored value by more than `maxChangeBps` in a single update, the transaction reverts with `CircuitBreakerTripped`. The oracle goes stale until the owner intervenes. Set to `0` to disable.

## Multi-keeper setup

Running multiple keepers eliminates the single point of failure and enables cross-validation.

**Step 1 — Whitelist the new wallet**

```bash
cast send 0x88C62602c10D80FE04fc81c3B9368E770F93F650 \
  "setWhitelisted(address,bool)" <NEW_KEEPER_WALLET> true \
  --private-key $ORACLE_OWNER_PK \
  --rpc-url https://mainnet.evm.nodes.onflow.org
```

**Step 2 — Configure the new keeper**

In `config.toml`, set `keeper_url` to this keeper's own public URL and add existing keepers under `peers`:

```toml
keeper_url = "https://keeper2.example.com:8080"

peers = [
  "https://keeper1.example.com:8080",
]
```

**Step 3 — Start**

```bash
docker compose up -d
```

On startup the keeper automatically registers with each configured peer using challenge-response auth. No manual API key exchange is needed. The peer then broadcasts the new keeper to any other known peers so the full mesh forms automatically.

### What connected peers do

- Cross-validate spoke readings before each push — divergences >100 bps are logged as warnings
- Fall back to peer readings if a local RPC call fails for a spoke
- Distribute push times evenly across the schedule (startup sleep calculation)
- Detect peer inactivity and trigger early update cycles to cover missed pushes
- Evict peers that have been silent for more than 6 hours

## Recovery procedures

### Oracle is stale (keeper is down or behind)

1. Check keeper logs for the root cause.
2. If the keeper is running but behind schedule, `POST /update` with curator credentials to force an immediate cycle.
3. If the keeper is not running, start it. It will push on its first cycle.

### All oracles stale and recovery is not immediate

The vault owner can disable oracle mode to unblock deposits and redeems:

```bash
cast send 0xaf46A54208CE9924B7577AFf146dfD65eB193861 \
  "setOraclesCrossChainAccounting(bool)" false \
  --private-key $VAULT_OWNER_PK \
  --rpc-url https://mainnet.evm.nodes.onflow.org
```

Re-enable once the keeper is running and oracles are fresh:

```bash
cast send 0xaf46A54208CE9924B7577AFf146dfD65eB193861 \
  "setOraclesCrossChainAccounting(bool)" true \
  --private-key $VAULT_OWNER_PK \
  --rpc-url https://mainnet.evm.nodes.onflow.org
```

### On-chain circuit breaker tripped

If `SpokeVaultOracle.maxChangeBps` is set and a push trips it, the oracle goes stale. The oracle owner must either:
- Call `setMaxChangeBps(0)` to disable the circuit breaker, then the keeper's next cycle will succeed
- Or push the correct value manually via a whitelisted wallet

### Bridge operations

When bridging assets between chains: pause the vault → bridge → update oracles manually (force a cycle with `POST /update`) → unpause. Never unpause before the oracle reflects the post-bridge TVL.

## Contract addresses (Flow EVM mainnet)

| Contract | Address |
|---|---|
| OracleBatchUpdater | `0x88C62602c10D80FE04fc81c3B9368E770F93F650` |
| Oracle — Arbitrum (EID 30110) | `0xCaB4c73db4DE1f945B4425fB86449dEAa83f526A` |
| Oracle — Ethereum (EID 30101) | `0x7fB19a56325cf2D4f9Afb15C45bBab5E71b91635` |
| Oracle — Base (EID 30184) | `0x134BAacE6a2b05C6e744E60b789c48dE9B209922` |
| Oracle — Avalanche (EID 30106) | `0xc61A5aFEc4A6e0755DF9Fb2B1f0F4D9BE6139309` |
| Oracle — Hyperliquid (EID 30367) | `0x751FA01f341De8f7248bE98762675F20e02c5cD7` |
| Oracle — Plasma (EID 30383) | `0x8feE1936f0841C1F3eD363f0576A34059f069380` |
