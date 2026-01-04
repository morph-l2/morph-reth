use crate::hardfork::{MorphHardfork, MorphHardforks};
use alloy_consensus::Header;
use alloy_eips::eip7840::BlobParams;
use alloy_evm::eth::spec::EthExecutorSpec;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, U256};
use reth_chainspec::{
    BaseFeeParams, Chain, ChainSpec, DepositContract, DisplayHardforks, EthChainSpec,
    EthereumHardfork, EthereumHardforks, ForkCondition, ForkFilter, ForkId, Hardfork, Hardforks,
    Head,
};
use reth_network_peers::NodeRecord;
use std::sync::Arc;

pub const MORPH_BASE_FEE: u64 = 10_000_000_000;

/// Morph genesis info extracted from genesis extra_fields
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphGenesisInfo {
    /// Timestamp of Bernoulli hardfork activation
    #[serde(skip_serializing_if = "Option::is_none")]
    bernoulli_time: Option<u64>,

    /// Timestamp of Andantino hardfork activation
    #[serde(skip_serializing_if = "Option::is_none")]
    curie_time: Option<u64>,

    /// Timestamp of Morph203 hardfork activation
    #[serde(skip_serializing_if = "Option::is_none")]
    morph203_time: Option<u64>,

    /// Timestamp of viridian hardfork activation
    #[serde(skip_serializing_if = "Option::is_none")]
    viridian_time: Option<u64>,

    /// The epoch length used by consensus.
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch_length: Option<u64>,
}

impl MorphGenesisInfo {
    /// Extract Morph genesis info from genesis extra_fields
    fn extract_from(genesis: &Genesis) -> Self {
        genesis
            .config
            .extra_fields
            .deserialize_as::<Self>()
            .unwrap_or_default()
    }

    pub fn epoch_length(&self) -> Option<u64> {
        self.epoch_length
    }
}

/// Morph chain specification parser.
#[derive(Debug, Clone, Default)]
pub struct MorphChainSpecParser;

/// Chains supported by Morph. First value should be used as the default.
pub const SUPPORTED_CHAINS: &[&str] = &["testnet"];

/// Clap value parser for [`ChainSpec`]s.
///
/// The value parser matches either a known chain, the path
/// to a json file, or a json formatted string in-memory. The json needs to be a Genesis struct.
#[cfg(feature = "cli")]
pub fn chain_value_parser(s: &str) -> eyre::Result<Arc<MorphChainSpec>> {
    Ok(MorphChainSpec::from_genesis(reth_cli::chainspec::parse_genesis(s)?).into())
}

#[cfg(feature = "cli")]
impl reth_cli::chainspec::ChainSpecParser for MorphChainSpecParser {
    type ChainSpec = MorphChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<Self::ChainSpec>> {
        chain_value_parser(s)
    }
}

/// Morph chain spec type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MorphChainSpec {
    /// [`ChainSpec`].
    pub inner: ChainSpec<Header>,
    pub info: MorphGenesisInfo,
}

impl MorphChainSpec {
    /// Converts the given [`Genesis`] into a [`MorphChainSpec`].
    pub fn from_genesis(genesis: Genesis) -> Self {
        // Extract Morph genesis info from extra_fields
        let info @ MorphGenesisInfo {
            bernoulli_time,
            curie_time,
            morph203_time,
            viridian_time,
            ..
        } = MorphGenesisInfo::extract_from(&genesis);

        // Create base chainspec from genesis (already has ordered Ethereum hardforks)
        let mut base_spec = ChainSpec::from_genesis(genesis);

        let morph_forks = vec![
            (MorphHardfork::Bernoulli, bernoulli_time),
            (MorphHardfork::Curie, curie_time),
            (MorphHardfork::Morph203, morph203_time),
            (MorphHardfork::Viridian, viridian_time),
        ]
        .into_iter()
        .filter_map(|(fork, time)| time.map(|time| (fork, ForkCondition::Timestamp(time))));

        base_spec.hardforks.extend(morph_forks);

        Self {
            inner: base_spec,
            info,
        }
    }
}

// Required by reth's e2e-test-utils for integration tests.
// The test utilities need to convert from standard ChainSpec to custom chain specs.
impl From<ChainSpec<Header>> for MorphChainSpec {
    fn from(spec: ChainSpec<Header>) -> Self {
        Self {
            inner: spec,
            info: MorphGenesisInfo::default(),
        }
    }
}

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
    type Header = alloy_consensus::Header;

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
    use crate::hardfork::{MorphHardfork, MorphHardforks};
    use reth_chainspec::{EthereumHardfork, ForkCondition, Hardforks};
    use reth_cli::chainspec::ChainSpecParser as _;
    use serde_json::json;

    #[test]
    fn can_load_testnet() {
        let _ = super::MorphChainSpecParser::parse("testnet")
            .expect("the testnet chainspec must always be well formed");
    }

    #[test]
    fn can_load_dev() {
        let _ = super::MorphChainSpecParser::parse("dev")
            .expect("the dev chainspec must always be well formed");
    }

    #[test]
    fn test_morph_chainspec_has_morph_hardforks() {
        let chainspec = super::MorphChainSpecParser::parse("testnet")
            .expect("the testnet chainspec must always be well formed");

        // Bernoulli should be active at genesis (timestamp 0)
        assert!(chainspec.is_bernoulli_active_at_timestamp(0));
    }

    #[test]
    fn test_morph_chainspec_implements_morph_hardforks_trait() {
        let chainspec = super::MorphChainSpecParser::parse("testnet")
            .expect("the testnet chainspec must always be well formed");

        // Should be able to query Morph hardfork activation through trait
        let activation = chainspec.morph_fork_activation(MorphHardfork::Bernoulli);
        assert_eq!(activation, ForkCondition::Timestamp(0));

        // Should be able to use convenience method through trait
        assert!(chainspec.is_bernoulli_active_at_timestamp(0));
        assert!(chainspec.is_bernoulli_active_at_timestamp(1000));
    }

    #[test]
    fn test_morph_hardforks_in_inner_hardforks() {
        let chainspec = super::MorphChainSpecParser::parse("testnet")
            .expect("the testnet chainspec must always be well formed");

        // Morph hardforks should be queryable from inner.hardforks via Hardforks trait
        let activation = chainspec.fork(MorphHardfork::Bernoulli);
        assert_eq!(activation, ForkCondition::Timestamp(0));

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
        // Create a genesis with Morph hardfork timestamps as extra fields in config
        // (non-standard fields automatically go into extra_fields)
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
                "bernoulliTime": 1000,
                "curieTime": 2000,
                "morph203Time": 3000,
                "viridianTime": 4000,
            },
            "alloc": {}
        });

        let genesis: alloy_genesis::Genesis =
            serde_json::from_value(genesis_json).expect("genesis should be valid");

        let chainspec = super::MorphChainSpec::from_genesis(genesis);

        // Test Bernoulli activation
        let activation = chainspec.fork(MorphHardfork::Bernoulli);
        assert_eq!(
            activation,
            ForkCondition::Timestamp(1000),
            "Bernoulli should be activated at the parsed timestamp from extra_fields"
        );

        assert!(
            !chainspec.is_bernoulli_active_at_timestamp(0),
            "Bernoulli should not be active before its activation timestamp"
        );
        assert!(
            chainspec.is_bernoulli_active_at_timestamp(1000),
            "Bernoulli should be active at its activation timestamp"
        );
        assert!(
            chainspec.is_bernoulli_active_at_timestamp(2000),
            "Bernoulli should be active after its activation timestamp"
        );

        // Test Curie activation
        let activation = chainspec.fork(MorphHardfork::Curie);
        assert_eq!(
            activation,
            ForkCondition::Timestamp(2000),
            "Curie should be activated at the parsed timestamp from extra_fields"
        );

        assert!(
            !chainspec.is_curie_active_at_timestamp(0),
            "Curie should not be active before its activation timestamp"
        );
        assert!(
            !chainspec.is_curie_active_at_timestamp(1000),
            "Curie should not be active at Bernoulli's activation timestamp"
        );
        assert!(
            chainspec.is_curie_active_at_timestamp(2000),
            "Curie should be active at its activation timestamp"
        );
        assert!(
            chainspec.is_curie_active_at_timestamp(3000),
            "Curie should be active after its activation timestamp"
        );

        // Test Morph203 activation
        let activation = chainspec.fork(MorphHardfork::Morph203);
        assert_eq!(
            activation,
            ForkCondition::Timestamp(3000),
            "Morph203 should be activated at the parsed timestamp from extra_fields"
        );

        assert!(
            !chainspec.is_morph203_active_at_timestamp(0),
            "Morph203 should not be active before its activation timestamp"
        );
        assert!(
            !chainspec.is_morph203_active_at_timestamp(1000),
            "Morph203 should not be active at Bernoulli's activation timestamp"
        );
        assert!(
            !chainspec.is_morph203_active_at_timestamp(2000),
            "Morph203 should not be active at Curie's activation timestamp"
        );
        assert!(
            chainspec.is_morph203_active_at_timestamp(3000),
            "Morph203 should be active at its activation timestamp"
        );
        assert!(
            chainspec.is_morph203_active_at_timestamp(4000),
            "Morph203 should be active after its activation timestamp"
        );

        // Test Viridian activation
        let activation = chainspec.fork(MorphHardfork::Viridian);
        assert_eq!(
            activation,
            ForkCondition::Timestamp(4000),
            "Viridian should be activated at the parsed timestamp from extra_fields"
        );

        assert!(
            !chainspec.is_viridian_active_at_timestamp(0),
            "Viridian should not be active before its activation timestamp"
        );
        assert!(
            !chainspec.is_viridian_active_at_timestamp(1000),
            "Viridian should not be active at Bernoulli's activation timestamp"
        );
        assert!(
            !chainspec.is_viridian_active_at_timestamp(2000),
            "Viridian should not be active at Curie's activation timestamp"
        );
        assert!(
            !chainspec.is_viridian_active_at_timestamp(3000),
            "Viridian should not be active at Morph203's activation timestamp"
        );
        assert!(
            chainspec.is_viridian_active_at_timestamp(4000),
            "Viridian should be active at its activation timestamp"
        );
        assert!(
            chainspec.is_viridian_active_at_timestamp(5000),
            "Viridian should be active after its activation timestamp"
        );
    }

    #[test]
    fn test_morph_hardforks_are_ordered_correctly() {
        // Create a genesis where Bernoulli should appear between Shanghai (time 0) and Cancun (time 2000)
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
                "cancunTime": 2000,
                "bernoulliTime": 1000,
            },
            "alloc": {}
        });

        let genesis: alloy_genesis::Genesis =
            serde_json::from_value(genesis_json).expect("genesis should be valid");

        let chainspec = super::MorphChainSpec::from_genesis(genesis);

        // Collect forks in order
        let forks: Vec<_> = chainspec.inner.hardforks.forks_iter().collect();

        // Find positions of Shanghai, Bernoulli, and Cancun
        let shanghai_pos = forks
            .iter()
            .position(|(f, _)| f.name() == EthereumHardfork::Shanghai.name());
        let bernoulli_pos = forks
            .iter()
            .position(|(f, _)| f.name() == MorphHardfork::Bernoulli.name());
        let cancun_pos = forks
            .iter()
            .position(|(f, _)| f.name() == EthereumHardfork::Cancun.name());

        assert!(shanghai_pos.is_some(), "Shanghai should be present");
        assert!(bernoulli_pos.is_some(), "Bernoulli should be present");
        assert!(cancun_pos.is_some(), "Cancun should be present");

        // Verify ordering: Shanghai (0) < Bernoulli (1000) < Cancun (2000)
        assert!(
            shanghai_pos.unwrap() < bernoulli_pos.unwrap(),
            "Shanghai (time 0) should come before Bernoulli (time 1000), but got positions {} and {}",
            shanghai_pos.unwrap(),
            bernoulli_pos.unwrap()
        );
        assert!(
            bernoulli_pos.unwrap() < cancun_pos.unwrap(),
            "Bernoulli (time 1000) should come before Cancun (time 2000), but got positions {} and {}",
            bernoulli_pos.unwrap(),
            cancun_pos.unwrap()
        );
    }

    #[test]
    fn test_morph_hardfork_at() {
        // Create a genesis with specific timestamps for each hardfork
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
                "bernoulliTime": 1000,
                "curieTime": 2000,
                "morph203Time": 3000,
                "viridianTime": 4000
            },
            "alloc": {}
        });

        let genesis: alloy_genesis::Genesis =
            serde_json::from_value(genesis_json).expect("genesis should be valid");

        let chainspec = super::MorphChainSpec::from_genesis(genesis);

        // Before Bernoulli activation - should return Bernoulli (it's the baseline)
        assert_eq!(
            chainspec.morph_hardfork_at(0),
            MorphHardfork::Bernoulli,
            "Should return Bernoulli at timestamp 0"
        );

        // At Bernoulli time
        assert_eq!(
            chainspec.morph_hardfork_at(1000),
            MorphHardfork::Bernoulli,
            "Should return Bernoulli at its activation time"
        );

        // Between Bernoulli and Curie
        assert_eq!(
            chainspec.morph_hardfork_at(1500),
            MorphHardfork::Bernoulli,
            "Should return Bernoulli between Bernoulli and Curie activation"
        );

        // At Curie time
        assert_eq!(
            chainspec.morph_hardfork_at(2000),
            MorphHardfork::Curie,
            "Should return Curie at its activation time"
        );

        // Between Curie and Morph203
        assert_eq!(
            chainspec.morph_hardfork_at(2500),
            MorphHardfork::Curie,
            "Should return Curie between Curie and Morph203 activation"
        );

        // At Morph203 time
        assert_eq!(
            chainspec.morph_hardfork_at(3000),
            MorphHardfork::Morph203,
            "Should return Morph203 at its activation time"
        );

        // Between Morph203 and Viridian
        assert_eq!(
            chainspec.morph_hardfork_at(3500),
            MorphHardfork::Morph203,
            "Should return Morph203 between Morph203 and Viridian activation"
        );

        // At Viridian time
        assert_eq!(
            chainspec.morph_hardfork_at(4000),
            MorphHardfork::Viridian,
            "Should return Viridian at its activation time"
        );

        // After Viridian
        assert_eq!(
            chainspec.morph_hardfork_at(5000),
            MorphHardfork::Viridian,
            "Should return Viridian after its activation time"
        );
    }
}
