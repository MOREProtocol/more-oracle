use alloy::{
    network::EthereumWallet,
    primitives::{Address, TxHash, U256},
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
    sol,
};
use eyre::{Context, Result};
use std::str::FromStr;

// Minimal ABI for SpokeVaultOracle on Flow EVM
sol! {
    #[allow(missing_docs)]
    function latestTimestamp() external view returns (uint256);

    #[allow(missing_docs)]
    function storedTotalAssets() external view returns (uint256);

    #[allow(missing_docs)]
    function update(uint256 totalAssets) external;
}

// OracleBatchUpdater — single tx for all oracle updates
sol! {
    #[allow(missing_docs)]
    function batchUpdate(address[] calldata oracles, uint256[] calldata values) external;
}


/// Check the `latestTimestamp()` on the oracle contract.
///
/// Returns the U256 timestamp from the oracle on Flow EVM.
pub async fn latest_timestamp(
    oracle_address: &str,
    flow_rpc: &str,
) -> Result<U256> {
    let rpc_url = flow_rpc
        .parse::<reqwest::Url>()
        .with_context(|| format!("Invalid Flow RPC URL: {flow_rpc}"))?;

    let provider = ProviderBuilder::new().on_http(rpc_url);

    let oracle_addr =
        Address::from_str(oracle_address).with_context(|| {
            format!("Invalid oracle address: {oracle_address}")
        })?;

    let call = latestTimestampCall {};
    let call_builder = alloy::contract::SolCallBuilder::new_sol(&provider, &oracle_addr, &call);
    let result = call_builder
        .call()
        .await
        .with_context(|| format!("latestTimestamp() call failed on oracle {oracle_address}"))?;

    Ok(result._0)
}

/// Check the `storedTotalAssets()` on the oracle contract.
///
/// Returns the U256 value from the oracle on Flow EVM.
pub async fn stored_total_assets(
    oracle_address: &str,
    flow_rpc: &str,
) -> Result<U256> {
    let rpc_url = flow_rpc
        .parse::<reqwest::Url>()
        .with_context(|| format!("Invalid Flow RPC URL: {flow_rpc}"))?;

    let provider = ProviderBuilder::new().on_http(rpc_url);

    let oracle_addr =
        Address::from_str(oracle_address).with_context(|| {
            format!("Invalid oracle address: {oracle_address}")
        })?;

    let call = storedTotalAssetsCall {};
    let call_builder = alloy::contract::SolCallBuilder::new_sol(&provider, &oracle_addr, &call);
    let result = call_builder
        .call()
        .await
        .with_context(|| format!("storedTotalAssets() call failed on oracle {oracle_address}"))?;

    Ok(result._0)
}

/// Send a single `batchUpdate(oracles, values)` tx via the OracleBatchUpdater contract.
///
/// The batch updater contract calls each oracle directly so `msg.sender`
/// is the batch updater (whitelisted), not Multicall3.
pub async fn send_batch_update(
    flow_rpc: &str,
    batch_updater: Address,
    signer: PrivateKeySigner,
    calls: Vec<(Address, u128)>,
) -> Result<TxHash> {
    let rpc_url = flow_rpc
        .parse::<reqwest::Url>()
        .with_context(|| format!("Invalid Flow RPC URL: {flow_rpc}"))?;

    let wallet = EthereumWallet::from(signer);
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http(rpc_url);

    let (oracles, values): (Vec<Address>, Vec<U256>) = calls
        .into_iter()
        .map(|(addr, val)| (addr, U256::from(val)))
        .unzip();

    let call = batchUpdateCall { oracles, values };
    let call_builder = alloy::contract::SolCallBuilder::new_sol(&provider, &batch_updater, &call);

    let pending = call_builder
        .send()
        .await
        .context("batchUpdate() send failed")?;

    let receipt = pending
        .get_receipt()
        .await
        .context("Waiting for batchUpdate() receipt failed")?;

    Ok(receipt.transaction_hash)
}

/// Push a new `totalAssets` value to the oracle on Flow EVM (single-oracle path,
/// kept for backwards-compatibility / testing).
#[allow(dead_code)]
///
/// Uses the keeper wallet as signer. Returns the transaction hash on success.
pub async fn push_update(
    oracle_address: &str,
    flow_rpc: &str,
    signer: PrivateKeySigner,
    total_assets: U256,
) -> Result<alloy::primitives::TxHash> {
    let rpc_url = flow_rpc
        .parse::<reqwest::Url>()
        .with_context(|| format!("Invalid Flow RPC URL: {flow_rpc}"))?;

    let wallet = EthereumWallet::from(signer);

    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http(rpc_url);

    let oracle_addr =
        Address::from_str(oracle_address).with_context(|| {
            format!("Invalid oracle address: {oracle_address}")
        })?;

    let call = updateCall { totalAssets: total_assets };
    let call_builder = alloy::contract::SolCallBuilder::new_sol(&provider, &oracle_addr, &call);
    let pending = call_builder
        .send()
        .await
        .with_context(|| format!("update() send failed on oracle {oracle_address}"))?;

    let receipt = pending
        .get_receipt()
        .await
        .with_context(|| "Waiting for update() receipt failed")?;

    Ok(receipt.transaction_hash)
}
