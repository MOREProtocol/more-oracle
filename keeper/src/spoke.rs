use alloy::{
    primitives::{Address, U256},
    providers::ProviderBuilder,
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

    // Build call data for totalAssets()
    let call = totalAssetsCall {};
    let call_builder = alloy::contract::SolCallBuilder::new_sol(&provider, &vault_addr, &call);
    let result = call_builder
        .call()
        .await
        .with_context(|| format!("[{}] totalAssets() RPC call failed", spoke.name))?;

    Ok(result._0)
}

/// Read `totalAssets()` from all spokes in parallel.
///
/// Returns a Vec of (spoke_name, total_assets_u128) pairs. For any spoke where
/// the read fails or returns zero, the value `1` is used (avoids ValueNotPositive
/// revert on the oracle).
pub async fn read_all_spokes(spokes: &[SpokeConfig]) -> Vec<(String, u128)> {
    let futures: Vec<_> = spokes
        .iter()
        .map(|spoke| {
            let spoke = spoke.clone();
            async move {
                let name = spoke.name.clone();
                match read_total_assets(&spoke).await {
                    Ok(val) => {
                        let as_u128 = if val == U256::ZERO {
                            tracing::warn!(
                                spoke = %name,
                                "totalAssets() returned 0 — using 1 to avoid ValueNotPositive revert"
                            );
                            1u128
                        } else {
                            // Clamp to u128::MAX if the value overflows
                            let max = U256::from(u128::MAX);
                            if val > max {
                                u128::MAX
                            } else {
                                val.to::<u128>()
                            }
                        };
                        (name, as_u128)
                    }
                    Err(err) => {
                        tracing::warn!(
                            spoke = %name,
                            error = %err,
                            "failed to read totalAssets() — using 1"
                        );
                        (name, 1u128)
                    }
                }
            }
        })
        .collect();

    join_all(futures).await
}
