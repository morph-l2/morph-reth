//! Morph system contract addresses and storage slots.
//!
//! This module defines the pre-deployed system contract addresses and their storage layouts
//! for the Morph L2 network. These contracts are deployed at genesis and provide essential
//! L2 functionality like L1 fee calculation and message queuing.

use alloy_primitives::{Address, B256, U256, address};

// =============================================================================
// System Contract Addresses
// =============================================================================

/// L1 Gas Price Oracle contract address.
///
/// This contract stores L1 gas prices used for calculating L1 data fees.
/// Reference: `rollup/rcfg/config.go` in go-ethereum
pub const L1_GAS_PRICE_ORACLE_ADDRESS: Address =
    address!("530000000000000000000000000000000000000F");

/// L2 Message Queue contract address.
///
/// Manages the L1-to-L2 message queue and stores the withdraw trie root.
/// See: <https://github.com/morph-l2/morph/blob/main/contracts/contracts/L2/predeploys/L2MessageQueue.sol>
pub const L2_MESSAGE_QUEUE_ADDRESS: Address = address!("5300000000000000000000000000000000000001");

// =============================================================================
// L2 Message Queue Storage Slots
// =============================================================================

// forge inspect contracts/L2/predeploys/L2MessageQueue.sol:L2MessageQueue storageLayout
// The contract inherits from AppendOnlyMerkleTree which has:
// - slot 0-31: branches[32] array
// - slot 32: nextIndex
// - slot 33: messageRoot (the withdraw trie root)

/// Storage slot for the withdraw trie root (`messageRoot`) in L2MessageQueue contract.
///
/// This is slot 33, which stores the Merkle root for L2→L1 messages.
/// See: <https://github.com/morph-l2/morph/blob/main/contracts/contracts/libraries/common/AppendOnlyMerkleTree.sol>
pub const L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT: U256 = U256::from_limbs([33, 0, 0, 0]);

// =============================================================================
// L2 Message Queue Functions
// =============================================================================

/// Reads the withdraw trie root from the L2MessageQueue contract storage.
///
/// This function reads the `messageRoot` slot from the L2MessageQueue contract,
/// which stores the Merkle root for L2→L1 withdrawal messages.
///
pub fn read_withdraw_trie_root<DB: revm::Database>(db: &mut DB) -> Result<B256, DB::Error> {
    let value = db.storage(
        L2_MESSAGE_QUEUE_ADDRESS,
        L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT,
    )?;
    Ok(B256::from(value))
}

/// Morph Fee Vault contract address.
///
/// Collects transaction fees.
pub const MORPH_FEE_VAULT_ADDRESS: Address = address!("530000000000000000000000000000000000000a");

/// Morph Token Registry contract address.
pub const MORPH_TOKEN_REGISTRY_ADDRESS: Address =
    address!("5300000000000000000000000000000000000021");

/// Sequencer contract address.
///
/// Manages sequencer operations.
pub const SEQUENCER_ADDRESS: Address = address!("5300000000000000000000000000000000000017");

// =============================================================================
// L1 Gas Price Oracle Storage Slots
// =============================================================================

// forge inspect contracts/l2/system/GasPriceOracle.sol:GasPriceOracle storageLayout
// +-----------------+---------------------+------+--------+-------+
// | Name            | Type                | Slot | Offset | Bytes |
// +=================================================================+
// | owner           | address             | 0    | 0      | 20    |
// +-----------------+---------------------+------+--------+-------+
// | l1BaseFee       | uint256             | 1    | 0      | 32    |
// +-----------------+---------------------+------+--------+-------+
// | overhead        | uint256             | 2    | 0      | 32    |
// +-----------------+---------------------+------+--------+-------+
// | scalar          | uint256             | 3    | 0      | 32    |
// +-----------------+---------------------+------+--------+-------+
// | whitelist       | contract IWhitelist | 4    | 0      | 20    |
// +-----------------+---------------------+------+--------+-------+
// | __deprecated0   | uint256             | 5    | 0      | 32    |
// +-----------------+---------------------+------+--------+-------+
// | l1BlobBaseFee   | uint256             | 6    | 0      | 32    |
// +-----------------+---------------------+------+--------+-------+
// | commitScalar    | uint256             | 7    | 0      | 32    |
// +-----------------+---------------------+------+--------+-------+
// | blobScalar      | uint256             | 8    | 0      | 32    |
// +-----------------+---------------------+------+--------+-------+
// | isBlobEnabled   | bool                | 9    | 0      | 1     |
// +-----------------+---------------------+------+--------+-------+

/// Storage slot for `l1BaseFee` in the `L1GasPriceOracle` contract.
pub const GPO_L1_BASE_FEE_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Storage slot for `overhead` in the `L1GasPriceOracle` contract.
pub const GPO_OVERHEAD_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);

/// Storage slot for `scalar` in the `L1GasPriceOracle` contract.
pub const GPO_SCALAR_SLOT: U256 = U256::from_limbs([3, 0, 0, 0]);

/// Storage slot for `l1BlobBaseFee` in the `L1GasPriceOracle` contract.
pub const GPO_L1_BLOB_BASE_FEE_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);

/// Storage slot for `commitScalar` in the `L1GasPriceOracle` contract.
pub const GPO_COMMIT_SCALAR_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

/// Storage slot for `blobScalar` in the `L1GasPriceOracle` contract.
pub const GPO_BLOB_SCALAR_SLOT: U256 = U256::from_limbs([8, 0, 0, 0]);

/// Storage slot for `isBlobEnabled` in the `L1GasPriceOracle` contract.
pub const GPO_IS_BLOB_ENABLED_SLOT: U256 = U256::from_limbs([9, 0, 0, 0]);

// =============================================================================
// L1 Gas Price Oracle Initial Values
// =============================================================================

/// The initial blob base fee used by the oracle contract.
pub const INITIAL_L1_BLOB_BASE_FEE: U256 = U256::from_limbs([1, 0, 0, 0]);

/// The initial commit scalar used by the oracle contract.
/// Reference: `rcfg.InitialCommitScalar` in go-ethereum (230759955285)
pub const INITIAL_COMMIT_SCALAR: U256 = U256::from_limbs([230759955285, 0, 0, 0]);

/// The initial blob scalar used by the oracle contract.
/// Reference: `rcfg.InitialBlobScalar` in go-ethereum (417565260)
pub const INITIAL_BLOB_SCALAR: U256 = U256::from_limbs([417565260, 0, 0, 0]);

/// Blob enabled flag is set to 1 (true).
pub const BLOB_ENABLED: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Storage updates for L1 gas price oracle initialization.
pub const L1_GAS_PRICE_ORACLE_INIT_STORAGE: [(U256, U256); 4] = [
    (GPO_L1_BLOB_BASE_FEE_SLOT, INITIAL_L1_BLOB_BASE_FEE),
    (GPO_COMMIT_SCALAR_SLOT, INITIAL_COMMIT_SCALAR),
    (GPO_BLOB_SCALAR_SLOT, INITIAL_BLOB_SCALAR),
    (GPO_IS_BLOB_ENABLED_SLOT, BLOB_ENABLED),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_contract_addresses() {
        // Verify addresses match go-ethereum definitions
        assert_eq!(
            L1_GAS_PRICE_ORACLE_ADDRESS,
            address!("530000000000000000000000000000000000000F")
        );
        assert_eq!(
            L2_MESSAGE_QUEUE_ADDRESS,
            address!("5300000000000000000000000000000000000001")
        );
        assert_eq!(
            MORPH_FEE_VAULT_ADDRESS,
            address!("530000000000000000000000000000000000000a")
        );
        assert_eq!(
            SEQUENCER_ADDRESS,
            address!("5300000000000000000000000000000000000017")
        );
    }

    #[test]
    fn test_storage_slots() {
        assert_eq!(GPO_L1_BASE_FEE_SLOT, U256::from(1));
        assert_eq!(GPO_OVERHEAD_SLOT, U256::from(2));
        assert_eq!(GPO_SCALAR_SLOT, U256::from(3));
        assert_eq!(GPO_L1_BLOB_BASE_FEE_SLOT, U256::from(6));
        assert_eq!(GPO_COMMIT_SCALAR_SLOT, U256::from(7));
        assert_eq!(GPO_BLOB_SCALAR_SLOT, U256::from(8));
        assert_eq!(GPO_IS_BLOB_ENABLED_SLOT, U256::from(9));
    }

    #[test]
    fn test_initial_values() {
        // Values from go-ethereum rcfg/config.go
        assert_eq!(INITIAL_COMMIT_SCALAR, U256::from(230759955285u64));
        assert_eq!(INITIAL_BLOB_SCALAR, U256::from(417565260u64));
    }
}
