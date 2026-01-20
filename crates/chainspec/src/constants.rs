//! Morph chainspec constants.

/// The Morph Mainnet chain ID.
pub const MORPH_MAINNET_CHAIN_ID: u64 = 2818;

/// The Morph Hoodi (testnet) chain ID.
pub const MORPH_HOODI_CHAIN_ID: u64 = 2910;

/// The default L2 sequencer fee (0.001 Gwei = 1_000_000 wei).
/// The sequencer has the right to set any base fee below `MORPH_MAX_BASE_FEE`.
pub const MORPH_BASE_FEE: u64 = 1_000_000;
