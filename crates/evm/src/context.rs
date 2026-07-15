use reth_evm::NextBlockEnvAttributes;

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
    fn build_pending_env(
        parent: &reth_primitives_traits::SealedHeader<morph_primitives::MorphHeader>,
        block_overrides: Option<&alloy_rpc_types_eth::BlockOverrides>,
    ) -> Self {
        Self {
            inner: NextBlockEnvAttributes::build_pending_env(parent, block_overrides),
            base_fee_per_gas: None,
        }
    }
}
