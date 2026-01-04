//! Morph EVM implementation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod assemble;
#[cfg(feature = "engine")]
mod engine;
use alloy_consensus::BlockHeader as _;
pub use assemble::MorphBlockAssembler;
mod block;
mod context;
pub use context::{MorphBlockExecutionCtx, MorphNextBlockEnvAttributes};

mod error;
pub use error::MorphEvmError;
pub mod evm;
use std::{borrow::Cow, sync::Arc};

use alloy_evm::{
    self, Database, EvmEnv,
    block::{BlockExecutorFactory, BlockExecutorFor},
    eth::{EthBlockExecutionCtx, NextEvmEnvAttributes},
    revm::{Inspector, database::State},
};
pub use evm::MorphEvmFactory;
use morph_primitives::{Block, MorphHeader, MorphPrimitives, MorphReceipt, MorphTxEnvelope};
use reth_chainspec::EthChainSpec;
use reth_evm::{self, ConfigureEvm, EvmEnvFor};
use reth_primitives_traits::{SealedBlock, SealedHeader};

use crate::{block::MorphBlockExecutor, evm::MorphEvm};
use morph_chainspec::{MorphChainSpec, hardfork::MorphHardforks};
use morph_revm::evm::MorphContext;
use reth_evm_ethereum::EthEvmConfig;

pub use morph_revm::{MorphBlockEnv, MorphHaltReason};

/// Morph-related EVM configuration.
#[derive(Debug, Clone)]
pub struct MorphEvmConfig {
    /// Inner evm config
    pub inner: EthEvmConfig<MorphChainSpec, MorphEvmFactory>,

    /// Block assembler
    pub block_assembler: MorphBlockAssembler,
}

impl MorphEvmConfig {
    /// Create a new [`MorphEvmConfig`] with the given chain spec and EVM factory.
    pub fn new(chain_spec: Arc<MorphChainSpec>, evm_factory: MorphEvmFactory) -> Self {
        let inner = EthEvmConfig::new_with_evm_factory(chain_spec.clone(), evm_factory);
        Self {
            inner,
            block_assembler: MorphBlockAssembler::new(chain_spec),
        }
    }

    /// Create a new [`MorphEvmConfig`] with the given chain spec and default EVM factory.
    pub fn new_with_default_factory(chain_spec: Arc<MorphChainSpec>) -> Self {
        Self::new(chain_spec, MorphEvmFactory::default())
    }

    /// Returns the chain spec
    pub const fn chain_spec(&self) -> &Arc<MorphChainSpec> {
        self.inner.chain_spec()
    }

    /// Returns the inner EVM config
    pub const fn inner(&self) -> &EthEvmConfig<MorphChainSpec, MorphEvmFactory> {
        &self.inner
    }
}

impl BlockExecutorFactory for MorphEvmConfig {
    type EvmFactory = MorphEvmFactory;
    type ExecutionCtx<'a> = MorphBlockExecutionCtx<'a>;
    type Transaction = MorphTxEnvelope;
    type Receipt = MorphReceipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.inner.executor_factory.evm_factory()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: MorphEvm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<MorphContext<&'a mut State<DB>>> + 'a,
    {
        MorphBlockExecutor::new(evm, ctx, self.chain_spec())
    }
}

impl ConfigureEvm for MorphEvmConfig {
    type Primitives = MorphPrimitives;
    type Error = MorphEvmError;
    type NextBlockEnvCtx = MorphNextBlockEnvAttributes;
    type BlockExecutorFactory = Self;
    type BlockAssembler = MorphBlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        self
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &MorphHeader) -> Result<EvmEnvFor<Self>, Self::Error> {
        let EvmEnv { cfg_env, block_env } = EvmEnv::for_eth_block(
            header,
            self.chain_spec(),
            self.chain_spec().chain().id(),
            self.chain_spec()
                .blob_params_at_timestamp(header.timestamp()),
        );

        let spec = self.chain_spec().morph_hardfork_at(header.timestamp());

        Ok(EvmEnv {
            cfg_env: cfg_env.with_spec(spec),
            block_env: MorphBlockEnv { inner: block_env },
        })
    }

    fn next_evm_env(
        &self,
        parent: &MorphHeader,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        let EvmEnv { cfg_env, block_env } = EvmEnv::for_eth_next_block(
            parent,
            NextEvmEnvAttributes {
                timestamp: attributes.timestamp,
                suggested_fee_recipient: attributes.suggested_fee_recipient,
                prev_randao: attributes.prev_randao,
                gas_limit: attributes.gas_limit,
            },
            self.chain_spec()
                .next_block_base_fee(parent, attributes.timestamp)
                .unwrap_or_default(),
            self.chain_spec(),
            self.chain_spec().chain().id(),
            self.chain_spec()
                .blob_params_at_timestamp(attributes.timestamp),
        );

        let spec = self.chain_spec().morph_hardfork_at(attributes.timestamp);

        Ok(EvmEnv {
            cfg_env: cfg_env.with_spec(spec),
            block_env: MorphBlockEnv { inner: block_env },
        })
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<Block>,
    ) -> Result<MorphBlockExecutionCtx<'a>, Self::Error> {
        Ok(MorphBlockExecutionCtx {
            inner: EthBlockExecutionCtx {
                parent_hash: block.header().parent_hash(),
                parent_beacon_block_root: block.header().parent_beacon_block_root(),
                ommers: &[],
                withdrawals: block.body().withdrawals.as_ref().map(Cow::Borrowed),
                extra_data: block.extra_data().clone(),
            },
        })
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<MorphHeader>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<MorphBlockExecutionCtx<'_>, Self::Error> {
        Ok(MorphBlockExecutionCtx {
            inner: EthBlockExecutionCtx {
                parent_hash: parent.hash(),
                parent_beacon_block_root: attributes.parent_beacon_block_root,
                ommers: &[],
                withdrawals: attributes.inner.withdrawals.map(Cow::Owned),
                extra_data: attributes.inner.extra_data,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use morph_chainspec::hardfork::{MorphHardfork, MorphHardforks};
    use serde_json::json;

    /// Helper function to create a test genesis with Morph hardforks at timestamp 0
    fn create_test_genesis() -> alloy_genesis::Genesis {
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
                "bernoulliTime": 0,
                "curieTime": 0,
                "morph203Time": 0,
                "viridianTime": 0
            },
            "alloc": {}
        });
        serde_json::from_value(genesis_json).expect("genesis should be valid")
    }

    #[test]
    fn test_evm_config_can_query_morph_hardforks() {
        // Create a test chainspec with Bernoulli at genesis
        let chainspec = Arc::new(morph_chainspec::MorphChainSpec::from_genesis(
            create_test_genesis(),
        ));

        let evm_config = MorphEvmConfig::new_with_default_factory(chainspec);

        // Should be able to query Morph hardforks through the chainspec
        assert!(evm_config.chain_spec().is_bernoulli_active_at_timestamp(0));
        assert!(
            evm_config
                .chain_spec()
                .is_bernoulli_active_at_timestamp(1000)
        );

        // Should be able to query activation condition
        let activation = evm_config
            .chain_spec()
            .morph_fork_activation(MorphHardfork::Bernoulli);
        assert_eq!(activation, reth_chainspec::ForkCondition::Timestamp(0));
    }
}
