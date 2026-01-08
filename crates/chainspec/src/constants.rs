//! Morph chainspec constants.

use alloy_primitives::{Address, address};

/// The transaction fee recipient on the L2.
pub const MORPH_FEE_VAULT_ADDRESS_HOODI: Address =
    address!("29107CB79Ef8f69fE1587F77e283d47E84c5202f");

/// The transaction fee recipient on the L2.
pub const MORPH_FEE_VAULT_ADDRESS_MAINNET: Address =
    address!("530000000000000000000000000000000000000a");

/// The maximum size in bytes of the tx payload for a block.
pub const MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK: usize = 120 * 1024;

/// The Morph Mainnet chain ID.
pub const MORPH_MAINNET_CHAIN_ID: u64 = 2818;

/// The Morph Hoodi (testnet) chain ID.
pub const MORPH_HOODI_CHAIN_ID: u64 = 2910;

/// The default L2 sequencer fee (0.001 Gwei = 1_000_000 wei).
/// The sequencer has the right to set any base fee below `MORPH_MAX_BASE_FEE`.
pub const MORPH_BASE_FEE: u64 = 1_000_000;

/// The maximum allowed L2 base fee (10 Gwei = 10_000_000_000 wei).
pub const MORPH_MAX_BASE_FEE: u64 = 10_000_000_000;
