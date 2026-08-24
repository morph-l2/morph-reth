//! Morph chainspec constants.

use alloy_primitives::{Address, B256, U256, address, b256};

/// The Morph Mainnet chain ID.
pub const MORPH_MAINNET_CHAIN_ID: u64 = 2818;

/// The Morph Hoodi (testnet) chain ID.
pub const MORPH_HOODI_CHAIN_ID: u64 = 2910;

/// The default L2 sequencer fee (0.001 Gwei = 1_000_000 wei).
/// The sequencer has the right to set any base fee below `MORPH_MAX_BASE_FEE`.
pub const MORPH_BASE_FEE: u64 = 1_000_000;

/// Maximum L2 transaction payload bytes per block (L1 messages excluded).
///
/// Matches morph-geth `params.MorphMaxTxPayloadBytesPerBlock` (`720 * 1024`).
/// `720 KiB = 120 KiB × 6`, sized so one uncompressed L2 block fits in a 6-blob
/// batch (`6 × 4096 × 31 = 761_856` usable bytes). Enforced on import by Morph
/// consensus and used as the sequencer packing default.
pub const MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK: u64 = 720 * 1024;

/// Default priority fee returned by `eth_maxPriorityFeePerGas` when the gas
/// price oracle has no usable block samples (cold start or empty/zero-tip
/// blocks, the common case on Morph L2).
///
/// Matches morph-geth, which inherits this value from upstream go-ethereum's
/// `miner.DefaultConfig.GasPrice = params.GWei / 1000 = 1_000_000 wei`
/// (see `eth/backend.go`: `gpoParams.Default = config.Miner.GasPrice`).
/// Without this default, reth would fall back to its own 1 gwei default,
/// causing `eth_maxPriorityFeePerGas` to diverge from geth by 1000x.
pub const MORPH_DEFAULT_PRIORITY_FEE: u64 = 1_000_000;

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

// =============================================================================
// L2 System Contract Constants
// =============================================================================

/// L2 Message Queue contract address.
///
/// Manages the L1-to-L2 message queue and stores the withdraw trie root.
pub const L2_MESSAGE_QUEUE_ADDRESS: Address = address!("5300000000000000000000000000000000000001");

/// Storage slot for the withdraw trie root (`messageRoot`) in L2MessageQueue contract.
///
/// This is slot 33, which stores the Merkle root for L2->L1 messages.
pub const L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT: U256 = U256::from_limbs([33, 0, 0, 0]);

// =============================================================================
// Protocol Gas Constants
// =============================================================================

/// Lowest block `gasLimit` the protocol accepts.
///
/// Matches go-ethereum's `params.MinGasLimit` (`params/protocol_params.go`).
/// Both ends of block production read it, which is why it lives here rather than
/// in either crate alone: the sequencer clamps its `gasLimit` target to it before
/// ramping (`morph-payload-builder`), and header validation rejects anything below
/// it (`morph-consensus`). One definition keeps producer and validator from
/// drifting apart.
pub const MINIMUM_GAS_LIMIT: u64 = 5000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chain_ids_are_distinct() {
        assert_ne!(MORPH_MAINNET_CHAIN_ID, MORPH_HOODI_CHAIN_ID);
    }

    #[test]
    fn test_chain_id_values() {
        assert_eq!(MORPH_MAINNET_CHAIN_ID, 2818);
        assert_eq!(MORPH_HOODI_CHAIN_ID, 2910);
    }

    #[test]
    fn test_genesis_hashes_are_distinct() {
        assert_ne!(MORPH_MAINNET_GENESIS_HASH, MORPH_HOODI_GENESIS_HASH);
        assert_ne!(
            MORPH_MAINNET_GENESIS_STATE_ROOT,
            MORPH_HOODI_GENESIS_STATE_ROOT
        );
    }

    #[test]
    fn test_genesis_hashes_are_nonzero() {
        assert_ne!(MORPH_MAINNET_GENESIS_HASH, B256::ZERO);
        assert_ne!(MORPH_HOODI_GENESIS_HASH, B256::ZERO);
        assert_ne!(MORPH_MAINNET_GENESIS_STATE_ROOT, B256::ZERO);
        assert_ne!(MORPH_HOODI_GENESIS_STATE_ROOT, B256::ZERO);
    }

    #[test]
    fn test_l2_message_queue_address() {
        assert_eq!(
            L2_MESSAGE_QUEUE_ADDRESS,
            address!("5300000000000000000000000000000000000001")
        );
    }

    #[test]
    fn test_withdraw_trie_root_slot() {
        assert_eq!(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT, U256::from(33));
    }

    #[test]
    fn test_base_fee() {
        assert_eq!(MORPH_BASE_FEE, 1_000_000);
    }

    #[test]
    fn test_max_tx_payload_bytes_per_block() {
        assert_eq!(MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK, 720 * 1024);
        assert_eq!(MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK, 737_280);
    }
}
