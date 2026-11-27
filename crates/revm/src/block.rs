use crate::evm::MorphEvmTypes;

/// Morph block environment.
///
/// Morph currently needs nothing beyond the standard fields; the extension point stays
/// `MorphEvmTypes::BlockEnvExt`, so block-scoped data can be added without renaming this type.
pub type MorphBlockEnv = evm2::env::BlockEnv<MorphEvmTypes>;
