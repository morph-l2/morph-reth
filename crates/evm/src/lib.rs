//! Morph EVM implementation.
//!
//! This crate provides the EVM configuration and block execution logic for Morph L2.
//! It implements reth's trait system to integrate Morph-specific behavior into the
//! standard reth node architecture.
//!
//! # Main Components
//!
//! - [`MorphEvmConfig`]: Main EVM configuration that implements `BlockExecutorFactory`
//! - [`MorphEvmFactory`]: Factory for creating Morph EVM instances with custom context
//! - [`MorphBlockAssembler`]: Block assembly logic for building `MorphHeader` blocks
//! - [`MorphNextBlockEnvAttributes`]: Attributes for configuring the next block environment
//!
//! # Architecture
//!
//! The crate is structured around reth's EVM trait hierarchy:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      MorphEvmConfig                             │
//! │  ┌─────────────────────────┐  ┌─────────────────────────────┐  │
//! │  │ MorphBlockExecutorFactory│  │    MorphBlockAssembler     │  │
//! │  │  (creates executors)    │  │  (block building)          │  │
//! │  └─────────────────────────┘  └─────────────────────────────┘  │
//! │                 │                                               │
//! │                 ▼                                               │
//! │  ┌─────────────────────────────────────────────────────────┐   │
//! │  │              MorphBlockExecutor                         │   │
//! │  │   - Executes transactions with Morph EVM               │   │
//! │  │   - Handles L1 message transactions (0x7E)             │   │
//! │  │   - Calculates L1 data fee for all L2 transactions     │   │
//! │  │   - Extracts token fee info for MorphTx (0x7F)         │   │
//! │  │   - Builds receipts with full Morph-specific context   │   │
//! │  │   - Applies hardfork state changes (Curie, etc.)       │   │
//! │  └─────────────────────────────────────────────────────────┘   │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Differences from Standard Ethereum
//!
//! 1. **L1 Data Fee**: Every L2 transaction (except L1 messages) pays an L1 fee
//!    calculated from the L1 Gas Price Oracle contract state.
//!
//! 2. **Token Gas Payment**: MorphTx (0x7F) allows users to pay gas with ERC20 tokens.
//!    The executor queries L2TokenRegistry for exchange rates.
//!
//! 3. **Custom Receipts**: Receipts include L1 fee and optional token fee info,
//!    requiring a custom receipt builder.
//!
//! 4. **L2-Specific Header**: `MorphHeader` extends standard header with
//!    `next_l1_msg_index` and `batch_hash` fields.
//!
//! 5. **No Blob Transactions**: EIP-4844 blob transactions are not supported.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

// reth_ethereum_primitives is explicitly depended on to ensure feature unification
// for NodePrimitives trait bounds that require serde features.
use reth_ethereum_primitives as _;

mod config;
mod engine;

mod assemble;
pub use assemble::MorphBlockAssembler;
mod block;
mod context;
pub use context::MorphNextBlockEnvAttributes;

mod error;
pub use error::MorphEvmError;
pub mod evm;
use std::sync::Arc;

use alloy_evm::{
    Database,
    block::{BlockExecutorFactory, BlockExecutorFor},
    eth::EthBlockExecutionCtx,
    revm::{Inspector, database::State},
};
pub use evm::MorphEvmFactory;
use morph_primitives::{MorphReceipt, MorphTxEnvelope};

use crate::{block::MorphBlockExecutorFactory, evm::MorphEvm};
use morph_chainspec::MorphChainSpec;
use morph_revm::evm::MorphContext;

pub use morph_revm::{MorphBlockEnv, MorphHaltReason};

/// Morph EVM configuration and block executor factory.
///
/// This is the main entry point for Morph EVM integration with reth. It implements
/// both `ConfigureEvm` and `BlockExecutorFactory` traits, providing:
///
/// - EVM environment configuration for block execution and building
/// - Block executor creation with Morph-specific execution logic
/// - Block assembler for constructing `MorphHeader` blocks
///
/// # Usage
///
/// Create with a chain specification:
/// # Trait Implementations
///
/// - `ConfigureEvm`: Provides EVM environment setup and block context creation
/// - `BlockExecutorFactory`: Creates `MorphBlockExecutor` instances for block execution
///
/// # Thread Safety
///
/// `MorphEvmConfig` is `Clone` and can be safely shared across threads.
/// The internal executor factory is lightweight and stateless.
#[derive(Debug, Clone)]
pub struct MorphEvmConfig {
    /// Internal block executor factory that creates `MorphBlockExecutor` instances.
    executor_factory: MorphBlockExecutorFactory,

    /// Block assembler for building `MorphHeader` blocks from execution results.
    pub block_assembler: MorphBlockAssembler,
}

impl MorphEvmConfig {
    /// Creates a new [`MorphEvmConfig`] with the given chain spec and EVM factory.
    ///
    /// This is the primary constructor that allows specifying a custom EVM factory
    /// for advanced use cases (e.g., custom inspector or precompile configuration).
    pub fn new(chain_spec: Arc<MorphChainSpec>, evm_factory: MorphEvmFactory) -> Self {
        let executor_factory = MorphBlockExecutorFactory::new(chain_spec.clone(), evm_factory);
        Self {
            executor_factory,
            block_assembler: MorphBlockAssembler::new(chain_spec),
        }
    }

    /// Creates a new [`MorphEvmConfig`] with the given chain spec and default EVM factory.
    ///
    /// This is the recommended constructor for most use cases. It uses the default
    /// Morph EVM factory with standard configuration.
    pub fn new_with_default_factory(chain_spec: Arc<MorphChainSpec>) -> Self {
        Self::new(chain_spec, MorphEvmFactory::default())
    }

    /// Returns a reference to the Morph chain specification.
    ///
    /// The chain spec contains hardfork configuration and network parameters.
    pub const fn chain_spec(&self) -> &Arc<MorphChainSpec> {
        self.executor_factory.spec()
    }

    /// Returns a reference to the Morph EVM factory.
    ///
    /// The factory is used to create EVM instances for block execution.
    pub const fn evm_factory(&self) -> &MorphEvmFactory {
        self.executor_factory.evm_factory()
    }
}

impl BlockExecutorFactory for MorphEvmConfig {
    type EvmFactory = MorphEvmFactory;
    type ExecutionCtx<'a> = EthBlockExecutionCtx<'a>;
    type Transaction = MorphTxEnvelope;
    type Receipt = MorphReceipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.executor_factory.evm_factory()
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
        self.executor_factory.create_executor(evm, ctx)
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
        // Morph203 is configured at timestamp 0, so it should be active at timestamp 0
        assert!(activation.active_at_timestamp(0));
    }
}
