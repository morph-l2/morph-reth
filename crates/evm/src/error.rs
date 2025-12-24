//! Error types for Morph EVM operations.

/// Errors that can occur during EVM configuration and execution.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MorphEvmError {
    /// Invalid EVM configuration.
    #[error("invalid EVM configuration: {0}")]
    InvalidEvmConfig(String),
}
