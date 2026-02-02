//! Morph transaction receipt types.
//!
//! This module defines the Morph-specific receipt types that include:
//! - L1 data fee for rollup transactions
//! - Morph transaction fields for alternative fee token transactions

use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom, TxReceipt};
use alloy_primitives::{Bloom, Log, U256};
use alloy_rlp::{BufMut, Decodable, Encodable, Header};

/// Morph transaction receipt with L1 fee and Morph transaction fields.
///
/// This receipt extends the standard Ethereum receipt with:
/// - `l1_fee`: The L1 data fee charged for posting transaction data to L1
/// - `version`: The version of the Morph transaction format
/// - `fee_token_id`: The ERC20 token ID used for fee payment (TxMorph)
/// - `fee_rate`: The exchange rate for the fee token
/// - `token_scale`: The scale factor for the token
/// - `fee_limit`: The fee limit for TxMorph
/// - `reference`: The reference key for the transaction
/// - `memo`: The memo field for arbitrary data
///
/// Reference: <https://github.com/morph-l2/go-ethereum/pull/282>
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct MorphTransactionReceipt<T = Log> {
    /// The inner receipt type.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub inner: Receipt<T>,

    /// L1 fee for Morph transactions.
    /// This is the cost of posting the transaction data to L1.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub l1_fee: Option<U256>,

    /// The version of the Morph transaction format.
    /// Only present for TxMorph.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub version: Option<u8>,

    /// The ERC20 token ID used for fee payment (TxMorph feature).
    /// Only present for TxMorph.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub fee_token_id: Option<u16>,

    /// The exchange rate for the fee token.
    /// Only present for TxMorph.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub fee_rate: Option<U256>,

    /// The scale factor for the token.
    /// Only present for TxMorph.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub token_scale: Option<U256>,

    /// The fee limit for the TxMorph.
    /// Only present for TxMorph.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub fee_limit: Option<U256>,

    /// Reference key for the transaction.
    /// Used for indexing and looking up transactions by external systems.
    /// Only present for TxMorph.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub reference: Option<alloy_primitives::B256>,

    /// Memo field for arbitrary data.
    /// Only present for TxMorph.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub memo: Option<alloy_primitives::Bytes>,
}

impl<T> MorphTransactionReceipt<T> {
    /// Creates a new receipt with the given inner receipt.
    pub const fn new(inner: Receipt<T>) -> Self {
        Self {
            inner,
            l1_fee: None,
            version: None,
            fee_token_id: None,
            fee_rate: None,
            token_scale: None,
            fee_limit: None,
            reference: None,
            memo: None,
        }
    }

    /// Creates a new receipt with L1 fee.
    pub const fn with_l1_fee(inner: Receipt<T>, l1_fee: U256) -> Self {
        Self {
            inner,
            l1_fee: Some(l1_fee),
            version: None,
            fee_token_id: None,
            fee_rate: None,
            token_scale: None,
            fee_limit: None,
            reference: None,
            memo: None,
        }
    }

    /// Creates a new receipt with TxMorph fields (legacy version without reference/memo).
    pub const fn with_morph_tx(
        inner: Receipt<T>,
        l1_fee: U256,
        fee_token_id: u16,
        fee_rate: U256,
        token_scale: U256,
        fee_limit: U256,
    ) -> Self {
        Self {
            inner,
            l1_fee: Some(l1_fee),
            version: None,
            fee_token_id: Some(fee_token_id),
            fee_rate: Some(fee_rate),
            token_scale: Some(token_scale),
            fee_limit: Some(fee_limit),
            reference: None,
            memo: None,
        }
    }

    /// Creates a new receipt with all TxMorph fields including version, reference, and memo.
    #[allow(clippy::too_many_arguments)]
    pub const fn with_morph_tx_full(
        inner: Receipt<T>,
        l1_fee: U256,
        version: u8,
        fee_token_id: u16,
        fee_rate: U256,
        token_scale: U256,
        fee_limit: U256,
        reference: Option<alloy_primitives::B256>,
        memo: Option<alloy_primitives::Bytes>,
    ) -> Self {
        Self {
            inner,
            l1_fee: Some(l1_fee),
            version: Some(version),
            fee_token_id: Some(fee_token_id),
            fee_rate: Some(fee_rate),
            token_scale: Some(token_scale),
            fee_limit: Some(fee_limit),
            reference,
            memo,
        }
    }

    /// Returns true if this receipt is for a TxMorph.
    pub const fn is_morph_tx(&self) -> bool {
        self.fee_token_id.is_some() || self.version.is_some() || self.reference.is_some()
    }

    /// Returns true if this receipt has a reference.
    pub const fn has_reference(&self) -> bool {
        self.reference.is_some()
    }

    /// Returns the L1 fee, defaulting to zero if not set.
    pub fn l1_fee_or_zero(&self) -> U256 {
        self.l1_fee.unwrap_or(U256::ZERO)
    }

    /// Returns the version, defaulting to 0 if not set.
    pub fn version_or_zero(&self) -> u8 {
        self.version.unwrap_or(0)
    }
}

impl MorphTransactionReceipt {
    /// Calculates [`Log`]'s bloom filter.
    pub fn bloom_slow(&self) -> Bloom {
        self.inner.logs.iter().collect()
    }

    /// Calculates the bloom filter for the receipt and returns the [`ReceiptWithBloom`]
    /// container type.
    pub fn with_bloom(self) -> MorphReceiptWithBloom {
        self.into()
    }
}

impl<T: Encodable> MorphTransactionReceipt<T> {
    /// Returns length of RLP-encoded receipt fields with the given [`Bloom`] without an RLP header.
    ///
    /// Note: L1 fee and TxMorph fields are NOT included in the RLP encoding for consensus,
    /// matching go-ethereum's behavior.
    pub fn rlp_encoded_fields_length_with_bloom(&self, bloom: &Bloom) -> usize {
        self.inner.rlp_encoded_fields_length_with_bloom(bloom)
    }

    /// RLP-encodes receipt fields with the given [`Bloom`] without an RLP header.
    ///
    /// Note: L1 fee and TxMorph fields are NOT included in the RLP encoding for consensus,
    /// matching go-ethereum's behavior.
    pub fn rlp_encode_fields_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        self.inner.rlp_encode_fields_with_bloom(bloom, out);
    }

    /// Returns RLP header for this receipt encoding with the given [`Bloom`].
    pub fn rlp_header_with_bloom(&self, bloom: &Bloom) -> Header {
        Header {
            list: true,
            payload_length: self.rlp_encoded_fields_length_with_bloom(bloom),
        }
    }
}

impl<T: Decodable> MorphTransactionReceipt<T> {
    /// RLP-decodes receipt's field with a [`Bloom`].
    ///
    /// Does not expect an RLP header.
    pub fn rlp_decode_fields_with_bloom(
        buf: &mut &[u8],
    ) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let ReceiptWithBloom {
            receipt: inner,
            logs_bloom,
        } = Receipt::rlp_decode_fields_with_bloom(buf)?;

        Ok(ReceiptWithBloom {
            logs_bloom,
            receipt: Self::new(inner),
        })
    }
}

impl<T> AsRef<Receipt<T>> for MorphTransactionReceipt<T> {
    fn as_ref(&self) -> &Receipt<T> {
        &self.inner
    }
}

impl<T> TxReceipt for MorphTransactionReceipt<T>
where
    T: AsRef<Log> + Clone + core::fmt::Debug + PartialEq + Eq + Send + Sync,
{
    type Log = T;

    fn status_or_post_state(&self) -> Eip658Value {
        self.inner.status_or_post_state()
    }

    fn status(&self) -> bool {
        self.inner.status()
    }

    fn bloom(&self) -> Bloom {
        self.inner.bloom_slow()
    }

    fn cumulative_gas_used(&self) -> u64 {
        self.inner.cumulative_gas_used()
    }

    fn logs(&self) -> &[Self::Log] {
        self.inner.logs()
    }
}

impl<T> From<Receipt<T>> for MorphTransactionReceipt<T> {
    fn from(inner: Receipt<T>) -> Self {
        Self::new(inner)
    }
}

/// [`MorphTransactionReceipt`] with calculated bloom filter.
pub type MorphReceiptWithBloom<T = Log> = ReceiptWithBloom<MorphTransactionReceipt<T>>;

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{LogData, address, b256, bytes};

    fn create_test_log() -> Log {
        Log {
            address: address!("0000000000000000000000000000000000000011"),
            data: LogData::new_unchecked(
                vec![
                    b256!("000000000000000000000000000000000000000000000000000000000000dead"),
                    b256!("000000000000000000000000000000000000000000000000000000000000beef"),
                ],
                bytes!("0100ff"),
            ),
        }
    }

    #[test]
    fn test_morph_receipt_new() {
        let inner = Receipt {
            status: true.into(),
            cumulative_gas_used: 21000,
            logs: vec![create_test_log()],
        };
        let receipt = MorphTransactionReceipt::new(inner);

        assert!(receipt.status());
        assert_eq!(receipt.cumulative_gas_used(), 21000);
        assert!(receipt.l1_fee.is_none());
        assert!(receipt.fee_token_id.is_none());
        assert!(!receipt.is_morph_tx());
    }

    #[test]
    fn test_morph_receipt_with_l1_fee() {
        let inner: Receipt<Log> = Receipt {
            status: true.into(),
            cumulative_gas_used: 21000,
            logs: vec![],
        };
        let l1_fee = U256::from(1000000);
        let receipt = MorphTransactionReceipt::with_l1_fee(inner, l1_fee);

        assert_eq!(receipt.l1_fee, Some(l1_fee));
        assert_eq!(receipt.l1_fee_or_zero(), l1_fee);
        assert!(!receipt.is_morph_tx());
    }

    #[test]
    fn test_morph_receipt_with_morph_tx() {
        let inner: Receipt<Log> = Receipt {
            status: true.into(),
            cumulative_gas_used: 21000,
            logs: vec![],
        };
        let l1_fee = U256::from(1000000);
        let fee_token_id = 1u16;
        let fee_rate = U256::from(100);
        let token_scale = U256::from(18);
        let fee_limit = U256::from(5000000);

        let receipt = MorphTransactionReceipt::with_morph_tx(
            inner,
            l1_fee,
            fee_token_id,
            fee_rate,
            token_scale,
            fee_limit,
        );

        assert!(receipt.is_morph_tx());
        assert_eq!(receipt.fee_token_id, Some(fee_token_id));
        assert_eq!(receipt.fee_rate, Some(fee_rate));
        assert_eq!(receipt.token_scale, Some(token_scale));
        assert_eq!(receipt.fee_limit, Some(fee_limit));
    }

    #[test]
    fn test_morph_receipt_bloom() {
        let inner = Receipt {
            status: true.into(),
            cumulative_gas_used: 21000,
            logs: vec![create_test_log()],
        };
        let receipt = MorphTransactionReceipt::new(inner);

        let bloom = receipt.bloom_slow();
        assert_ne!(bloom, Bloom::default());
    }

    #[test]
    fn test_morph_receipt_with_bloom() {
        let inner = Receipt {
            status: true.into(),
            cumulative_gas_used: 21000,
            logs: vec![create_test_log()],
        };
        let receipt = MorphTransactionReceipt::new(inner);
        let receipt_with_bloom = receipt.with_bloom();

        assert_ne!(receipt_with_bloom.logs_bloom, Bloom::default());
    }
}
