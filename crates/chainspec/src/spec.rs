//! Morph chain specification.

use crate::{
    MORPH_BASE_FEE,
    genesis::{MorphChainConfig, MorphGenesisInfo},
    hardfork::{MorphHardfork, MorphHardforks},
};
use alloy_chains::Chain;
use alloy_consensus::Header;
use alloy_eips::eip7840::BlobParams;
use alloy_evm::eth::spec::EthExecutorSpec;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, U256};
use reth_chainspec::{
    BaseFeeParams, ChainSpec, DepositContract, DisplayHardforks, EthChainSpec, EthereumHardfork,
    EthereumHardforks, ForkCondition, ForkFilter, ForkId, Hardfork, Hardforks, Head,
};
use reth_network_peers::NodeRecord;

#[cfg(feature = "cli")]
use crate::{morph::MORPH_MAINNET, morph_hoodi::MORPH_HOODI};
#[cfg(feature = "cli")]
use std::sync::Arc;

/// Chains supported by Morph. First value should be used as the default.
pub const SUPPORTED_CHAINS: &[&str] = &["mainnet", "hoodi"];

// =============================================================================
// Chain Specification Parser (CLI)
// =============================================================================

/// Morph chain specification parser.
#[derive(Debug, Clone, Default)]
pub struct MorphChainSpecParser;

/// Clap value parser for [`MorphChainSpec`]s.
///
/// The value parser matches either a known chain, the path
/// to a json file, or a json formatted string in-memory.
#[cfg(feature = "cli")]
pub fn chain_value_parser(s: &str) -> eyre::Result<Arc<MorphChainSpec>> {
    Ok(match s {
        "mainnet" => MORPH_MAINNET.clone(),
        "hoodi" => MORPH_HOODI.clone(),
        _ => Arc::new(MorphChainSpec::from(reth_cli::chainspec::parse_genesis(s)?)),
    })
}

#[cfg(feature = "cli")]
impl reth_cli::chainspec::ChainSpecParser for MorphChainSpecParser {
    type ChainSpec = MorphChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<Self::ChainSpec>> {
        chain_value_parser(s)
    }
}

// =============================================================================
// ChainConfig Trait
// =============================================================================

/// Returns the chain configuration.
#[auto_impl::auto_impl(Arc)]
pub trait ChainConfig {
    /// The configuration type.
    type Config;

    /// Returns the chain configuration.
    fn chain_config(&self) -> &Self::Config;
}

impl ChainConfig for MorphChainSpec {
    type Config = MorphChainConfig;

    fn chain_config(&self) -> &Self::Config {
        &self.info.morph_chain_info
    }
}

// =============================================================================
// MorphChainSpec
// =============================================================================

/// Morph chain spec type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MorphChainSpec {
    /// [`ChainSpec`].
    pub inner: ChainSpec<Header>,
    pub info: MorphGenesisInfo,
}

impl MorphChainSpec {
    /// Create a new [`MorphChainSpec`] with the given inner spec and config.
    pub fn new(inner: ChainSpec<Header>, info: MorphGenesisInfo) -> Self {
        Self { inner, info }
    }

    /// Returns whether the fee vault is enabled.
    pub fn is_fee_vault_enabled(&self) -> bool {
        self.info.morph_chain_info.is_fee_vault_enabled()
    }

    /// Returns the fee vault address.
    pub fn fee_vault_address(&self) -> Option<Address> {
        self.info.morph_chain_info.fee_vault_address
    }

    /// Returns the maximum tx payload size per block in bytes.
    pub fn max_tx_payload_bytes_per_block(&self) -> Option<usize> {
        self.info.morph_chain_info.max_tx_payload_bytes_per_block
    }

    /// Checks if the given block size (in bytes) is valid for this chain.
    pub fn is_valid_block_size(&self, size: usize) -> bool {
        self.info.morph_chain_info.is_valid_block_size(size)
    }
}

impl From<ChainSpec> for MorphChainSpec {
    fn from(value: ChainSpec) -> Self {
        let genesis = value.genesis;
        genesis.into()
    }
}

impl From<Genesis> for MorphChainSpec {
    fn from(genesis: Genesis) -> Self {
        let chain_info = MorphGenesisInfo::extract_from(&genesis.config.extra_fields)
            .expect("failed to extract morph genesis info");

        let hardfork_info = chain_info.hard_fork_info.clone().unwrap_or_default();

        // Create base chainspec from genesis (already has ordered Ethereum hardforks)
        let mut base_spec = ChainSpec::from_genesis(genesis);

        // Add Morph hardforks
        // Note: Bernoulli and Curie use block-based activation,
        // while Morph203, Viridian, and Emerald use timestamp-based activation.
        let block_forks = vec![
            (MorphHardfork::Bernoulli, hardfork_info.bernoulli_block),
            (MorphHardfork::Curie, hardfork_info.curie_block),
        ]
        .into_iter()
        .filter_map(|(fork, block)| block.map(|b| (fork, ForkCondition::Block(b))));

        let time_forks = vec![
            (MorphHardfork::Morph203, hardfork_info.morph203_time),
            (MorphHardfork::Viridian, hardfork_info.viridian_time),
            (MorphHardfork::Emerald, hardfork_info.emerald_time),
        ]
        .into_iter()
        .filter_map(|(fork, time)| time.map(|t| (fork, ForkCondition::Timestamp(t))));

        let morph_forks = block_forks.chain(time_forks);

        base_spec.hardforks.extend(morph_forks);

        Self {
            inner: base_spec,
            info: chain_info,
        }
    }
}

impl MorphChainSpec {
    pub fn set_hardfork(&mut self, hardfork: MorphHardfork, time: u64) {
        self.inner
            .hardforks
            .insert(hardfork, ForkCondition::Timestamp(time));
    }
}

// =============================================================================
// Trait Implementations
// =============================================================================

impl Hardforks for MorphChainSpec {
    fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        self.inner.fork(fork)
    }

    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.inner.forks_iter()
    }

    fn fork_id(&self, head: &Head) -> ForkId {
        self.inner.fork_id(head)
    }

    fn latest_fork_id(&self) -> ForkId {
        self.inner.latest_fork_id()
    }

    fn fork_filter(&self, head: Head) -> ForkFilter {
        self.inner.fork_filter(head)
    }
}

impl EthChainSpec for MorphChainSpec {
    type Header = Header;

    fn chain(&self) -> Chain {
        self.inner.chain()
    }

    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.inner.base_fee_params_at_timestamp(timestamp)
    }

    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        self.inner.blob_params_at_timestamp(timestamp)
    }

    fn deposit_contract(&self) -> Option<&DepositContract> {
        self.inner.deposit_contract()
    }

    fn genesis_hash(&self) -> B256 {
        self.inner.genesis_hash()
    }

    fn prune_delete_limit(&self) -> usize {
        self.inner.prune_delete_limit()
    }

    fn display_hardforks(&self) -> Box<dyn std::fmt::Display> {
        // filter only morph hardforks
        let morph_forks = self.inner.hardforks.forks_iter().filter(|(fork, _)| {
            !EthereumHardfork::VARIANTS
                .iter()
                .any(|h| h.name() == (*fork).name())
        });

        Box::new(DisplayHardforks::new(morph_forks))
    }

    fn genesis_header(&self) -> &Self::Header {
        self.inner.genesis_header()
    }

    fn genesis(&self) -> &Genesis {
        self.inner.genesis()
    }

    fn bootnodes(&self) -> Option<Vec<NodeRecord>> {
        self.inner.bootnodes()
    }

    fn final_paris_total_difficulty(&self) -> Option<U256> {
        self.inner.get_final_paris_total_difficulty()
    }

    fn next_block_base_fee(&self, _parent: &Header, _target_timestamp: u64) -> Option<u64> {
        Some(MORPH_BASE_FEE)
    }
}

impl EthereumHardforks for MorphChainSpec {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.inner.ethereum_fork_activation(fork)
    }
}

impl EthExecutorSpec for MorphChainSpec {
    fn deposit_contract_address(&self) -> Option<Address> {
        self.inner.deposit_contract_address()
    }
}

impl MorphHardforks for MorphChainSpec {
    fn morph_fork_activation(&self, fork: MorphHardfork) -> ForkCondition {
        self.fork(fork)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardfork::MorphHardforks;
    use alloy_primitives::address;
    use serde_json::json;

    /// Helper function to create a test genesis with Morph hardforks at genesis
    fn create_test_genesis() -> Genesis {
        let genesis_json = json!({
            "config": {
                "chainId": 1337,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "mergeNetsplitBlock": 0,
                "terminalTotalDifficulty": 0,
                "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0,
                "cancunTime": 0,
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph203Time": 0,
                "viridianTime": 0,
                "emeraldTime": 0,
                "morph": {
                    "feeVaultAddress": "0x530000000000000000000000000000000000000a"
                }
            },
            "alloc": {}
        });
        serde_json::from_value(genesis_json).expect("genesis should be valid")
    }

    #[test]
    fn test_morph_chainspec_has_morph_hardforks() {
        let chainspec = MorphChainSpec::from(create_test_genesis());

        // Block-based hardforks should be active at block 0
        assert!(chainspec.is_bernoulli_active_at_block(0));
        assert!(chainspec.is_curie_active_at_block(0));
        // Timestamp-based hardforks should be active at timestamp 0
        assert!(chainspec.is_emerald_active_at_timestamp(0));
    }

    #[test]
    fn test_morph_chainspec_implements_morph_hardforks_trait() {
        let chainspec = MorphChainSpec::from(create_test_genesis());

        // Should be able to query Morph hardfork activation through trait
        let activation = chainspec.morph_fork_activation(MorphHardfork::Bernoulli);
        assert_eq!(activation, ForkCondition::Block(0));

        // Should be able to use convenience method through trait
        assert!(chainspec.is_bernoulli_active_at_block(0));
        assert!(chainspec.is_bernoulli_active_at_block(1000));
    }

    #[test]
    fn test_morph_hardforks_in_inner_hardforks() {
        let chainspec = MorphChainSpec::from(create_test_genesis());

        // Morph hardforks should be queryable from inner.hardforks via Hardforks trait
        let activation = chainspec.fork(MorphHardfork::Bernoulli);
        assert_eq!(activation, ForkCondition::Block(0));

        // Verify Bernoulli appears in forks iterator
        let has_bernoulli = chainspec
            .forks_iter()
            .any(|(fork, _)| fork.name() == "Bernoulli");
        assert!(
            has_bernoulli,
            "Bernoulli hardfork should be in inner.hardforks"
        );
    }

    #[test]
    fn test_parse_morph_hardforks_from_genesis_extra_fields() {
        let genesis_json = json!({
            "config": {
                "chainId": 1337,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "mergeNetsplitBlock": 0,
                "terminalTotalDifficulty": 0,
                "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0,
                "cancunTime": 0,
                "bernoulliBlock": 100,
                "curieBlock": 200,
                "morph203Time": 3000,
                "viridianTime": 4000,
                "emeraldTime": 5000,
                "morph": {}
            },
            "alloc": {}
        });

        let genesis: Genesis =
            serde_json::from_value(genesis_json).expect("genesis should be valid");
        let chainspec = MorphChainSpec::from(genesis);

        // Test Bernoulli activation (block-based)
        let activation = chainspec.fork(MorphHardfork::Bernoulli);
        assert_eq!(activation, ForkCondition::Block(100));

        assert!(!chainspec.is_bernoulli_active_at_block(0));
        assert!(chainspec.is_bernoulli_active_at_block(100));
        assert!(chainspec.is_bernoulli_active_at_block(200));

        // Test Curie activation (block-based)
        let activation = chainspec.fork(MorphHardfork::Curie);
        assert_eq!(activation, ForkCondition::Block(200));

        assert!(!chainspec.is_curie_active_at_block(0));
        assert!(!chainspec.is_curie_active_at_block(100));
        assert!(chainspec.is_curie_active_at_block(200));

        // Test Emerald activation (timestamp-based)
        let activation = chainspec.fork(MorphHardfork::Emerald);
        assert_eq!(activation, ForkCondition::Timestamp(5000));

        assert!(!chainspec.is_emerald_active_at_timestamp(4000));
        assert!(chainspec.is_emerald_active_at_timestamp(5000));
    }

    #[test]
    fn test_morph_hardfork_at() {
        let genesis_json = json!({
            "config": {
                "chainId": 1337,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "mergeNetsplitBlock": 0,
                "terminalTotalDifficulty": 0,
                "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0,
                "cancunTime": 0,
                "bernoulliBlock": 100,
                "curieBlock": 200,
                "morph203Time": 3000,
                "viridianTime": 4000,
                "emeraldTime": 5000,
                "morph": {}
            },
            "alloc": {}
        });

        let genesis: Genesis =
            serde_json::from_value(genesis_json).expect("genesis should be valid");
        let chainspec = MorphChainSpec::from(genesis);

        // Before Bernoulli activation (block 0, timestamp 0) - should return Bernoulli (baseline)
        assert_eq!(chainspec.morph_hardfork_at(0, 0), MorphHardfork::Bernoulli);

        // At Bernoulli block (block 100, timestamp 0)
        assert_eq!(
            chainspec.morph_hardfork_at(100, 0),
            MorphHardfork::Bernoulli
        );

        // At Curie block (block 200, timestamp 0)
        assert_eq!(chainspec.morph_hardfork_at(200, 0), MorphHardfork::Curie);

        // At Morph203 time (block 300, timestamp 3000)
        assert_eq!(
            chainspec.morph_hardfork_at(300, 3000),
            MorphHardfork::Morph203
        );

        // At Viridian time (block 400, timestamp 4000)
        assert_eq!(
            chainspec.morph_hardfork_at(400, 4000),
            MorphHardfork::Viridian
        );

        // At Emerald time (block 500, timestamp 5000)
        assert_eq!(
            chainspec.morph_hardfork_at(500, 5000),
            MorphHardfork::Emerald
        );

        // After Emerald (block 600, timestamp 6000)
        assert_eq!(
            chainspec.morph_hardfork_at(600, 6000),
            MorphHardfork::Emerald
        );
    }

    #[test]
    fn test_chainspec_from_genesis() {
        let genesis_json = json!({
            "config": {
                "chainId": 1337,
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph203Time": 0,
                "viridianTime": 0,
                "emeraldTime": 0,
                "morph": {
                    "feeVaultAddress": "0x530000000000000000000000000000000000000a",
                    "maxTxPayloadBytesPerBlock": 122880
                }
            },
            "alloc": {}
        });
        let genesis: Genesis = serde_json::from_value(genesis_json).unwrap();

        let chainspec = MorphChainSpec::from(genesis);

        // Block-based hardforks should be active at block 0
        assert!(chainspec.is_bernoulli_active_at_block(0));
        assert!(chainspec.is_curie_active_at_block(0));
        // Timestamp-based hardforks should be active at timestamp 0
        assert!(chainspec.is_morph203_active_at_timestamp(0));
        assert!(chainspec.is_viridian_active_at_timestamp(0));
        assert!(chainspec.is_emerald_active_at_timestamp(0));

        // Config should be extracted from genesis
        assert!(chainspec.is_fee_vault_enabled());
    }

    #[test]
    fn test_parse_morph_chain_info() {
        let genesis_json = json!({
            "config": {
                "chainId": 1337,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph": {
                    "feeVaultAddress": "0x530000000000000000000000000000000000000a",
                    "maxTxPayloadBytesPerBlock": 122880
                }
            },
            "alloc": {}
        });

        let genesis: Genesis = serde_json::from_value(genesis_json).unwrap();
        let chainspec = MorphChainSpec::from(genesis);

        assert!(chainspec.is_fee_vault_enabled());
        assert_eq!(chainspec.max_tx_payload_bytes_per_block(), Some(122880));
        assert!(chainspec.is_valid_block_size(100000));
        assert!(!chainspec.is_valid_block_size(200000));
    }

    #[test]
    fn test_chain_config_trait() {
        let genesis = create_test_genesis();
        let chainspec = MorphChainSpec::from(genesis);

        let config = chainspec.chain_config();
        // Test genesis includes morph config with fee vault address
        assert!(config.is_fee_vault_enabled());
    }

    #[test]
    fn test_chain_config_with_fee_vault() {
        let genesis_json = json!({
            "config": {
                "chainId": 1337,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "mergeNetsplitBlock": 0,
                "terminalTotalDifficulty": 0,
                "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0,
                "cancunTime": 0,
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph203Time": 0,
                "viridianTime": 0,
                "emeraldTime": 0,
                "morph": {
                    "feeVaultAddress": "0x530000000000000000000000000000000000000a",
                    "maxTxPayloadBytesPerBlock": 122880
                }
            },
            "alloc": {}
        });
        let genesis: Genesis =
            serde_json::from_value(genesis_json).expect("genesis should be valid");
        let chainspec = MorphChainSpec::from(genesis);

        let config = chainspec.chain_config();
        assert!(config.is_fee_vault_enabled());
        assert_eq!(
            config.fee_vault_address,
            Some(address!("530000000000000000000000000000000000000a"))
        );
    }
}
