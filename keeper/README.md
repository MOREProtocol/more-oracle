# ayUSD Spoke Vault Keeper

Reads `totalAssets()` from spoke vaults and pushes values to the hub oracle contracts on Flow EVM. Uses a dual-trigger model: pushes immediately when any spoke changes more than 25 bps, and on a 1-hour heartbeat otherwise.

## Requirements

- Docker + Docker Compose
- A whitelisted keeper wallet with FLOW for gas (~1.5 FLOW/month)

## Setup

```bash
cp .env.example .env
# Fill in KEEPER_PRIVATE_KEY
```

## Run

```bash
docker compose up -d
docker compose logs -f
```

## Adding a second keeper (redundancy)

Running a second keeper eliminates the single point of failure on uptime and enables cross-validation of spoke readings.

**Step 1 — Whitelist the new wallet**

The oracle owner calls `setWhitelisted(<new_keeper_wallet>, true)` on `OracleBatchUpdater` (`0x88C62602c10D80FE04fc81c3B9368E770F93F650` on Flow EVM).

**Step 2 — Configure the new keeper**

In `config.toml`, set `keeper_url` (your own public URL) and add the existing keeper under `peers`:

```toml
keeper_url = "http://<this-keeper-ip>:8080"

peers = [
  "http://<existing-keeper-ip>:8080",
]
```

**Step 3 — Start**

```bash
docker compose up -d
```

On startup the new keeper automatically registers with each peer — it sends a challenge, signs it with its whitelisted wallet, and the peer verifies on-chain. No manual API key exchange needed. The peer then broadcasts the new keeper to any other known keepers so the whole network updates.

**What peers do once connected:**
- Cross-validate spoke readings before each push — if values diverge > 100 bps, a warning is logged
- Fall back to peer readings if a local RPC fails for a spoke
- Detect if a peer has gone silent and wake up early to cover the missed push
- Each keeper can see all peers and their last push time via `GET /status`

## API

| Endpoint | Auth | Description |
|---|---|---|
| `GET /health` | none | Liveness check |
| `GET /status` | none | Oracle states, last push, active peers |
| `GET /challenge` | none | Get a one-time challenge for curator auth |
| `POST /update` | curator signature | Force an immediate update cycle |
| `POST /peers/register` | none | Start peer registration (returns challenge) |
| `POST /peers/verify` | challenge signature | Complete peer registration (returns api_key) |
| `POST /peers/notify` | none | Notify of a new peer in the network |
| `GET /peers/spoke-values` | `X-Keeper-Key` | Get this keeper's latest spoke readings |

## Curator operational requirements

- `stalenessThreshold` in `OracleRegistry` must have a buffer above the 3-hour heartbeat — recommended **6 hours**. If the keeper is delayed and the threshold is too tight, hub deposits revert until the oracle is refreshed.
- On bridge: pause the vault → bridge assets → update oracles manually → unpause. Never unpause before updating the oracle.
- If all oracles go stale and keeper recovery is not immediate, the owner can call `setOraclesCrossChainAccounting(false)` to unblock the vault.

## Contract addresses (Flow EVM mainnet)

| Contract | Address |
|---|---|
| OracleBatchUpdater | `0x88C62602c10D80FE04fc81c3B9368E770F93F650` |
| Oracle — Arbitrum | `0xCaB4c73db4DE1f945B4425fB86449dEAa83f526A` |
| Oracle — Ethereum | `0x7fB19a56325cf2D4f9Afb15C45bBab5E71b91635` |
| Oracle — Base | `0x134BAacE6a2b05C6e744E60b789c48dE9B209922` |
| Oracle — Avalanche | `0xc61A5aFEc4A6e0755DF9Fb2B1f0F4D9BE6139309` |
| Oracle — Hyperliquid | `0x751FA01f341De8f7248bE98762675F20e02c5cD7` |
| Oracle — Plasma | `0x8feE1936f0841C1F3eD363f0576A34059f069380` |
