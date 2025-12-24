use alloy_evm::eth::EthBlockExecutionCtx;
use reth_evm::NextBlockEnvAttributes;

/// Execution context for Morph block.
#[derive(Debug, Clone, derive_more::Deref)]
pub struct MorphBlockExecutionCtx<'a> {
    /// Inner [`EthBlockExecutionCtx`].
    #[deref]
    pub inner: EthBlockExecutionCtx<'a>,
}

/// Context required for next block environment.
#[derive(Debug, Clone, derive_more::Deref)]
pub struct MorphNextBlockEnvAttributes {
    /// Inner [`NextBlockEnvAttributes`].
    #[deref]
    pub inner: NextBlockEnvAttributes,
}

#[cfg(feature = "rpc")]
impl reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv<morph_primitives::MorphHeader>
    for MorphNextBlockEnvAttributes
{
    fn build_pending_env(parent: &crate::SealedHeader<morph_primitives::MorphHeader>) -> Self {
        Self {
            inner: NextBlockEnvAttributes::build_pending_env(parent),
        }
    }
}
