//! L1 Block Info and Gas Price Oracle constants for Morph L2.
//!
//! This module provides:
//! - [`L1BlockInfo`]: L1 gas price oracle data for L1 data fee calculation
//! - Gas Price Oracle contract address and storage slot constants
//!
//! # Storage Layout
//!
//! The L1 Gas Price Oracle contract has the following storage layout:
//!
//! | Name          | Type    | Slot | Description                    |
//! |---------------|---------|------|--------------------------------|
//! | owner         | address | 0    | Contract owner                 |
//! | l1BaseFee     | uint256 | 1    | L1 base fee                    |
//! | overhead      | uint256 | 2    | L1 overhead gas                |
//! | scalar        | uint256 | 3    | L1 fee scalar                  |
//! | whitelist     | address | 4    | Whitelist contract             |
//! | __deprecated0 | uint256 | 5    | Deprecated                     |
//! | l1BlobBaseFee | uint256 | 6    | L1 blob base fee (Curie+)      |
//! | commitScalar  | uint256 | 7    | Commit scalar (Curie+)         |
//! | blobScalar    | uint256 | 8    | Blob scalar (Curie+)           |
//! | isCurie       | bool    | 9    | Curie hardfork flag            |

use alloy_primitives::{Address, U256, address};
use morph_chainspec::hardfork::MorphHardfork;
use revm::Database;

// =============================================================================
// Gas Price Oracle Constants
// =============================================================================

/// Gas cost for zero bytes in calldata.
const ZERO_BYTE_COST: u64 = 4;
/// Gas cost for non-zero bytes in calldata.
const NON_ZERO_BYTE_COST: u64 = 16;

/// Extra cost added to L1 commit gas calculation.
const TX_L1_COMMIT_EXTRA_COST: U256 = U256::from_limbs([64u64, 0, 0, 0]);
/// Precision factor for L1 fee calculation (1e9).
const TX_L1_FEE_PRECISION: U256 = U256::from_limbs([1_000_000_000u64, 0, 0, 0]);

// =============================================================================
// L1 Gas Price Oracle Address
// =============================================================================

/// L1 Gas Price Oracle contract address on Morph L2.
///
/// This contract stores L1 gas prices used for calculating L1 data fees.
/// Reference: `rollup/rcfg/config.go` in go-ethereum
pub const L1_GAS_PRICE_ORACLE_ADDRESS: Address =
    address!("530000000000000000000000000000000000000F");

// =============================================================================
// L1 Gas Price Oracle Storage Slots
// =============================================================================

/// Storage slot for `owner` in the `L1GasPriceOracle` contract.
pub const GPO_OWNER_SLOT: U256 = U256::from_limbs([0, 0, 0, 0]);

/// Storage slot for `l1BaseFee` in the `L1GasPriceOracle` contract.
pub const GPO_L1_BASE_FEE_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Storage slot for `overhead` in the `L1GasPriceOracle` contract.
pub const GPO_OVERHEAD_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);

/// Storage slot for `scalar` in the `L1GasPriceOracle` contract.
pub const GPO_SCALAR_SLOT: U256 = U256::from_limbs([3, 0, 0, 0]);

/// Storage slot for `whitelist` in the `L1GasPriceOracle` contract.
pub const GPO_WHITELIST_SLOT: U256 = U256::from_limbs([4, 0, 0, 0]);

/// Storage slot for `l1BlobBaseFee` in the `L1GasPriceOracle` contract.
/// Added in the Curie hardfork.
pub const GPO_L1_BLOB_BASE_FEE_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);

/// Storage slot for `commitScalar` in the `L1GasPriceOracle` contract.
/// Added in the Curie hardfork.
pub const GPO_COMMIT_SCALAR_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

/// Storage slot for `blobScalar` in the `L1GasPriceOracle` contract.
/// Added in the Curie hardfork.
pub const GPO_BLOB_SCALAR_SLOT: U256 = U256::from_limbs([8, 0, 0, 0]);

/// Storage slot for `isCurie` in the `L1GasPriceOracle` contract.
/// Added in the Curie hardfork. Set to 1 (true) after Curie activation.
pub const GPO_IS_CURIE_SLOT: U256 = U256::from_limbs([9, 0, 0, 0]);

// =============================================================================
// L1 Gas Price Oracle Initial Values (for Curie hardfork)
// =============================================================================

/// The initial blob base fee used by the oracle contract at Curie activation.
pub const INITIAL_L1_BLOB_BASE_FEE: U256 = U256::from_limbs([1, 0, 0, 0]);

/// The initial commit scalar used by the oracle contract at Curie activation.
/// Reference: `rcfg.InitialCommitScalar` in go-ethereum (230759955285)
pub const INITIAL_COMMIT_SCALAR: U256 = U256::from_limbs([230759955285, 0, 0, 0]);

/// The initial blob scalar used by the oracle contract at Curie activation.
/// Reference: `rcfg.InitialBlobScalar` in go-ethereum (417565260)
pub const INITIAL_BLOB_SCALAR: U256 = U256::from_limbs([417565260, 0, 0, 0]);

/// Curie hardfork flag value (1 = true).
pub const IS_CURIE: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Storage updates for L1 gas price oracle Curie hardfork initialization.
///
/// These storage slots are initialized when the Curie hardfork activates:
/// - l1BlobBaseFee = 1
/// - commitScalar = InitialCommitScalar
/// - blobScalar = InitialBlobScalar
/// - isCurie = 1 (true)
pub const CURIE_L1_GAS_PRICE_ORACLE_STORAGE: [(U256, U256); 4] = [
    (GPO_L1_BLOB_BASE_FEE_SLOT, INITIAL_L1_BLOB_BASE_FEE),
    (GPO_COMMIT_SCALAR_SLOT, INITIAL_COMMIT_SCALAR),
    (GPO_BLOB_SCALAR_SLOT, INITIAL_BLOB_SCALAR),
    (GPO_IS_CURIE_SLOT, IS_CURIE),
];

// =============================================================================
// L1 Block Info
// =============================================================================

/// L1 block info for fee calculation.
///
/// Contains the fee parameters fetched from the L1 Gas Price Oracle contract.
/// These parameters are used to calculate the L1 data fee for transactions.
#[derive(Clone, Debug, Default)]
pub struct L1BlockInfo {
    /// The base fee of the L1 origin block.
    pub l1_base_fee: U256,
    /// The current L1 fee overhead.
    pub l1_fee_overhead: U256,
    /// The current L1 fee scalar.
    pub l1_base_fee_scalar: U256,
    /// The current L1 blob base fee, None if before Curie.
    pub l1_blob_base_fee: Option<U256>,
    /// The current L1 commit scalar, None if before Curie.
    pub l1_commit_scalar: Option<U256>,
    /// The current L1 blob scalar, None if before Curie.
    pub l1_blob_scalar: Option<U256>,
    /// The current call data gas (l1_commit_scalar * l1_base_fee), None if before Curie.
    pub calldata_gas: Option<U256>,
}

impl L1BlockInfo {
    /// Try to fetch the L1 block info from the database.
    ///
    /// This reads the fee parameters from the L1 Gas Price Oracle contract storage.
    /// Different parameters are fetched depending on whether the Curie hardfork is active.
    pub fn try_fetch<DB: Database>(
        db: &mut DB,
        hardfork: MorphHardfork,
    ) -> Result<Self, DB::Error> {
        let l1_base_fee = db.storage(L1_GAS_PRICE_ORACLE_ADDRESS, GPO_L1_BASE_FEE_SLOT)?;
        let l1_fee_overhead = db.storage(L1_GAS_PRICE_ORACLE_ADDRESS, GPO_OVERHEAD_SLOT)?;
        let l1_base_fee_scalar = db.storage(L1_GAS_PRICE_ORACLE_ADDRESS, GPO_SCALAR_SLOT)?;

        if !hardfork.is_curie() {
            Ok(Self {
                l1_base_fee,
                l1_fee_overhead,
                l1_base_fee_scalar,
                ..Default::default()
            })
        } else {
            let l1_blob_base_fee =
                db.storage(L1_GAS_PRICE_ORACLE_ADDRESS, GPO_L1_BLOB_BASE_FEE_SLOT)?;
            let l1_commit_scalar =
                db.storage(L1_GAS_PRICE_ORACLE_ADDRESS, GPO_COMMIT_SCALAR_SLOT)?;
            let l1_blob_scalar = db.storage(L1_GAS_PRICE_ORACLE_ADDRESS, GPO_BLOB_SCALAR_SLOT)?;

            // calldata component of commit fees (calldata gas + execution)
            let calldata_gas = l1_commit_scalar.saturating_mul(l1_base_fee);

            Ok(Self {
                l1_base_fee,
                l1_fee_overhead,
                l1_base_fee_scalar,
                l1_blob_base_fee: Some(l1_blob_base_fee),
                l1_commit_scalar: Some(l1_commit_scalar),
                l1_blob_scalar: Some(l1_blob_scalar),
                calldata_gas: Some(calldata_gas),
            })
        }
    }

    /// Calculate the data gas for posting the transaction on L1.
    ///
    /// Before Curie: Calldata costs 16 gas per non-zero byte and 4 gas per zero byte,
    /// plus overhead and extra commit cost.
    ///
    /// After Curie: Uses blob-based calculation with blob base fee and blob scalar.
    pub fn data_gas(&self, input: &[u8], hardfork: MorphHardfork) -> U256 {
        if !hardfork.is_curie() {
            U256::from(input.iter().fold(0, |acc, byte| {
                acc + if *byte == 0x00 {
                    ZERO_BYTE_COST
                } else {
                    NON_ZERO_BYTE_COST
                }
            }))
            .saturating_add(self.l1_fee_overhead)
            .saturating_add(TX_L1_COMMIT_EXTRA_COST)
        } else {
            U256::from(input.len())
                .saturating_mul(self.l1_blob_base_fee.unwrap_or_default())
                .saturating_mul(self.l1_blob_scalar.unwrap_or_default())
        }
    }

    /// Calculate L1 cost for a transaction before Curie hardfork.
    fn calculate_tx_l1_cost_pre_curie(&self, input: &[u8], hardfork: MorphHardfork) -> U256 {
        let tx_l1_gas = self.data_gas(input, hardfork);
        tx_l1_gas
            .saturating_mul(self.l1_base_fee)
            .saturating_mul(self.l1_base_fee_scalar)
            .wrapping_div(TX_L1_FEE_PRECISION)
    }

    /// Calculate L1 cost for a transaction after Curie hardfork.
    ///
    /// Formula: `commitScalar * l1BaseFee + blobScalar * _data.length * l1BlobBaseFee`
    fn calculate_tx_l1_cost_curie(&self, input: &[u8], hardfork: MorphHardfork) -> U256 {
        let blob_gas = self.data_gas(input, hardfork);

        self.calldata_gas
            .unwrap_or_default()
            .saturating_add(blob_gas)
            .wrapping_div(TX_L1_FEE_PRECISION)
    }

    /// Calculate the L1 data fee for a transaction.
    ///
    /// This is the cost of posting the transaction data to L1 for data availability.
    /// The calculation method differs based on whether the Curie hardfork is active.
    pub fn calculate_tx_l1_cost(&self, input: &[u8], hardfork: MorphHardfork) -> U256 {
        if !hardfork.is_curie() {
            self.calculate_tx_l1_cost_pre_curie(input, hardfork)
        } else {
            self.calculate_tx_l1_cost_curie(input, hardfork)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l1_block_info_default() {
        let info = L1BlockInfo::default();
        assert_eq!(info.l1_base_fee, U256::ZERO);
        assert_eq!(info.l1_fee_overhead, U256::ZERO);
        assert_eq!(info.l1_base_fee_scalar, U256::ZERO);
        assert!(info.l1_blob_base_fee.is_none());
        assert!(info.l1_commit_scalar.is_none());
        assert!(info.l1_blob_scalar.is_none());
        assert!(info.calldata_gas.is_none());
    }

    #[test]
    fn test_data_gas_pre_curie() {
        let info = L1BlockInfo {
            l1_fee_overhead: U256::from(100),
            ..Default::default()
        };

        // Test with mixed zero and non-zero bytes
        let input = vec![0x00, 0x01, 0x00, 0xff];
        // 2 zero bytes * 4 + 2 non-zero bytes * 16 + 100 overhead + 64 extra = 200
        let gas = info.data_gas(&input, MorphHardfork::Bernoulli);
        assert_eq!(gas, U256::from(2 * 4 + 2 * 16 + 100 + 64));
    }

    #[test]
    fn test_data_gas_curie() {
        let info = L1BlockInfo {
            l1_blob_base_fee: Some(U256::from(10)),
            l1_blob_scalar: Some(U256::from(2)),
            ..Default::default()
        };

        let input = vec![0x00, 0x01, 0x00, 0xff];
        // length * blob_base_fee * blob_scalar = 4 * 10 * 2 = 80
        let gas = info.data_gas(&input, MorphHardfork::Curie);
        assert_eq!(gas, U256::from(80));
    }

    #[test]
    fn test_calculate_tx_l1_cost_pre_curie() {
        let info = L1BlockInfo {
            l1_base_fee: U256::from(1_000_000_000), // 1 gwei
            l1_fee_overhead: U256::from(0),
            l1_base_fee_scalar: U256::from(1_000_000_000), // 1.0 scaled
            ..Default::default()
        };

        // 1 non-zero byte = 16 gas
        // + 64 extra cost = 80 gas total
        // cost = 80 * 1_000_000_000 * 1_000_000_000 / 1_000_000_000 = 80_000_000_000
        let input = vec![0xff];
        let cost = info.calculate_tx_l1_cost(&input, MorphHardfork::Bernoulli);
        assert_eq!(cost, U256::from(80_000_000_000u64));
    }
}
