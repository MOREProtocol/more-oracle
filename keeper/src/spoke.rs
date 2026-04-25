use alloy::{
    eips::BlockId,
    primitives::{Address, U256},
    providers::ProviderBuilder,
    rpc::types::BlockNumberOrTag,
    sol,
};
use eyre::{Context, Result};
use futures::future::join_all;
use std::str::FromStr;

use crate::config::SpokeConfig;

// Minimal ABI for the spoke vault
sol! {
    #[allow(missing_docs)]
    function totalAssets() external view returns (uint256);
}

/// Read `totalAssets()` from the spoke vault on the spoke chain.
///
/// Returns the raw U256 from the contract. Returns an error if the RPC call
/// fails; the caller is responsible for retry logic.
pub async fn read_total_assets(spoke: &SpokeConfig) -> Result<U256> {
    let rpc_url = spoke
        .rpc_url
        .parse::<reqwest::Url>()
        .with_context(|| format!("[{}] invalid RPC URL: {}", spoke.name, spoke.rpc_url))?;

    let provider = ProviderBuilder::new()
        .on_http(rpc_url);

    let vault_addr = Address::from_str(&spoke.vault_address).with_context(|| {
        format!(
            "[{}] invalid vault address: {}",
            spoke.name, spoke.vault_address
        )
    })?;

    // Build call data for totalAssets() — read against finalized block to avoid reorg risk
    let call = totalAssetsCall {};
    let call_builder = alloy::contract::SolCallBuilder::new_sol(&provider, &vault_addr, &call)
        .block(BlockId::Number(BlockNumberOrTag::Finalized));
    let result = call_builder
        .call()
        .await
        .with_context(|| format!("[{}] totalAssets() RPC call failed", spoke.name))?;

    Ok(result._0)
}

/// Read `totalAssets()` from all spokes in parallel.
///
/// Returns a Vec of `(spoke_name, value, rpc_failed)` triples:
/// - `rpc_failed = false`: RPC call succeeded (value may be 1 if vault is empty)
/// - `rpc_failed = true`: RPC call failed; spoke will be skipped — oracle not updated this cycle
pub async fn read_all_spokes(spokes: &[SpokeConfig]) -> Vec<(String, u128, bool)> {
    const MAX_ATTEMPTS: u32 = 3;
    const RETRY_DELAY_MS: u64 = 3_000;

    let futures: Vec<_> = spokes
        .iter()
        .map(|spoke| {
            let spoke = spoke.clone();
            async move {
                let name = spoke.name.clone();
                let mut last_err = String::new();
                for attempt in 1..=MAX_ATTEMPTS {
                    match read_total_assets(&spoke).await {
                        Ok(val) => {
                            let as_u128 = if val == U256::ZERO {
                                tracing::warn!(
                                    spoke = %name,
                                    "totalAssets() returned 0 — using 1 to avoid ValueNotPositive revert"
                                );
                                1u128
                            } else {
                                let max = U256::from(u128::MAX);
                                if val > max { u128::MAX } else { val.to::<u128>() }
                            };
                            return (name, as_u128, false);
                        }
                        Err(err) => {
                            last_err = err.to_string();
                            if attempt < MAX_ATTEMPTS {
                                tracing::warn!(
                                    spoke = %name,
                                    attempt,
                                    "totalAssets() failed, retrying in {RETRY_DELAY_MS}ms"
                                );
                                tokio::time::sleep(tokio::time::Duration::from_millis(RETRY_DELAY_MS)).await;
                            }
                        }
                    }
                }
                tracing::warn!(
                    spoke = %name,
                    error = %last_err,
                    "failed to read totalAssets() after {MAX_ATTEMPTS} attempts — spoke will be skipped this cycle"
                );
                (name, 1u128, true)
            }
        })
        .collect();

    join_all(futures).await
}
