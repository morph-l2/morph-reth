use crate::MorphEvmConfig;
use alloy_consensus::crypto::RecoveryError;
use alloy_primitives::Address;
use morph_payload_types::MorphExecutionData;
use morph_primitives::{Block, MorphTxEnvelope};
use morph_revm::MorphTxEnv;
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
    FromRecoveredTx, RecoveredTx, ToTxEnv,
};
use reth_primitives_traits::{SealedBlock, SignedTransaction};
use std::sync::Arc;

impl ConfigureEngineEvm<MorphExecutionData> for MorphEvmConfig {
    fn evm_env_for_payload(
        &self,
        payload: &MorphExecutionData,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.evm_env(&payload.block)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a MorphExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.context_for_block(&payload.block)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &MorphExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        let block = payload.block.clone();
        let transactions =
            (0..payload.block.body().transactions.len()).map(move |i| (block.clone(), i));

        Ok((transactions, RecoveredInBlock::new))
    }
}

/// A [`reth_evm::execute::ExecutableTxFor`] implementation that contains a pointer to the
/// block and the transaction index, allowing to prepare a [`MorphTxEnv`] without having to
/// clone block or transaction.
#[derive(Clone)]
struct RecoveredInBlock {
    block: Arc<SealedBlock<Block>>,
    index: usize,
    sender: Address,
}

impl RecoveredInBlock {
    fn new((block, index): (Arc<SealedBlock<Block>>, usize)) -> Result<Self, RecoveryError> {
        let sender = block.body().transactions[index].try_recover()?;
        Ok(Self {
            block,
            index,
            sender,
        })
    }
}

impl RecoveredTx<MorphTxEnvelope> for RecoveredInBlock {
    fn tx(&self) -> &MorphTxEnvelope {
        &self.block.body().transactions[self.index]
    }

    fn signer(&self) -> &alloy_primitives::Address {
        &self.sender
    }
}

impl ToTxEnv<MorphTxEnv> for RecoveredInBlock {
    fn to_tx_env(&self) -> MorphTxEnv {
        MorphTxEnv::from_recovered_tx(self.tx(), *self.signer())
    }
}
