//! Morph-specific `eth_config` RPC handler.
//!
//! Implements the EIP-7910 `eth_config` endpoint with Morph extension fields.
//! The standard EIP-7910 response is extended with a `morph` object on each
//! fork config containing:
//! - `useZktrie`: whether the chain uses ZkTrie (pre-Jade) or MPT (post-Jade)
//! - `jadeForkTime`: the Jade hardfork activation timestamp (if configured)
//!
//! This is required by morphnode which calls `eth_config` at startup to determine
//! the trie type and Jade fork timing.

use alloy_consensus::BlockHeader;
use alloy_eips::eip7840::BlobParams;
use alloy_primitives::Address;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use morph_chainspec::{
    hardfork::{MorphHardfork, MorphHardforks},
    spec::MorphChainSpec,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, ForkCondition, Hardforks, Head};
use reth_errors::{ProviderError, RethError};
use reth_evm::{
    ConfigureEvm, Evm,
    precompiles::{Precompile, PrecompilesMap},
};
use reth_node_api::NodePrimitives;
use reth_primitives_traits::header::HeaderMut;
use reth_revm::db::EmptyDB;
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::BlockReaderIdExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ─── Custom response types ──────────────────────────────────────────────────

/// Response type for `eth_config` with Morph extension.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphEthConfig {
    /// Fork configuration of the current active fork.
    pub current: MorphForkConfig,
    /// Fork configuration of the next scheduled fork.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<MorphForkConfig>,
    /// Fork configuration of the last fork.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<MorphForkConfig>,
}

/// A single fork configuration with Morph extension fields.
///
/// This mirrors `alloy_eips::eip7910::EthForkConfig` but adds the `morph` extension.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphForkConfig {
    /// The fork activation timestamp.
    pub activation_time: u64,
    /// Blob schedule parameters.
    pub blob_schedule: BlobParams,
    /// Chain ID (hex-encoded quantity string).
    #[serde(with = "alloy_serde::quantity")]
    pub chain_id: u64,
    /// The fork hash from EIP-6122.
    pub fork_id: alloy_primitives::Bytes,
    /// Active precompile contracts: name -> address.
    pub precompiles: BTreeMap<String, Address>,
    /// System contracts: name -> address.
    pub system_contracts: BTreeMap<String, Address>,
    /// Morph-specific extension fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub morph: Option<MorphExtension>,
}

/// Morph-specific extension fields for the fork config.
///
/// morphnode reads these to determine trie type and Jade fork timing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphExtension {
    /// Whether the chain uses ZkTrie at this fork's activation time.
    /// Pre-Jade = true, post-Jade = false.
    pub use_zktrie: bool,
    /// The Jade hardfork activation timestamp, if configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jade_fork_time: Option<u64>,
}

// ─── RPC trait ──────────────────────────────────────────────────────────────

/// RPC endpoint for `eth_config` with Morph extension.
#[rpc(server, namespace = "eth")]
pub trait MorphEthConfigApi {
    /// Returns an object with data about recent and upcoming fork configurations,
    /// including Morph-specific extension fields.
    #[method(name = "config")]
    fn config(&self) -> RpcResult<MorphEthConfig>;
}

// ─── Handler ────────────────────────────────────────────────────────────────

/// Handler for the `eth_config` RPC endpoint with Morph extensions.
#[derive(Debug, Clone)]
pub struct MorphEthConfigHandler<Provider, Evm> {
    provider: Provider,
    evm_config: Evm,
}

impl<Provider, EvmConfig> MorphEthConfigHandler<Provider, EvmConfig>
where
    Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>
        + BlockReaderIdExt<Header: HeaderMut>
        + 'static,
    EvmConfig: ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + 'static,
{
    /// Creates a new [`MorphEthConfigHandler`].
    pub const fn new(provider: Provider, evm_config: EvmConfig) -> Self {
        Self {
            provider,
            evm_config,
        }
    }

    /// Extracts the Jade fork timestamp from the chain spec, if configured.
    fn jade_fork_time(&self) -> Option<u64> {
        match self
            .provider
            .chain_spec()
            .morph_fork_activation(MorphHardfork::Jade)
        {
            ForkCondition::Timestamp(t) => Some(t),
            _ => None,
        }
    }

    /// Returns the Morph extension for a given fork activation timestamp.
    fn morph_extension_at(&self, timestamp: u64) -> MorphExtension {
        let chain_spec = self.provider.chain_spec();
        // Pre-Jade uses ZkTrie, post-Jade uses MPT
        let use_zktrie = !chain_spec.is_jade_active_at_timestamp(timestamp);
        MorphExtension {
            use_zktrie,
            jade_fork_time: self.jade_fork_time(),
        }
    }

    /// Builds a fork config for a specific timestamp.
    fn build_fork_config_at(
        &self,
        timestamp: u64,
        precompiles: BTreeMap<String, Address>,
    ) -> MorphForkConfig {
        let chain_spec = self.provider.chain_spec();

        // Morph L2 doesn't use standard Ethereum system contracts
        // (no beacon roots, no deposit contract, etc.)
        let system_contracts = BTreeMap::<String, Address>::new();

        let fork_id = chain_spec
            .fork_id(&Head {
                timestamp,
                number: u64::MAX,
                ..Default::default()
            })
            .hash
            .0
            .into();

        MorphForkConfig {
            activation_time: timestamp,
            blob_schedule: chain_spec
                .blob_params_at_timestamp(timestamp)
                .unwrap_or(BlobParams::cancun()),
            chain_id: chain_spec.chain().id(),
            fork_id,
            precompiles,
            system_contracts,
            morph: Some(self.morph_extension_at(timestamp)),
        }
    }

    /// Core implementation of the `eth_config` method.
    fn config_impl(&self) -> Result<MorphEthConfig, RethError> {
        let chain_spec = self.provider.chain_spec();
        let latest = self
            .provider
            .latest_header()?
            .ok_or_else(|| ProviderError::BestBlockNotFound)?
            .into_header();

        let current_precompiles = evm_to_precompiles_map(
            self.evm_config
                .evm_for_block(EmptyDB::default(), &latest)
                .map_err(RethError::other)?,
        );

        let mut fork_timestamps = chain_spec
            .forks_iter()
            .filter_map(|(_, cond)| cond.as_timestamp())
            .collect::<Vec<_>>();
        fork_timestamps.sort_unstable();
        fork_timestamps.dedup();

        let latest_ts = latest.timestamp();
        let current_fork_timestamp = fork_timestamps
            .iter()
            .copied()
            .rfind(|&ts| ts <= latest_ts)
            .ok_or_else(|| RethError::msg("no active timestamp fork found"))?;
        let next_fork_timestamp = fork_timestamps.iter().copied().find(|&ts| ts > latest_ts);

        let current = self.build_fork_config_at(current_fork_timestamp, current_precompiles);

        let mut config = MorphEthConfig {
            current,
            next: None,
            last: None,
        };

        if let Some(next_fork_timestamp) = next_fork_timestamp {
            let fake_header = {
                let mut header = latest.clone();
                header.set_timestamp(next_fork_timestamp);
                header
            };
            let next_precompiles = evm_to_precompiles_map(
                self.evm_config
                    .evm_for_block(EmptyDB::default(), &fake_header)
                    .map_err(RethError::other)?,
            );

            config.next = Some(self.build_fork_config_at(next_fork_timestamp, next_precompiles));
        } else {
            // No future fork scheduled — no "last" either.
            return Ok(config);
        }

        let last_fork_timestamp = fork_timestamps.last().copied().unwrap();
        let fake_header = {
            let mut header = latest;
            header.set_timestamp(last_fork_timestamp);
            header
        };
        let last_precompiles = evm_to_precompiles_map(
            self.evm_config
                .evm_for_block(EmptyDB::default(), &fake_header)
                .map_err(RethError::other)?,
        );

        config.last = Some(self.build_fork_config_at(last_fork_timestamp, last_precompiles));

        Ok(config)
    }
}

impl<Provider, EvmConfig> MorphEthConfigApiServer for MorphEthConfigHandler<Provider, EvmConfig>
where
    Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>
        + BlockReaderIdExt<Header: HeaderMut>
        + 'static,
    EvmConfig: ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + 'static,
{
    fn config(&self) -> RpcResult<MorphEthConfig> {
        Ok(self.config_impl().map_err(EthApiError::from)?)
    }
}

/// Extracts a precompiles name -> address map from an EVM instance.
fn evm_to_precompiles_map(
    evm: impl Evm<Precompiles = PrecompilesMap>,
) -> BTreeMap<String, Address> {
    let precompiles = evm.precompiles();
    precompiles
        .addresses()
        .filter_map(|address| {
            Some((
                precompiles.get(address)?.precompile_id().name().to_string(),
                *address,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_morph_extension_serialization() {
        let ext = MorphExtension {
            use_zktrie: true,
            jade_fork_time: Some(1700000000),
        };
        let json = serde_json::to_value(&ext).unwrap();
        assert_eq!(json["useZktrie"], true);
        assert_eq!(json["jadeForkTime"], 1700000000);
    }

    #[test]
    fn test_morph_extension_without_jade() {
        let ext = MorphExtension {
            use_zktrie: true,
            jade_fork_time: None,
        };
        let json = serde_json::to_value(&ext).unwrap();
        assert_eq!(json["useZktrie"], true);
        assert!(json.get("jadeForkTime").is_none());
    }

    #[test]
    fn test_morph_fork_config_serialization() {
        let config = MorphForkConfig {
            activation_time: 0,
            blob_schedule: BlobParams::cancun(),
            chain_id: 0xb0a2,
            fork_id: alloy_primitives::Bytes::from_static(&[0x01, 0x02, 0x03, 0x04]),
            precompiles: BTreeMap::new(),
            system_contracts: BTreeMap::new(),
            morph: Some(MorphExtension {
                use_zktrie: false,
                jade_fork_time: Some(1700000000),
            }),
        };
        let json = serde_json::to_value(&config).unwrap();
        // chain_id should be hex-encoded quantity
        assert_eq!(json["chainId"], "0xb0a2");
        // morph extension should be present
        assert!(json["morph"].is_object());
        assert_eq!(json["morph"]["useZktrie"], false);
        assert_eq!(json["morph"]["jadeForkTime"], 1700000000);
    }
}
