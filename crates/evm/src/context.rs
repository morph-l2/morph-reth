use alloy_consensus::BlockHeader;
use morph_payload_types::MorphPayloadAttributes;
use reth_evm::NextBlockEnvAttributes;
use reth_payload_primitives::{BuildNextEnv, PayloadBuilderError};
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

/// v2.0.0 idiomatic constructor:
/// `MorphNextBlockEnvAttributes::build_next_env(&rpc_attrs, &parent, &())`.
///
/// Payload builders that hold a `&MorphPayloadAttributes` can call this trait
/// directly instead of manually splatting fields into `NextBlockEnvAttributes`.
/// The existing inline construction in `build_payload_inner` is preserved for
/// now; new code should prefer this entry point.
impl BuildNextEnv<MorphPayloadAttributes, morph_primitives::MorphHeader, ()>
    for MorphNextBlockEnvAttributes
{
    fn build_next_env(
        attributes: &MorphPayloadAttributes,
        parent: &SealedHeader<morph_primitives::MorphHeader>,
        _ctx: &(),
    ) -> Result<Self, PayloadBuilderError> {
        Ok(Self {
            inner: NextBlockEnvAttributes {
                timestamp: attributes.inner.timestamp,
                suggested_fee_recipient: attributes.inner.suggested_fee_recipient,
                prev_randao: attributes.inner.prev_randao,
                gas_limit: attributes.gas_limit.unwrap_or(parent.gas_limit()),
                withdrawals: attributes.inner.withdrawals.clone().map(Into::into),
                parent_beacon_block_root: attributes.inner.parent_beacon_block_root,
                extra_data: Default::default(),
            },
            base_fee_per_gas: attributes.base_fee_per_gas,
        })
    }
}
