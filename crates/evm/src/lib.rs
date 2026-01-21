//! Morph EVM implementation.
//!
//! This crate provides the EVM configuration and block execution logic for Morph L2.
//!
//! # Main Components
//!
//! - [`MorphEvmConfig`]: Main EVM configuration that implements `BlockExecutorFactory`
//! - [`MorphEvmFactory`]: Factory for creating Morph EVM instances
//! - [`MorphBlockAssembler`]: Block assembly logic for payload building
//! - [`MorphBlockExecutionCtx`]: Execution context for block processing
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      MorphEvmConfig                             │
//! │  ┌─────────────────────┐  ┌─────────────────────────────────┐  │
//! │  │   EthEvmConfig      │  │    MorphBlockAssembler          │  │
//! │  │  (inner config)     │  │  (block building)               │  │
//! │  └─────────────────────┘  └─────────────────────────────────┘  │
//! │                 │                                               │
//! │                 ▼                                               │
//! │  ┌─────────────────────────────────────────────────────────┐   │
//! │  │              MorphBlockExecutor                         │   │
//! │  │   - Executes transactions                               │   │
//! │  │   - Handles L1 messages                                 │   │
//! │  │   - Calculates L1 data fee                              │   │
//! │  └─────────────────────────────────────────────────────────┘   │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Features
//!
//! - `reth-codec`: Enable `ConfigureEvm` implementation for reth integration
//! - `engine`: Enable engine API types

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "reth-codec")]
mod config;

mod assemble;
pub use assemble::MorphBlockAssembler;
mod block;
mod context;
pub use context::{MorphBlockExecutionCtx, MorphNextBlockEnvAttributes};

mod error;
pub use error::MorphEvmError;
pub mod evm;
use std::sync::Arc;

use alloy_evm::{
    Database,
    block::{BlockExecutorFactory, BlockExecutorFor},
    revm::{Inspector, database::State},
};
pub use evm::MorphEvmFactory;
use morph_primitives::{MorphReceipt, MorphTxEnvelope};

use crate::{block::MorphBlockExecutor, evm::MorphEvm};
use morph_chainspec::MorphChainSpec;
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
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph203Time": 0,
                "viridianTime": 0,
                "morph": {}
            },
            "alloc": {}
        });
        serde_json::from_value(genesis_json).expect("genesis should be valid")
    }

    #[test]
    fn test_evm_config_can_query_morph_hardforks() {
        // Create a test chainspec with Morph203 at genesis
        let chainspec = Arc::new(morph_chainspec::MorphChainSpec::from(create_test_genesis()));

        let evm_config = MorphEvmConfig::new_with_default_factory(chainspec);

        // Should be able to query Morph hardforks through the chainspec
        assert!(evm_config.chain_spec().is_morph203_active_at_timestamp(0));
        assert!(
            evm_config
                .chain_spec()
                .is_morph203_active_at_timestamp(1000)
        );

        // Should be able to query activation condition
        let activation = evm_config
            .chain_spec()
            .morph_fork_activation(MorphHardfork::Morph203);
        assert_eq!(activation, reth_chainspec::ForkCondition::Timestamp(0));
    }
}
