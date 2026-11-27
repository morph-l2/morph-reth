#[cfg(feature = "rpc")]
use alloy_consensus::BlockHeader;
use reth_evm::NextBlockEnvAttributes;

/// Default timestamp increment (in seconds) when building a synthetic pending
/// block environment on Morph L2.
///
/// Upstream reth defaults to L1's 12-second slot (`parent.timestamp + 12`),
/// which would prematurely activate timestamp-gated hardforks up to 12 seconds
/// early on Morph's 2-second block cadence. Using `+2s` matches Morph's block
/// time while avoiding premature fork activation.
#[cfg(feature = "rpc")]
const MORPH_PENDING_BLOCK_TIMESTAMP_INCREMENT: u64 = 2;

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
        let mut inner = NextBlockEnvAttributes::build_pending_env(parent, block_overrides);
        if block_overrides
            .and_then(|overrides| overrides.time)
            .is_none()
        {
            inner.timestamp = parent
                .timestamp()
                .saturating_add(MORPH_PENDING_BLOCK_TIMESTAMP_INCREMENT);
        }
        Self {
            inner,
            base_fee_per_gas: None,
        }
    }
}

#[cfg(all(test, feature = "rpc"))]
mod tests {
    use super::*;
    use alloy_rpc_types_eth::BlockOverrides;
    use morph_primitives::MorphHeader;
    use reth_primitives_traits::SealedHeader;
    use reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv;

    #[test]
    fn build_pending_env_uses_morph_timestamp_increment_unless_overridden() {
        let mut header = MorphHeader::default();
        header.inner.timestamp = 1_000;
        let parent = SealedHeader::seal_slow(header);

        let default_env = MorphNextBlockEnvAttributes::build_pending_env(&parent, None);
        assert_eq!(default_env.inner.timestamp, 1_002);

        let overrides = BlockOverrides {
            time: Some(5_000),
            ..Default::default()
        };
        let overridden_env =
            MorphNextBlockEnvAttributes::build_pending_env(&parent, Some(&overrides));
        assert_eq!(overridden_env.inner.timestamp, 5_000);
    }
}
