use crate::{
    MorphEvmConfig, MorphEvmFactory, block::MorphReceiptBuilder, context::MorphBlockExecutionCtx,
};
use alloy_evm::{block::BlockExecutionError, eth::EthBlockExecutorFactory};
use morph_chainspec::MorphChainSpec;
use morph_primitives::MorphHeader;
use reth_evm::execute::{BlockAssembler, BlockAssemblerInput};
use reth_evm_ethereum::EthBlockAssembler;
use std::sync::Arc;

/// Assembler for Morph blocks.
#[derive(Debug, Clone)]
pub struct MorphBlockAssembler {
    pub(crate) inner: EthBlockAssembler<MorphChainSpec>,
}

impl MorphBlockAssembler {
    pub fn new(chain_spec: Arc<MorphChainSpec>) -> Self {
        Self {
            inner: EthBlockAssembler::new(chain_spec),
        }
    }
}

impl BlockAssembler<MorphEvmConfig> for MorphBlockAssembler {
    type Block = morph_primitives::Block;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, MorphEvmConfig, MorphHeader>,
    ) -> Result<Self::Block, BlockExecutionError> {
        let BlockAssemblerInput {
            evm_env,
            execution_ctx: MorphBlockExecutionCtx { inner },
            parent,
            transactions,
            output,
            bundle_state,
            state_provider,
            state_root,
            ..
        } = input;

        // Delegate block building to the inner assembler
        self.inner.assemble_block(BlockAssemblerInput::<
            EthBlockExecutorFactory<MorphReceiptBuilder, MorphChainSpec, MorphEvmFactory>,
        >::new(
            evm_env,
            inner,
            parent,
            transactions,
            output,
            bundle_state,
            state_provider,
            state_root,
        ))
    }
}
