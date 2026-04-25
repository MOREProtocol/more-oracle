use alloy::{
    eips::BlockId,
    network::EthereumWallet,
    primitives::{Address, TxHash, U256},
    providers::ProviderBuilder,
    rpc::types::BlockNumberOrTag,
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

    #[allow(missing_docs)]
    function curator() external view returns (address);

    #[allow(missing_docs)]
    function setMaxChangeBps(uint256 newMaxChangeBps) external;

    #[allow(missing_docs)]
    function maxChangeBps() external view returns (uint256);
}

// OracleBatchUpdater — single tx for all oracle updates
sol! {
    #[allow(missing_docs)]
    function batchUpdate(address[] calldata oracles, uint256[] calldata values) external;

    #[allow(missing_docs)]
    function isWhitelisted(address keeper) external view returns (bool);
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
    let call_builder = alloy::contract::SolCallBuilder::new_sol(&provider, &oracle_addr, &call)
        .block(BlockId::Number(BlockNumberOrTag::Finalized));
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

/// Result of an individual oracle update attempt.
#[derive(Debug)]
pub struct IndividualUpdateResult {
    pub oracle: Address,
    pub success: bool,
    pub tx_hash: Option<TxHash>,
    pub error: Option<String>,
}

/// Send individual `update(totalAssets)` calls to each oracle separately.
///
/// This is the fallback path when `batchUpdate()` reverts (e.g. one oracle's
/// circuit breaker tripped). Each oracle is updated independently so a single
/// failure does not block the others.
pub async fn send_individual_updates(
    flow_rpc: &str,
    signer: PrivateKeySigner,
    calls: Vec<(Address, u128)>,
) -> Vec<IndividualUpdateResult> {
    let mut results = Vec::with_capacity(calls.len());

    for (oracle_addr, total_assets) in calls {
        let oracle_str = format!("{oracle_addr:#x}");
        let total_assets_u256 = U256::from(total_assets);

        // Each call needs its own signer clone (same key, fresh nonce handling)
        let signer_clone = signer.clone();

        match push_update(&oracle_str, flow_rpc, signer_clone, total_assets_u256).await {
            Ok(tx_hash) => {
                results.push(IndividualUpdateResult {
                    oracle: oracle_addr,
                    success: true,
                    tx_hash: Some(tx_hash),
                    error: None,
                });
            }
            Err(err) => {
                results.push(IndividualUpdateResult {
                    oracle: oracle_addr,
                    success: false,
                    tx_hash: None,
                    error: Some(format!("{err:#}")),
                });
            }
        }
    }

    results
}

/// Push a new `totalAssets` value to the oracle on Flow EVM (single-oracle path).
///
/// Used as the fallback when batchUpdate() reverts, and kept for
/// backwards-compatibility / testing.
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

/// Check whether `keeper` is whitelisted on the OracleBatchUpdater contract.
pub async fn is_whitelisted(
    batch_updater: Address,
    keeper: Address,
    flow_rpc: &str,
) -> Result<bool> {
    let rpc_url = flow_rpc
        .parse::<reqwest::Url>()
        .with_context(|| format!("Invalid Flow RPC URL: {flow_rpc}"))?;

    let provider = ProviderBuilder::new().on_http(rpc_url);

    let call = isWhitelistedCall { keeper };
    let call_builder =
        alloy::contract::SolCallBuilder::new_sol(&provider, &batch_updater, &call);
    let result = call_builder
        .call()
        .await
        .with_context(|| format!("isWhitelisted() call failed for keeper {keeper}"))?;

    Ok(result._0)
}

/// Call `setMaxChangeBps(newMaxChangeBps)` on the oracle (requires oracle owner signer).
pub async fn set_max_change_bps(
    oracle_address: &str,
    flow_rpc: &str,
    signer: PrivateKeySigner,
    new_max_change_bps: u64,
) -> Result<TxHash> {
    let rpc_url = flow_rpc
        .parse::<reqwest::Url>()
        .with_context(|| format!("Invalid Flow RPC URL: {flow_rpc}"))?;

    let wallet = EthereumWallet::from(signer);
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http(rpc_url);

    let oracle_addr = Address::from_str(oracle_address)
        .with_context(|| format!("Invalid oracle address: {oracle_address}"))?;

    let call = setMaxChangeBpsCall { newMaxChangeBps: U256::from(new_max_change_bps) };
    let call_builder = alloy::contract::SolCallBuilder::new_sol(&provider, &oracle_addr, &call);
    let pending = call_builder
        .send()
        .await
        .with_context(|| format!("setMaxChangeBps() send failed on oracle {oracle_address}"))?;

    let receipt = pending
        .get_receipt()
        .await
        .context("Waiting for setMaxChangeBps() receipt failed")?;

    Ok(receipt.transaction_hash)
}

/// Read the current `maxChangeBps` from the oracle contract.
pub async fn get_max_change_bps(oracle_address: &str, flow_rpc: &str) -> Result<u64> {
    let rpc_url = flow_rpc
        .parse::<reqwest::Url>()
        .with_context(|| format!("Invalid Flow RPC URL: {flow_rpc}"))?;

    let provider = ProviderBuilder::new().on_http(rpc_url);

    let oracle_addr = Address::from_str(oracle_address)
        .with_context(|| format!("Invalid oracle address: {oracle_address}"))?;

    let call = maxChangeBpsCall {};
    let call_builder = alloy::contract::SolCallBuilder::new_sol(&provider, &oracle_addr, &call);
    let result = call_builder
        .call()
        .await
        .with_context(|| format!("maxChangeBps() call failed on oracle {oracle_address}"))?;

    let val: u64 = result._0.try_into().unwrap_or(u64::MAX);
    Ok(val)
}

/// Read the curator() address from the vault contract.
pub async fn read_curator(vault_address: &str, flow_rpc: &str) -> Result<Address> {
    let rpc_url = flow_rpc
        .parse::<reqwest::Url>()
        .with_context(|| format!("Invalid Flow RPC URL: {flow_rpc}"))?;

    let provider = ProviderBuilder::new().on_http(rpc_url);

    let vault_addr = Address::from_str(vault_address)
        .with_context(|| format!("Invalid vault address: {vault_address}"))?;

    let call = curatorCall {};
    let call_builder = alloy::contract::SolCallBuilder::new_sol(&provider, &vault_addr, &call);
    let result = call_builder
        .call()
        .await
        .with_context(|| "curator() call failed on vault")?;

    Ok(result._0)
}
