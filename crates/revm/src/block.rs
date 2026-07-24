use alloy_evm::env::BlockEnvironment;
use alloy_primitives::{Address, B256, U256};
use revm::{
    context::{Block, BlockEnv},
    context_interface::block::BlobExcessGasAndPrice,
};

/// Configuration for deposit sweeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepConfig {
    /// Address of the sweep registry.
    pub registry_address: Address,
    /// Fixed EIP-7702 implementation authorized for sweep deposits.
    pub delegate_address: Address,
    /// Expected runtime code hash of the fixed sweep-deposit implementation.
    pub delegate_code_hash: B256,
}

/// Morph block environment.
#[derive(Debug, Clone, Default, derive_more::Deref, derive_more::DerefMut)]
pub struct MorphBlockEnv {
    /// Inner [`BlockEnv`].
    #[deref]
    #[deref_mut]
    pub inner: BlockEnv,
    /// Sweep configuration, enabled from Onyx onward.
    pub sweep: Option<SweepConfig>,
}

impl Block for MorphBlockEnv {
    #[inline]
    fn number(&self) -> U256 {
        self.inner.number()
    }

    #[inline]
    fn beneficiary(&self) -> Address {
        self.inner.beneficiary()
    }

    #[inline]
    fn timestamp(&self) -> U256 {
        self.inner.timestamp()
    }

    #[inline]
    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    #[inline]
    fn basefee(&self) -> u64 {
        self.inner.basefee()
    }

    #[inline]
    fn difficulty(&self) -> U256 {
        self.inner.difficulty()
    }

    #[inline]
    fn prevrandao(&self) -> Option<B256> {
        self.inner.prevrandao()
    }

    #[inline]
    fn blob_excess_gas_and_price(&self) -> Option<BlobExcessGasAndPrice> {
        self.inner.blob_excess_gas_and_price()
    }
}

impl BlockEnvironment for MorphBlockEnv {
    fn inner_mut(&mut self) -> &mut BlockEnv {
        &mut self.inner
    }
}
