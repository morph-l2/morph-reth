//! Morph chain specification.

use crate::{
    MORPH_BASE_FEE,
    genesis::{MorphChainConfig, MorphGenesisInfo},
    hardfork::{MorphHardfork, MorphHardforks},
};
use alloy_chains::Chain;
use alloy_consensus::Sealable;
use alloy_eips::eip7840::BlobParams;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, U256};
use morph_primitives::MorphHeader;
use reth_chainspec::{
    BaseFeeParams, ChainHardforks, ChainSpec, DepositContract, DisplayHardforks, EthChainSpec,
    EthereumHardfork, EthereumHardforks, ForkCondition, ForkFilter, ForkId, Hardfork, Hardforks,
    Head,
};
use reth_network_peers::NodeRecord;
use reth_primitives_traits::SealedHeader;

#[cfg(feature = "cli")]
use crate::{morph::MORPH_MAINNET, morph_hoodi::MORPH_HOODI};
#[cfg(feature = "cli")]
use std::sync::Arc;

// =============================================================================
// Genesis Helper Functions & Error Types
// =============================================================================

/// Errors returned when constructing a [`MorphChainSpec`] from a [`Genesis`].
#[derive(Debug)]
pub enum MorphChainSpecError {
    /// Failed to deserialize Morph-specific fields from `genesis.config.extra_fields`.
    InvalidGenesisInfo(serde_json::Error),
    /// [`GenesisConfig::state_root`] was specified without a matching
    /// [`GenesisConfig::genesis_hash`].
    MissingGenesisHash,
}

impl std::fmt::Display for MorphChainSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidGenesisInfo(err) => {
                write!(f, "invalid Morph genesis configuration: {err}")
            }
            Self::MissingGenesisHash => {
                write!(f, "genesis_hash is required when state_root is provided")
            }
        }
    }
}

impl std::error::Error for MorphChainSpecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidGenesisInfo(err) => Some(err),
            Self::MissingGenesisHash => None,
        }
    }
}

impl From<serde_json::Error> for MorphChainSpecError {
    fn from(err: serde_json::Error) -> Self {
        Self::InvalidGenesisInfo(err)
    }
}

/// Configuration for building a genesis header.
///
/// This allows customization of the genesis header construction,
/// particularly for networks that use ZK-trie state roots instead of MPT.
#[derive(Default)]
pub struct GenesisConfig {
    /// Custom state root to use (e.g., ZK-trie state root).
    ///
    /// If `None`, the state root will be computed from the genesis alloc using MPT.
    pub state_root: Option<B256>,
    /// Genesis block hash.
    ///
    /// Required when `state_root` is provided.
    pub genesis_hash: Option<B256>,
}

impl GenesisConfig {
    /// Create a configuration with custom state root and genesis hash.
    ///
    /// This is used for predefined networks (mainnet, testnet) that use ZK-trie
    /// state roots before the Jade hardfork.
    pub fn with_state_root(mut self, state_root: B256, genesis_hash: B256) -> Self {
        self.state_root = Some(state_root);
        self.genesis_hash = Some(genesis_hash);
        self
    }
}

/// Extend base Ethereum hardforks with Morph hardforks from genesis.
///
/// This is the unified hardfork construction logic, eliminating code duplication
/// between different construction paths (CLI genesis parsing vs predefined networks).
///
/// # Arguments
/// * `hardforks` - Base Ethereum hardforks parsed from genesis
/// * `chain_info` - Morph-specific genesis information extracted from genesis config
///
/// # Returns
/// A `ChainHardforks` containing both Ethereum and Morph hardforks
fn build_hardforks(mut hardforks: ChainHardforks, chain_info: &MorphGenesisInfo) -> ChainHardforks {
    let hardfork_info = chain_info
        .hard_fork_info
        .as_ref()
        .cloned()
        .unwrap_or_default();

    // Morph block-based hardforks (Bernoulli, Curie)
    let block_forks = vec![
        (MorphHardfork::Bernoulli, hardfork_info.bernoulli_block),
        (MorphHardfork::Curie, hardfork_info.curie_block),
    ]
    .into_iter()
    .filter_map(|(fork, block)| block.map(|b| (fork, ForkCondition::Block(b))));

    // Morph timestamp-based hardforks (Morph203, Viridian, Emerald, Jade, Celadon)
    let time_forks = vec![
        (MorphHardfork::Morph203, hardfork_info.morph203_time),
        (MorphHardfork::Viridian, hardfork_info.viridian_time),
        (MorphHardfork::Emerald, hardfork_info.emerald_time),
        (MorphHardfork::Jade, hardfork_info.jade_fork_time),
        (MorphHardfork::Celadon, hardfork_info.celadon_time),
    ]
    .into_iter()
    .filter_map(|(fork, time)| time.map(|t| (fork, ForkCondition::Timestamp(t))));

    // Merge all Morph hardforks into the base hardforks
    hardforks.extend(block_forks.chain(time_forks));

    // Activate Prague at Viridian time to align with go-ethereum's
    // EIP-7702 (SetCode tx) activation point.
    if let Some(viridian_time) = hardfork_info.viridian_time {
        hardforks.insert(
            EthereumHardfork::Prague,
            ForkCondition::Timestamp(viridian_time),
        );
    }

    hardforks
}

// =============================================================================
// Chain Specification Parser (CLI)
// =============================================================================

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
        _ => Arc::new(MorphChainSpec::try_from_genesis(
            reth_cli::chainspec::parse_genesis(s)?,
        )?),
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
    pub inner: ChainSpec<MorphHeader>,
    /// Morph-specific genesis info.
    pub info: MorphGenesisInfo,
}

impl MorphChainSpec {
    /// Create a new [`MorphChainSpec`] with the given inner spec and config.
    pub fn new(inner: ChainSpec<MorphHeader>, info: MorphGenesisInfo) -> Self {
        Self { inner, info }
    }

    /// Fallibly build a [`MorphChainSpec`] from a genesis configuration.
    ///
    /// Automatically computes the MPT state root from the genesis alloc.
    /// Suitable for CLI scenarios where users provide custom `genesis.json` files.
    pub fn try_from_genesis(genesis: Genesis) -> Result<Self, MorphChainSpecError> {
        Self::try_from_genesis_with_config(genesis, GenesisConfig::default())
    }

    /// Fallibly build a [`MorphChainSpec`] from a genesis configuration with custom settings.
    ///
    /// Preserves all fields parsed by [`ChainSpec::from_genesis`] (such as
    /// `paris_block_and_final_difficulty`, `base_fee_params`, `blob_params`,
    /// `deposit_contract`, and `prune_delete_limit`) while layering Morph's hardforks
    /// and genesis header defaults on top.
    ///
    /// # Errors
    /// Returns [`MorphChainSpecError::InvalidGenesisInfo`] if the Morph genesis fields are
    /// missing or malformed, or [`MorphChainSpecError::MissingGenesisHash`] if
    /// `config.state_root` is provided without `config.genesis_hash`.
    pub fn try_from_genesis_with_config(
        genesis: Genesis,
        config: GenesisConfig,
    ) -> Result<Self, MorphChainSpecError> {
        let chain_info = MorphGenesisInfo::try_from(&genesis.config.extra_fields)?;
        if config.state_root.is_some() && config.genesis_hash.is_none() {
            return Err(MorphChainSpecError::MissingGenesisHash);
        }

        let base_spec = ChainSpec::from_genesis(genesis);
        Self::from_base_spec_with_info(base_spec, chain_info, config)
    }

    fn from_base_spec_with_info(
        base_spec: ChainSpec,
        chain_info: MorphGenesisInfo,
        config: GenesisConfig,
    ) -> Result<Self, MorphChainSpecError> {
        let ChainSpec {
            chain,
            genesis,
            genesis_header: base_genesis_header,
            paris_block_and_final_difficulty,
            hardforks: base_hardforks,
            deposit_contract,
            base_fee_params,
            prune_delete_limit,
            blob_params,
        } = base_spec;

        // Extend Ethereum hardforks with Morph hardforks (and Prague at Viridian time).
        let hardforks = build_hardforks(base_hardforks, &chain_info);

        // Apply Morph's genesis base fee default and optional custom state root.
        let mut header = base_genesis_header.unseal();
        // morph-geth defaults to MORPH_BASE_FEE (1_000_000) when baseFeePerGas is
        // absent, not Ethereum's INITIAL_BASE_FEE (1_000_000_000). Apply the same
        // default so the genesis block header hash matches.
        if genesis.base_fee_per_gas.is_none() {
            header.base_fee_per_gas = Some(MORPH_BASE_FEE);
        }

        let genesis_header = match config.state_root {
            Some(state_root) => {
                // Use custom state root (for predefined networks with ZK-trie)
                let genesis_hash = config
                    .genesis_hash
                    .ok_or(MorphChainSpecError::MissingGenesisHash)?;
                header.state_root = state_root;
                SealedHeader::new(MorphHeader::from(header), genesis_hash)
            }
            None => {
                // Use MPT state root computed from alloc (for CLI-provided genesis)
                let header = MorphHeader::from(header);
                // Recompute genesis hash with the corrected base fee
                let hash = header.hash_slow();
                SealedHeader::new(header, hash)
            }
        };

        Ok(Self {
            inner: ChainSpec {
                chain,
                genesis_header,
                genesis,
                paris_block_and_final_difficulty,
                hardforks,
                deposit_contract,
                base_fee_params,
                prune_delete_limit,
                blob_params,
            },
            info: chain_info,
        })
    }

    /// Build a [`MorphChainSpec`] from a genesis configuration.
    ///
    /// This is the default method for constructing a chain spec from genesis.
    /// It automatically computes the MPT state root from the genesis alloc.
    ///
    /// # Panics
    /// Panics if the Morph genesis fields are missing or malformed. Use
    /// [`Self::try_from_genesis`] for fallible construction.
    pub fn from_genesis(genesis: Genesis) -> Self {
        Self::try_from_genesis(genesis).expect("failed to build morph chain spec from genesis")
    }

    /// Build a [`MorphChainSpec`] from a genesis configuration with custom settings.
    ///
    /// This is an advanced method that allows customizing the genesis header construction,
    /// particularly for predefined networks that use ZK-trie state roots instead of MPT.
    ///
    /// # Arguments
    /// * `genesis` - Genesis configuration
    /// * `config` - Genesis header configuration (state root, genesis hash)
    ///
    /// # Panics
    /// Panics if the Morph genesis fields are missing or malformed, or if
    /// `config.state_root` is provided without `config.genesis_hash`. Use
    /// [`Self::try_from_genesis_with_config`] for fallible construction.
    ///
    /// # Example
    /// ```no_run
    /// # use morph_chainspec::{MorphChainSpec, spec::GenesisConfig};
    /// # use alloy_genesis::Genesis;
    /// # use alloy_primitives::b256;
    /// # let genesis = Genesis::default();
    /// let config = GenesisConfig::default()
    ///     .with_state_root(
    ///         b256!("0000000000000000000000000000000000000000000000000000000000000000"),
    ///         b256!("0000000000000000000000000000000000000000000000000000000000000000"),
    ///     );
    /// let spec = MorphChainSpec::from_genesis_with_config(genesis, config);
    /// ```
    pub fn from_genesis_with_config(genesis: Genesis, config: GenesisConfig) -> Self {
        Self::try_from_genesis_with_config(genesis, config)
            .expect("failed to build morph chain spec from genesis")
    }

    /// Returns whether the fee vault is enabled.
    pub fn is_fee_vault_enabled(&self) -> bool {
        self.info.morph_chain_info.is_fee_vault_enabled()
    }

    /// Returns the fee vault address.
    pub fn fee_vault_address(&self) -> Option<Address> {
        self.info.morph_chain_info.fee_vault_address
    }

    /// Returns the maximum accepted L2 transaction payload bytes per block.
    pub const fn max_tx_payload_bytes_per_block(&self) -> u64 {
        self.info.morph_chain_info.max_tx_payload_bytes_per_block()
    }
}

impl From<ChainSpec> for MorphChainSpec {
    fn from(value: ChainSpec) -> Self {
        let chain_info = MorphGenesisInfo::try_from(&value.genesis.config.extra_fields)
            .expect("failed to extract morph genesis info");
        Self::from_base_spec_with_info(value, chain_info, GenesisConfig::default())
            .expect("failed to build morph chain spec from chain spec")
    }
}

impl From<Genesis> for MorphChainSpec {
    fn from(genesis: Genesis) -> Self {
        // Delegate to the unified construction method
        Self::from_genesis(genesis)
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
    type Header = MorphHeader;

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
        match self.inner.chain.id() {
            crate::MORPH_MAINNET_CHAIN_ID => Some(crate::bootnodes::morph_mainnet_nodes()),
            crate::MORPH_HOODI_CHAIN_ID => Some(crate::bootnodes::morph_hoodi_nodes()),
            _ => self.inner.bootnodes(),
        }
    }

    fn final_paris_total_difficulty(&self) -> Option<U256> {
        self.inner.get_final_paris_total_difficulty()
    }

    fn next_block_base_fee(&self, _parent: &MorphHeader, _target_timestamp: u64) -> Option<u64> {
        Some(MORPH_BASE_FEE)
    }
}

impl EthereumHardforks for MorphChainSpec {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.inner.ethereum_fork_activation(fork)
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
    fn test_celadon_activation_from_genesis() {
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
                "jadeForkTime": 6000,
                "celadonTime": 7000,
                "morph": {}
            },
            "alloc": {}
        });

        let genesis: Genesis =
            serde_json::from_value(genesis_json).expect("genesis should be valid");
        let chainspec = MorphChainSpec::from(genesis);

        assert_eq!(
            chainspec.fork(MorphHardfork::Celadon),
            ForkCondition::Timestamp(7000)
        );
        assert!(!chainspec.is_celadon_active_at_timestamp(6999));
        assert!(chainspec.is_celadon_active_at_timestamp(7000));

        // Celadon must be reported as the latest fork once active, and must not
        // shadow Jade before its own activation.
        assert_eq!(chainspec.morph_hardfork_at(1, 6000), MorphHardfork::Jade);
        assert_eq!(chainspec.morph_hardfork_at(1, 7000), MorphHardfork::Celadon);
    }

    #[test]
    fn test_celadon_absent_from_genesis_never_activates() {
        // A genesis without celadonTime must leave Celadon unscheduled.
        let chainspec = MorphChainSpec::from(create_test_genesis());
        assert!(!chainspec.is_celadon_active_at_timestamp(0));
        assert!(!chainspec.is_celadon_active_at_timestamp(u64::MAX));
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
        assert_eq!(
            chainspec.fee_vault_address(),
            Some(address!("530000000000000000000000000000000000000a"))
        );
        assert_eq!(chainspec.max_tx_payload_bytes_per_block(), 122_880);
    }

    #[test]
    fn test_chain_config_trait() {
        let genesis = create_test_genesis();
        let chainspec = MorphChainSpec::from(genesis);

        let config = chainspec.chain_config();
        // Test genesis includes morph config with fee vault address
        assert!(config.is_fee_vault_enabled());
        assert_eq!(
            config.max_tx_payload_bytes_per_block(),
            crate::MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK
        );
    }

    #[test]
    fn test_prague_activated_with_viridian() {
        // Prague should be activated at the same time as Viridian
        // This enables EIP-7702 in the transaction pool validator
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
                "morph203Time": 1000,
                "viridianTime": 2000,
                "emeraldTime": 3000,
                "morph": {}
            },
            "alloc": {}
        });

        let genesis: Genesis =
            serde_json::from_value(genesis_json).expect("genesis should be valid");
        let chainspec = MorphChainSpec::from(genesis);

        // Prague should not be active before Viridian
        assert!(!chainspec.is_prague_active_at_timestamp(1999));

        // Prague should be active at Viridian time
        assert!(chainspec.is_prague_active_at_timestamp(2000));

        // Prague should remain active after Viridian
        assert!(chainspec.is_prague_active_at_timestamp(5000));

        // Verify the fork condition is set correctly
        let prague_activation = chainspec.ethereum_fork_activation(EthereumHardfork::Prague);
        assert_eq!(prague_activation, ForkCondition::Timestamp(2000));
    }

    #[test]
    fn test_prague_not_activated_without_viridian() {
        // If Viridian is not configured, Prague should not be activated
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
                "morph203Time": 1000,
                "morph": {}
            },
            "alloc": {}
        });

        let genesis: Genesis =
            serde_json::from_value(genesis_json).expect("genesis should be valid");
        let chainspec = MorphChainSpec::from(genesis);

        // Prague should not be active since Viridian is not configured
        assert!(!chainspec.is_prague_active_at_timestamp(0));
        assert!(!chainspec.is_prague_active_at_timestamp(5000));
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
        assert_eq!(config.max_tx_payload_bytes_per_block(), 122_880);
    }

    #[test]
    fn test_try_from_genesis_rejects_malformed_hardfork_and_morph_config() {
        // Malformed Morph hardfork field (string instead of u64) must return Err.
        let malformed_fork: Genesis = serde_json::from_value(json!({
            "config": {
                "chainId": 1337,
                "celadonTime": "1750000000",
                "morph": {}
            },
            "alloc": {}
        }))
        .unwrap();
        let err = MorphChainSpec::try_from_genesis(malformed_fork)
            .expect_err("malformed celadonTime must fail try_from_genesis");
        assert!(matches!(err, MorphChainSpecError::InvalidGenesisInfo(_)));
        assert!(
            !err.to_string().contains("missing field `morph`"),
            "unexpected error: {err}"
        );

        // Missing `morph` object must return Err mentioning missing field `morph`.
        let missing_morph: Genesis = serde_json::from_value(json!({
            "config": {
                "chainId": 1337
            },
            "alloc": {}
        }))
        .unwrap();
        let err = MorphChainSpec::try_from_genesis(missing_morph)
            .expect_err("missing morph section must fail try_from_genesis");
        assert!(matches!(err, MorphChainSpecError::InvalidGenesisInfo(_)));
        assert!(
            err.to_string().contains("missing field `morph`"),
            "expected missing field error, got: {err}"
        );

        // Malformed `morph` subfield must preserve inner serde error (not `missing field 'morph'`).
        let malformed_morph: Genesis = serde_json::from_value(json!({
            "config": {
                "chainId": 1337,
                "morph": {
                    "feeVaultAddress": "not-an-address"
                }
            },
            "alloc": {}
        }))
        .unwrap();
        let err = MorphChainSpec::try_from_genesis(malformed_morph)
            .expect_err("malformed feeVaultAddress must fail try_from_genesis");
        assert!(matches!(err, MorphChainSpecError::InvalidGenesisInfo(_)));
        assert!(
            !err.to_string().contains("missing field `morph`"),
            "inner serde error must not be masked: {err}"
        );

        // Custom state_root without genesis_hash must return MissingGenesisHash without panicking.
        let valid_empty_forks: Genesis = serde_json::from_value(json!({
            "config": {
                "chainId": 1337,
                "morph": {}
            },
            "alloc": {}
        }))
        .unwrap();
        let err = MorphChainSpec::try_from_genesis_with_config(
            valid_empty_forks.clone(),
            GenesisConfig {
                state_root: Some(B256::ZERO),
                genesis_hash: None,
            },
        )
        .expect_err("state_root without genesis_hash must fail");
        assert!(matches!(err, MorphChainSpecError::MissingGenesisHash));

        // Empty Morph hardfork fields still succeed.
        let spec = MorphChainSpec::try_from_genesis(valid_empty_forks)
            .expect("empty Morph hardfork fields must succeed");
        assert_eq!(spec.chain().id(), 1337);
        assert_eq!(
            spec.genesis_header().inner.base_fee_per_gas,
            Some(MORPH_BASE_FEE)
        );
    }

    #[test]
    fn test_from_genesis_preserves_chainspec_fields() {
        let deposit_addr = address!("1111111111111111111111111111111111111111");
        let genesis: Genesis = serde_json::from_value(json!({
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
                "terminalTotalDifficulty": 42,
                "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0,
                "cancunTime": 0,
                "depositContractAddress": "0x1111111111111111111111111111111111111111",
                "blobSchedule": {
                    "cancun": {
                        "target": 4,
                        "max": 8,
                        "baseFeeUpdateFraction": 3338477
                    }
                },
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph": {}
            },
            "alloc": {}
        }))
        .expect("genesis should be valid");

        let base_spec = ChainSpec::from_genesis(genesis.clone());
        let morph_spec = MorphChainSpec::try_from_genesis(genesis).unwrap();

        assert_eq!(
            morph_spec.inner.paris_block_and_final_difficulty,
            Some((0, U256::from(42)))
        );
        assert_eq!(
            morph_spec.final_paris_total_difficulty(),
            Some(U256::from(42))
        );
        assert_eq!(
            morph_spec.deposit_contract().map(|c| c.address),
            Some(deposit_addr)
        );
        let blob_params = morph_spec
            .blob_params_at_timestamp(0)
            .expect("cancun blob params should be preserved");
        assert_eq!(blob_params.target_blob_count, 4);
        assert_eq!(blob_params.max_blob_count, 8);
        assert_eq!(blob_params.update_fraction, 3_338_477);
        assert_eq!(morph_spec.inner.base_fee_params, base_spec.base_fee_params);
        assert_eq!(
            morph_spec.prune_delete_limit(),
            base_spec.prune_delete_limit
        );

        // From<ChainSpec> must also preserve explicit ChainSpec overrides.
        let mut custom_base = base_spec;
        let custom_base_fee_params =
            reth_chainspec::BaseFeeParamsKind::Constant(BaseFeeParams::new(42, 7));
        custom_base.base_fee_params = custom_base_fee_params.clone();
        custom_base.prune_delete_limit = 12_345;
        let converted = MorphChainSpec::from(custom_base);
        assert_eq!(converted.inner.base_fee_params, custom_base_fee_params);
        assert_eq!(converted.prune_delete_limit(), 12_345);
    }

    #[cfg(feature = "cli")]
    #[test]
    fn test_chain_value_parser_handles_invalid_and_valid_custom_genesis() {
        // Malformed hardfork field returns Err instead of panicking.
        let malformed_fork = json!({
            "config": {
                "chainId": 1337,
                "celadonTime": "1750000000",
                "morph": {}
            },
            "alloc": {}
        })
        .to_string();
        let err = chain_value_parser(&malformed_fork)
            .expect_err("malformed hardfork in custom genesis must return Err");
        assert!(
            !err.to_string().contains("missing field `morph`"),
            "unexpected error: {err}"
        );

        // Missing `morph` section returns Err instead of panicking.
        let missing_morph = json!({
            "config": {
                "chainId": 1337
            },
            "alloc": {}
        })
        .to_string();
        let err = chain_value_parser(&missing_morph)
            .expect_err("missing morph config in custom genesis must return Err");
        assert!(
            err.to_string().contains("missing field `morph`"),
            "expected missing field error, got: {err}"
        );

        // Malformed `morph` subfield preserves inner serde error instead of panicking.
        let malformed_morph = json!({
            "config": {
                "chainId": 1337,
                "morph": {
                    "maxTxPayloadBytesPerBlock": "not-a-number"
                }
            },
            "alloc": {}
        })
        .to_string();
        let err = chain_value_parser(&malformed_morph)
            .expect_err("malformed morph subfield must return Err");
        assert!(
            !err.to_string().contains("missing field `morph`"),
            "inner serde error must not be replaced: {err}"
        );

        // Valid custom genesis with empty hardfork fields succeeds.
        let valid_custom = json!({
            "config": {
                "chainId": 1337,
                "morph": {}
            },
            "alloc": {}
        })
        .to_string();
        let parsed = chain_value_parser(&valid_custom)
            .expect("valid custom genesis must parse without error");
        assert_eq!(parsed.chain().id(), 1337);
        assert_eq!(
            parsed.genesis_header().inner.base_fee_per_gas,
            Some(MORPH_BASE_FEE)
        );

        // Predefined networks remain intact with expected genesis hash and base fee default.
        let mainnet = chain_value_parser("mainnet").unwrap();
        assert_eq!(mainnet.genesis_hash(), crate::MORPH_MAINNET_GENESIS_HASH);
        assert_eq!(
            mainnet.genesis_header().hash_slow(),
            crate::MORPH_MAINNET_GENESIS_HASH
        );
        assert_eq!(
            mainnet.genesis_header().inner.base_fee_per_gas,
            Some(MORPH_BASE_FEE)
        );

        let hoodi = chain_value_parser("hoodi").unwrap();
        assert_eq!(hoodi.genesis_hash(), crate::MORPH_HOODI_GENESIS_HASH);
        assert_eq!(
            hoodi.genesis_header().hash_slow(),
            crate::MORPH_HOODI_GENESIS_HASH
        );
        assert_eq!(
            hoodi.genesis_header().inner.base_fee_per_gas,
            Some(MORPH_BASE_FEE)
        );
    }
}
