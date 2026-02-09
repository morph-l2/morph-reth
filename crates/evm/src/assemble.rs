use crate::{
    MorphEvmConfig, MorphEvmFactory, block::DefaultMorphReceiptBuilder,
    context::MorphBlockExecutionCtx,
};
use alloy_evm::{block::BlockExecutionError, eth::EthBlockExecutorFactory};
use morph_chainspec::MorphChainSpec;
use morph_primitives::MorphHeader;
use reth_evm::execute::{BlockAssembler, BlockAssemblerInput};
use reth_evm_ethereum::EthBlockAssembler;
use reth_primitives_traits::SealedHeader;
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

        // Convert MorphHeader parent to standard Header for the inner assembler.
        // We extract the inner Header since EthBlockAssembler works with standard Headers.
        let inner_parent = SealedHeader::new_unhashed(parent.header().inner.clone());

        // Delegate block building to the inner assembler
        let block = self.inner.assemble_block(BlockAssemblerInput::<
            EthBlockExecutorFactory<DefaultMorphReceiptBuilder, MorphChainSpec, MorphEvmFactory>,
        >::new(
            evm_env,
            inner,
            &inner_parent,
            transactions,
            output,
            bundle_state,
            state_provider,
            state_root,
        ))?;

        // Convert the standard Header back to MorphHeader.
        // The next_l1_msg_index and batch_hash will be set by the payload builder.
        Ok(block.map_header(MorphHeader::from))
    }
}
