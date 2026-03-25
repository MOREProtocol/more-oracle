# ayUSD Spoke Vault Keeper

Reads `totalAssets()` from spoke vaults and pushes values to the hub oracle contracts on Flow EVM via a single batch transaction per cycle.

## Requirements

- Docker + Docker Compose
- A whitelisted keeper wallet with FLOW for gas (~1.5 FLOW/month at hourly intervals)

## Setup

```bash
cp .env.example .env
# Fill in KEEPER_PRIVATE_KEY
```

If another keeper is already running, add its URL to `config.toml`:

```toml
peers = ["http://<other-keeper-ip>:8080"]
```

The keeper will auto-position itself in the optimal push schedule gap.

## Run

```bash
docker compose up -d
docker compose logs -f
```

## API

| Endpoint | Description |
|---|---|
| `GET /health` | Liveness check |
| `GET /status` | Oracle states, last push, update interval |
| `POST /update` | Force an immediate update cycle |

## Add a second keeper

1. The oracle owner calls `setWhitelisted(<new_keeper_wallet>, true)` on `OracleBatchUpdater` (`0x88C62602c10D80FE04fc81c3B9368E770F93F650` on Flow EVM).
2. Set up this repo on the second machine with the new wallet's private key.
3. Add the first keeper's URL to `peers` in `config.toml`.

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
