use reth_evm::NextBlockEnvAttributes;
#[cfg(feature = "rpc")]
use reth_primitives_traits::SealedHeader;

/// Context required for next block environment.
#[derive(Debug, Clone, derive_more::Deref)]
pub struct MorphNextBlockEnvAttributes {
    /// Inner [`NextBlockEnvAttributes`].
    #[deref]
    pub inner: NextBlockEnvAttributes,

    /// Optional base fee override for deterministic derivation/safe imports.
    pub base_fee_per_gas: Option<u64>,
}

#[cfg(feature = "rpc")]
impl reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv<morph_primitives::MorphHeader>
    for MorphNextBlockEnvAttributes
{
    fn build_pending_env(parent: &SealedHeader<morph_primitives::MorphHeader>) -> Self {
        Self {
            inner: NextBlockEnvAttributes::build_pending_env(parent),
            base_fee_per_gas: None,
        }
    }
}
