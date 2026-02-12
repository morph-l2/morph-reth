//! Block executor factory for Morph.
//!
//! This module provides the [`MorphBlockExecutorFactory`] which creates block executors
//! that can execute Morph L2 blocks with proper L1 fee calculation and receipt building.

use crate::{
    block::{DefaultMorphReceiptBuilder, MorphBlockExecutor},
    evm::MorphEvm,
};
use alloy_evm::{
    Database,
    block::{BlockExecutorFactory, BlockExecutorFor},
    eth::EthBlockExecutionCtx,
    revm::{Inspector, database::State},
};
use morph_chainspec::MorphChainSpec;
use morph_primitives::{MorphReceipt, MorphTxEnvelope};
use morph_revm::evm::MorphContext;
use std::sync::Arc;

use crate::evm::MorphEvmFactory;

/// Block executor factory for Morph.
///
/// This factory creates [`MorphBlockExecutor`] instances that handle Morph-specific
/// block execution logic including:
/// - L1 fee calculation for transactions
/// - Token fee information extraction for MorphTx (0x7F) transactions
/// - Curie hardfork application
///
/// Unlike using `EthBlockExecutorFactory`, this factory uses the custom
/// `MorphReceiptBuilder` trait which includes `l1_fee` in its context,
/// ensuring receipts are built with complete information.
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

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: MorphEvm<&'a mut State<DB>, I>,
        _ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<MorphContext<&'a mut State<DB>>> + 'a,
    {
        MorphBlockExecutor::new(evm, &self.spec, &self.receipt_builder)
    }
}
