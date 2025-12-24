use revm::context::TxEnv;

/// Morph transaction environment.
///
/// An alias for [`TxEnv`] for backwards compatibility.
/// This can be extended to a full wrapper type if Morph-specific fields are needed.
pub type MorphTxEnv = TxEnv;
