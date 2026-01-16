//! L1 Block Info for Morph L2 fee calculation.
//!
//! This module provides the infrastructure for calculating L1 data fees on Morph L2.
//! The fee parameters are read from the L1 Gas Price Oracle contract deployed on L2.
//!
//! L1 fees are calculated using the blob-based model with commit scalar and blob scalar.

use alloy_primitives::{Address, U256, address};
use revm::Database;

/// Precision factor for L1 fee calculation (1e9).
const TX_L1_FEE_PRECISION: U256 = U256::from_limbs([1_000_000_000u64, 0, 0, 0]);

/// L1 Gas Price Oracle contract address on Morph L2.
pub const L1_GAS_PRICE_ORACLE_ADDRESS: Address =
    address!("530000000000000000000000000000000000000F");

/// Storage slot for L1 base fee.
const L1_BASE_FEE_SLOT: U256 = U256::from_limbs([1u64, 0, 0, 0]);
/// Storage slot for L1 blob base fee.
const L1_BLOB_BASE_FEE_SLOT: U256 = U256::from_limbs([6u64, 0, 0, 0]);
/// Storage slot for L1 commit scalar.
const L1_COMMIT_SCALAR_SLOT: U256 = U256::from_limbs([7u64, 0, 0, 0]);
/// Storage slot for L1 blob scalar.
const L1_BLOB_SCALAR_SLOT: U256 = U256::from_limbs([8u64, 0, 0, 0]);

/// L1 block info for fee calculation.
///
/// Contains the fee parameters fetched from the L1 Gas Price Oracle contract.
/// These parameters are used to calculate the L1 data fee for transactions
/// using the blob-based model.
#[derive(Clone, Debug, Default)]
pub struct L1BlockInfo {
    /// The base fee of the L1 origin block.
    pub l1_base_fee: U256,
    /// The current L1 blob base fee.
    pub l1_blob_base_fee: U256,
    /// The current L1 commit scalar.
    pub l1_commit_scalar: U256,
    /// The current L1 blob scalar.
    pub l1_blob_scalar: U256,
    /// Pre-computed calldata gas component (l1_commit_scalar * l1_base_fee).
    pub calldata_gas: U256,
}

impl L1BlockInfo {
    /// Try to fetch the L1 block info from the database.
    ///
    /// This reads the fee parameters from the L1 Gas Price Oracle contract storage
    /// for blob-based L1 fee calculation.
    pub fn try_fetch<DB: Database>(db: &mut DB) -> Result<Self, DB::Error> {
        let l1_base_fee = db.storage(L1_GAS_PRICE_ORACLE_ADDRESS, L1_BASE_FEE_SLOT)?;
        let l1_blob_base_fee = db.storage(L1_GAS_PRICE_ORACLE_ADDRESS, L1_BLOB_BASE_FEE_SLOT)?;
        let l1_commit_scalar = db.storage(L1_GAS_PRICE_ORACLE_ADDRESS, L1_COMMIT_SCALAR_SLOT)?;
        let l1_blob_scalar = db.storage(L1_GAS_PRICE_ORACLE_ADDRESS, L1_BLOB_SCALAR_SLOT)?;

        // Pre-compute calldata gas component: commitScalar * l1BaseFee
        let calldata_gas = l1_commit_scalar.saturating_mul(l1_base_fee);

        Ok(Self {
            l1_base_fee,
            l1_blob_base_fee,
            l1_commit_scalar,
            l1_blob_scalar,
            calldata_gas,
        })
    }

    /// Calculate the blob gas cost for transaction data.
    ///
    /// Formula: `data.length * l1BlobBaseFee * blobScalar`
    pub fn blob_gas(&self, input: &[u8]) -> U256 {
        U256::from(input.len())
            .saturating_mul(self.l1_blob_base_fee)
            .saturating_mul(self.l1_blob_scalar)
    }

    /// Calculate the L1 data fee for a transaction.
    ///
    /// This is the cost of posting the transaction data to L1 for data availability.
    ///
    /// Formula: `(commitScalar * l1BaseFee + blobScalar * data.length * l1BlobBaseFee) / precision`
    pub fn calculate_tx_l1_cost(&self, input: &[u8]) -> U256 {
        let blob_gas = self.blob_gas(input);

        self.calldata_gas
            .saturating_add(blob_gas)
            .wrapping_div(TX_L1_FEE_PRECISION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l1_block_info_default() {
        let info = L1BlockInfo::default();
        assert_eq!(info.l1_base_fee, U256::ZERO);
        assert_eq!(info.l1_blob_base_fee, U256::ZERO);
        assert_eq!(info.l1_commit_scalar, U256::ZERO);
        assert_eq!(info.l1_blob_scalar, U256::ZERO);
        assert_eq!(info.calldata_gas, U256::ZERO);
    }

    #[test]
    fn test_blob_gas() {
        let info = L1BlockInfo {
            l1_blob_base_fee: U256::from(10),
            l1_blob_scalar: U256::from(2),
            ..Default::default()
        };

        let input = vec![0x00, 0x01, 0x00, 0xff];
        // length * blob_base_fee * blob_scalar = 4 * 10 * 2 = 80
        let gas = info.blob_gas(&input);
        assert_eq!(gas, U256::from(80));
    }

    #[test]
    fn test_calculate_tx_l1_cost() {
        let l1_base_fee = U256::from(1_000_000_000); // 1 gwei
        let l1_commit_scalar = U256::from(1_000_000_000); // 1.0 scaled
        let l1_blob_base_fee = U256::from(10);
        let l1_blob_scalar = U256::from(2);

        let info = L1BlockInfo {
            l1_base_fee,
            l1_blob_base_fee,
            l1_commit_scalar,
            l1_blob_scalar,
            calldata_gas: l1_commit_scalar.saturating_mul(l1_base_fee),
        };

        // Formula: (commitScalar * l1BaseFee + blobScalar * data.length * l1BlobBaseFee) / precision
        // = (1e9 * 1e9 + 2 * 4 * 10) / 1e9
        // = (1e18 + 80) / 1e9
        // = 1_000_000_000 (the +80 is negligible)
        let input = vec![0x00, 0x01, 0x00, 0xff];
        let cost = info.calculate_tx_l1_cost(&input);
        assert_eq!(cost, U256::from(1_000_000_000u64));
    }
}
