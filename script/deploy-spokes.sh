#!/usr/bin/env bash
# deploy-spokes.sh — reads actual asset decimals from each spoke chain before deploying.
#
# Usage:
#   PRIVATE_KEY=0x... KEEPER_ADDRESS=0x... bash script/deploy-spokes.sh [--broadcast]
#
# For each active spoke, this script:
#   1. Reads asset() from the spoke vault
#   2. Reads decimals() from that asset contract
#   3. Exports DECIMALS_<SPOKE> so DeploySpokeOracles.s.sol uses the real value
#
# This eliminates the H-5 decimal misconfiguration risk for multi-asset spokes.

set -euo pipefail

# ── Spoke registry ───────────────────────────────────────────────────────────

declare -A RPCS=(
  [ARBITRUM]="https://arb1.arbitrum.io/rpc"
  [ETHEREUM]="https://eth.llamarpc.com"
  [BASE]="https://mainnet.base.org"
  [AVALANCHE]="https://api.avax.network/ext/bc/C/rpc"
  [HYPERLIQUID]="https://rpc.hyperliquid.xyz/evm"
  [PLASMA]="https://rpc.plasma.finance"
)

# Spoke vault address (same on all chains via CREATE2)
VAULT_ADDRESS="${SPOKE_VAULT_ADDRESS:-0xaf46A54208CE9924B7577AFf146dfD65eB193861}"

# ── Read decimals from each spoke ────────────────────────────────────────────

echo ""
echo "=== Reading spoke asset decimals ==="
echo ""

ALL_OK=true

for SPOKE in ARBITRUM ETHEREUM BASE AVALANCHE HYPERLIQUID PLASMA; do
  RPC="${RPCS[$SPOKE]}"

  # Read underlying asset address from the ERC-4626 vault
  ASSET=$(cast call "$VAULT_ADDRESS" "asset()(address)" --rpc-url "$RPC" 2>/dev/null || echo "")

  if [[ -z "$ASSET" || "$ASSET" == "0x0000000000000000000000000000000000000000" ]]; then
    echo "  ⚠️  $SPOKE: could not read asset() — defaulting to 6 decimals"
    export "DECIMALS_${SPOKE}=6"
    ALL_OK=false
    continue
  fi

  # Read decimals from the asset token
  DECIMALS=$(cast call "$ASSET" "decimals()(uint8)" --rpc-url "$RPC" 2>/dev/null || echo "")

  if [[ -z "$DECIMALS" ]]; then
    echo "  ⚠️  $SPOKE: could not read decimals() from $ASSET — defaulting to 6"
    export "DECIMALS_${SPOKE}=6"
    ALL_OK=false
    continue
  fi

  echo "  ✅ $SPOKE: asset=$ASSET  decimals=$DECIMALS"
  export "DECIMALS_${SPOKE}=$DECIMALS"
done

echo ""

if [[ "$ALL_OK" == false ]]; then
  echo "  ⚠️  Some spokes could not be read — check RPC endpoints above."
  echo "  Defaulted those spokes to 6. Review before broadcasting."
  echo ""
fi

echo "=== Decimal summary ==="
for SPOKE in ARBITRUM ETHEREUM BASE AVALANCHE HYPERLIQUID PLASMA; do
  VAR="DECIMALS_${SPOKE}"
  echo "  $SPOKE: ${!VAR}"
done
echo ""

# ── Run forge deploy ─────────────────────────────────────────────────────────

BROADCAST_FLAG="${1:-}"  # pass --broadcast to actually deploy

echo "=== Running forge deploy ==="
forge script script/DeploySpokeOracles.s.sol \
  --rpc-url flow \
  --private-key "$PRIVATE_KEY" \
  $BROADCAST_FLAG \
  -vvvv
