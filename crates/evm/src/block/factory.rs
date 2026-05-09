//! Block executor factory for Morph.
//!
//! This module provides the [`MorphBlockExecutorFactory`] which creates block executors
//! that can execute Morph L2 blocks with proper L1 fee calculation and receipt building.

use crate::{
    block::{DefaultMorphReceiptBuilder, MorphBlockExecutor, MorphTxResult},
    evm::MorphEvm,
};
use alloy_evm::{
    block::{BlockExecutorFactory, StateDB},
    eth::EthBlockExecutionCtx,
    revm::Inspector,
};
use morph_chainspec::MorphChainSpec;
use morph_primitives::{MorphReceipt, MorphTxEnvelope};
use morph_revm::evm::MorphContext;
use std::sync::Arc;

use crate::evm::MorphEvmFactory;

/// Block executor factory for Morph.
#[derive(Debug, Clone)]
pub(crate) struct MorphBlockExecutorFactory {
    /// Receipt builder
    receipt_builder: DefaultMorphReceiptBuilder,
    /// Chain specification
    spec: Arc<MorphChainSpec>,
    /// EVM factory
    evm_factory: MorphEvmFactory,
}

impl MorphBlockExecutorFactory {
    /// Creates a new [`MorphBlockExecutorFactory`].
    pub(crate) fn new(spec: Arc<MorphChainSpec>, evm_factory: MorphEvmFactory) -> Self {
        Self {
            receipt_builder: DefaultMorphReceiptBuilder,
            spec,
            evm_factory,
        }
    }

    /// Returns the chain spec.
    pub(crate) const fn spec(&self) -> &Arc<MorphChainSpec> {
        &self.spec
    }

    /// Returns the EVM factory.
    pub(crate) const fn evm_factory(&self) -> &MorphEvmFactory {
        &self.evm_factory
    }
}

impl BlockExecutorFactory for MorphBlockExecutorFactory {
    type EvmFactory = MorphEvmFactory;
    type ExecutionCtx<'a> = EthBlockExecutionCtx<'a>;
    type Transaction = MorphTxEnvelope;
    type Receipt = MorphReceipt;
    type TxExecutionResult = MorphTxResult;
    // The Morph executor owns its EVM and receipt builder by value rather than
    // borrowing them from the factory, so the lifetime parameter is unused.
    type Executor<'a, DB: StateDB, I: Inspector<MorphContext<DB>>> = MorphBlockExecutor<DB, I>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: MorphEvm<DB, I>,
        _ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<MorphContext<DB>>,
    {
        MorphBlockExecutor::new(evm, self.spec.clone(), self.receipt_builder)
    }
}
