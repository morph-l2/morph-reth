//! Error types for Morph EVM operations.

/// Errors that can occur during EVM configuration and execution.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MorphEvmError {
    /// Invalid EVM configuration.
    #[error("invalid EVM configuration: {0}")]
    InvalidEvmConfig(String),
}

/// The block's post-hoc sweep transfer-gas sum exceeds the consensus limit.
///
/// This is a block validation failure, not an internal executor failure: every
/// verifier reproducing the block must reject it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("block sweep transfer gas {transfer_gas_used} exceeds the {limit} limit")]
pub struct BlockSweepGasLimitExceeded {
    /// Actual sweep transfer gas consumed by the block.
    pub transfer_gas_used: u64,
    /// Maximum sweep transfer gas allowed in one block.
    pub limit: u64,
}
