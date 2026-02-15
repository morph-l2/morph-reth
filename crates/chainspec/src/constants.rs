//! Morph chainspec constants.

use alloy_primitives::{B256, b256};

/// The Morph Mainnet chain ID.
pub const MORPH_MAINNET_CHAIN_ID: u64 = 2818;

/// The Morph Hoodi (testnet) chain ID.
pub const MORPH_HOODI_CHAIN_ID: u64 = 2910;

/// The default L2 sequencer fee (0.001 Gwei = 1_000_000 wei).
/// The sequencer has the right to set any base fee below `MORPH_MAX_BASE_FEE`.
pub const MORPH_BASE_FEE: u64 = 1_000_000;

/// Morph Mainnet genesis hash (computed with ZK-trie state root).
///
/// Source: go-ethereum/params/config.go
pub const MORPH_MAINNET_GENESIS_HASH: B256 =
    b256!("649c9b1f9f831771529dbf286a63dd071530d73c8fa410997eebaf449acfa7a9");

/// Morph Mainnet genesis state root (ZK-trie).
///
/// Source: go-ethereum/params/config.go
pub const MORPH_MAINNET_GENESIS_STATE_ROOT: B256 =
    b256!("09688bec5d876538664e62247c2f64fc7a02c54a3f898b42020730c7dd4933aa");

/// Morph Hoodi genesis hash (computed with ZK-trie state root).
///
/// Source: go-ethereum/params/config.go
pub const MORPH_HOODI_GENESIS_HASH: B256 =
    b256!("2cbcff7ec8d68255cb130d5274217cded0c83c417b9ed5e045e1ffcc3ebfc35c");

/// Morph Hoodi genesis state root (ZK-trie).
///
/// Source: go-ethereum/params/config.go
pub const MORPH_HOODI_GENESIS_STATE_ROOT: B256 =
    b256!("0a31941eb1853862c0c38f378eb0c519e9e66f0942e39b47dca38c0437ab6b3e");
