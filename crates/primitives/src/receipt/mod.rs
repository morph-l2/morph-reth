//! Morph receipt types.
//!
//! This module provides:
//! - [`MorphTransactionReceipt`]: Receipt with L1 fee and Morph transaction fields
//! - [`MorphReceipt`]: Typed receipt enum for different transaction types

mod envelope;
#[allow(clippy::module_inception)]
mod receipt;
pub use envelope::MorphReceiptEnvelope;
pub use receipt::{MorphReceiptWithBloom, MorphTransactionReceipt};

use crate::transaction::envelope::MorphTxType;
use alloy_consensus::{
    Eip2718EncodableReceipt, Receipt, ReceiptWithBloom, RlpDecodableReceipt, RlpEncodableReceipt,
    TxReceipt, Typed2718,
};
use alloy_eips::eip2718::{Decodable2718, Eip2718Result, Encodable2718};
use alloy_primitives::{B256, Bloom, Log};
use alloy_rlp::{BufMut, Decodable, Encodable, Header};
use reth_primitives_traits::InMemorySize;

/// Morph typed receipt.
///
/// This enum wraps different receipt types based on the transaction type.
/// For L1 messages, it uses a standard receipt without L1 fee.
/// For other transactions, it uses [`MorphTransactionReceipt`] with L1 fee and optional Morph transaction fields.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MorphReceipt {
    /// Legacy receipt
    Legacy(MorphTransactionReceipt),
    /// EIP-2930 receipt
    Eip2930(MorphTransactionReceipt),
    /// EIP-1559 receipt
    Eip1559(MorphTransactionReceipt),
    /// EIP-7702 receipt
    Eip7702(MorphTransactionReceipt),
    /// L1 message receipt (no L1 fee since it's pre-paid on L1)
    L1Msg(Receipt),
    /// Morph transaction receipt
    Morph(MorphTransactionReceipt),
}

impl Default for MorphReceipt {
    fn default() -> Self {
        Self::Legacy(MorphTransactionReceipt::default())
    }
}

impl MorphReceipt {
    /// Returns [`MorphTxType`] of the receipt.
    pub const fn tx_type(&self) -> MorphTxType {
        match self {
            Self::Legacy(_) => MorphTxType::Legacy,
            Self::Eip2930(_) => MorphTxType::Eip2930,
            Self::Eip1559(_) => MorphTxType::Eip1559,
            Self::Eip7702(_) => MorphTxType::Eip7702,
            Self::L1Msg(_) => MorphTxType::L1Msg,
            Self::Morph(_) => MorphTxType::Morph,
        }
    }

    /// Returns inner [`Receipt`].
    pub const fn as_receipt(&self) -> &Receipt {
        match self {
            Self::Legacy(receipt)
            | Self::Eip2930(receipt)
            | Self::Eip1559(receipt)
            | Self::Eip7702(receipt)
            | Self::Morph(receipt) => &receipt.inner,
            Self::L1Msg(receipt) => receipt,
        }
    }

    /// Returns the L1 fee for the receipt.
    pub fn l1_fee(&self) -> alloy_primitives::U256 {
        match self {
            Self::Legacy(r)
            | Self::Eip2930(r)
            | Self::Eip1559(r)
            | Self::Eip7702(r)
            | Self::Morph(r) => r.l1_fee,
            Self::L1Msg(_) => alloy_primitives::U256::ZERO,
        }
    }

    /// Returns true if this is an L1 message receipt.
    pub const fn is_l1_message(&self) -> bool {
        matches!(self, Self::L1Msg(_))
    }

    /// Returns length of RLP-encoded receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encoded_fields_length(&self, bloom: &Bloom) -> usize {
        match self {
            Self::Legacy(r)
            | Self::Eip2930(r)
            | Self::Eip1559(r)
            | Self::Eip7702(r)
            | Self::Morph(r) => r.rlp_encoded_fields_length_with_bloom(bloom),
            Self::L1Msg(r) => r.rlp_encoded_fields_length_with_bloom(bloom),
        }
    }

    /// RLP-encodes receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encode_fields(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        match self {
            Self::Legacy(r)
            | Self::Eip2930(r)
            | Self::Eip1559(r)
            | Self::Eip7702(r)
            | Self::Morph(r) => r.rlp_encode_fields_with_bloom(bloom, out),
            Self::L1Msg(r) => r.rlp_encode_fields_with_bloom(bloom, out),
        }
    }

    /// Returns RLP header for inner encoding.
    pub fn rlp_header_inner(&self, bloom: &Bloom) -> Header {
        Header {
            list: true,
            payload_length: self.rlp_encoded_fields_length(bloom),
        }
    }

    /// Returns RLP header for inner encoding without bloom.
    ///
    /// Used for DA (data availability) layer compression where bloom is omitted to save space.
    pub fn rlp_header_inner_without_bloom(&self) -> Header {
        Header {
            list: true,
            payload_length: self.rlp_encoded_fields_length_without_bloom(),
        }
    }

    /// Returns length of RLP-encoded receipt fields without bloom and without an RLP header.
    ///
    /// The fields are: `[status, cumulative_gas_used, logs]` (no bloom).
    /// Used for DA layer compression.
    pub fn rlp_encoded_fields_length_without_bloom(&self) -> usize {
        match self {
            Self::Legacy(r)
            | Self::Eip2930(r)
            | Self::Eip1559(r)
            | Self::Eip7702(r)
            | Self::Morph(r) => {
                r.inner.status.length()
                    + r.inner.cumulative_gas_used.length()
                    + r.inner.logs.length()
            }
            Self::L1Msg(r) => r.status.length() + r.cumulative_gas_used.length() + r.logs.length(),
        }
    }

    /// RLP-encodes receipt fields without bloom and without an RLP header.
    ///
    /// Encodes: `[status, cumulative_gas_used, logs]` (no bloom).
    /// Used for DA layer compression.
    pub fn rlp_encode_fields_without_bloom(&self, out: &mut dyn BufMut) {
        match self {
            Self::Legacy(r)
            | Self::Eip2930(r)
            | Self::Eip1559(r)
            | Self::Eip7702(r)
            | Self::Morph(r) => {
                r.inner.status.encode(out);
                r.inner.cumulative_gas_used.encode(out);
                r.inner.logs.encode(out);
            }
            Self::L1Msg(r) => {
                r.status.encode(out);
                r.cumulative_gas_used.encode(out);
                r.logs.encode(out);
            }
        }
    }

    /// RLP-decodes the receipt from the provided buffer without bloom.
    ///
    /// Expects format: `[status, cumulative_gas_used, logs]` (no bloom).
    /// Used for DA layer decompression.
    pub fn rlp_decode_inner_without_bloom(
        buf: &mut &[u8],
        tx_type: MorphTxType,
    ) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        let remaining = buf.len();
        let status = Decodable::decode(buf)?;
        let cumulative_gas_used = Decodable::decode(buf)?;
        let logs = Decodable::decode(buf)?;

        if buf.len() + header.payload_length != remaining {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        let inner = Receipt {
            status,
            cumulative_gas_used,
            logs,
        };

        match tx_type {
            MorphTxType::Legacy => Ok(Self::Legacy(MorphTransactionReceipt::new(inner))),
            MorphTxType::Eip2930 => Ok(Self::Eip2930(MorphTransactionReceipt::new(inner))),
            MorphTxType::Eip1559 => Ok(Self::Eip1559(MorphTransactionReceipt::new(inner))),
            MorphTxType::Eip7702 => Ok(Self::Eip7702(MorphTransactionReceipt::new(inner))),
            MorphTxType::L1Msg => Ok(Self::L1Msg(inner)),
            MorphTxType::Morph => Ok(Self::Morph(MorphTransactionReceipt::new(inner))),
        }
    }

    /// RLP-decodes the receipt from the provided buffer. This does not expect a type byte or
    /// network header.
    fn rlp_decode_inner(
        buf: &mut &[u8],
        tx_type: MorphTxType,
    ) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        // Decode using standard Receipt, then wrap in appropriate MorphReceipt variant
        let ReceiptWithBloom {
            receipt: inner,
            logs_bloom,
        } = Receipt::rlp_decode_with_bloom(buf)?;

        let receipt = match tx_type {
            MorphTxType::Legacy => Self::Legacy(MorphTransactionReceipt::new(inner)),
            MorphTxType::Eip2930 => Self::Eip2930(MorphTransactionReceipt::new(inner)),
            MorphTxType::Eip1559 => Self::Eip1559(MorphTransactionReceipt::new(inner)),
            MorphTxType::Eip7702 => Self::Eip7702(MorphTransactionReceipt::new(inner)),
            MorphTxType::L1Msg => Self::L1Msg(inner),
            MorphTxType::Morph => Self::Morph(MorphTransactionReceipt::new(inner)),
        };

        Ok(ReceiptWithBloom {
            receipt,
            logs_bloom,
        })
    }
}

impl TxReceipt for MorphReceipt {
    type Log = Log;

    fn status_or_post_state(&self) -> alloy_consensus::Eip658Value {
        self.as_receipt().status_or_post_state()
    }

    fn status(&self) -> bool {
        self.as_receipt().status()
    }

    fn bloom(&self) -> Bloom {
        self.as_receipt().bloom()
    }

    fn cumulative_gas_used(&self) -> u64 {
        self.as_receipt().cumulative_gas_used()
    }

    fn logs(&self) -> &[Log] {
        self.as_receipt().logs()
    }
}

impl Typed2718 for MorphReceipt {
    fn ty(&self) -> u8 {
        self.tx_type().into()
    }
}

impl Eip2718EncodableReceipt for MorphReceipt {
    fn eip2718_encoded_length_with_bloom(&self, bloom: &Bloom) -> usize {
        !self.tx_type().is_legacy() as usize + self.rlp_header_inner(bloom).length_with_payload()
    }

    fn eip2718_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        if !self.tx_type().is_legacy() {
            out.put_u8(self.tx_type().into());
        }
        self.rlp_header_inner(bloom).encode(out);
        self.rlp_encode_fields(bloom, out);
    }
}

impl RlpEncodableReceipt for MorphReceipt {
    fn rlp_encoded_length_with_bloom(&self, bloom: &Bloom) -> usize {
        let mut len = self.eip2718_encoded_length_with_bloom(bloom);
        if !self.tx_type().is_legacy() {
            // For typed receipts, add string header length
            len += Header {
                list: false,
                payload_length: self.eip2718_encoded_length_with_bloom(bloom),
            }
            .length();
        }
        len
    }

    fn rlp_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        if !self.tx_type().is_legacy() {
            // For typed receipts, write string header first
            Header {
                list: false,
                payload_length: self.eip2718_encoded_length_with_bloom(bloom),
            }
            .encode(out);
        }
        self.eip2718_encode_with_bloom(bloom, out);
    }
}

impl RlpDecodableReceipt for MorphReceipt {
    fn rlp_decode_with_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let header_buf = &mut &**buf;
        let header = Header::decode(header_buf)?;

        // Legacy receipt: header.list = true (directly an RLP list)
        if header.list {
            return Self::rlp_decode_inner(buf, MorphTxType::Legacy);
        }

        // Typed receipt: header.list = false (string containing type + RLP list)
        // Advance the buffer past the header
        *buf = *header_buf;

        let remaining = buf.len();
        let tx_type = MorphTxType::rlp_decode(buf)?;
        let this = Self::rlp_decode_inner(buf, tx_type)?;

        if buf.len() + header.payload_length != remaining {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        Ok(this)
    }
}

impl Encodable2718 for MorphReceipt {
    /// Returns the length of the EIP-2718 encoded receipt without bloom.
    ///
    /// Format: `[type_byte] + RLP([status, cumulative_gas_used, logs])`
    ///
    /// Bloom is omitted for DA layer compression - it can be recalculated from logs.
    fn encode_2718_len(&self) -> usize {
        !self.tx_type().is_legacy() as usize
            + self.rlp_header_inner_without_bloom().length_with_payload()
    }

    /// EIP-2718 encodes the receipt without bloom.
    ///
    /// Format: `[type_byte] + RLP([status, cumulative_gas_used, logs])`
    ///
    /// Bloom is omitted for DA layer compression - it can be recalculated from logs.
    fn encode_2718(&self, out: &mut dyn BufMut) {
        if !self.tx_type().is_legacy() {
            out.put_u8(self.tx_type().into());
        }
        self.rlp_header_inner_without_bloom().encode(out);
        self.rlp_encode_fields_without_bloom(out);
    }
}

impl Decodable2718 for MorphReceipt {
    /// Decodes a typed receipt without bloom.
    ///
    /// Expects format: `RLP([status, cumulative_gas_used, logs])` (no bloom).
    /// This matches the encoding from `encode_2718`.
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        let tx_type = MorphTxType::try_from(ty)
            .map_err(|_| alloy_eips::eip2718::Eip2718Error::UnexpectedType(ty))?;

        Ok(Self::rlp_decode_inner_without_bloom(buf, tx_type)?)
    }

    /// Decodes a legacy receipt without bloom.
    ///
    /// Expects format: `RLP([status, cumulative_gas_used, logs])` (no bloom).
    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(Self::rlp_decode_inner_without_bloom(
            buf,
            MorphTxType::Legacy,
        )?)
    }
}

impl alloy_rlp::Encodable for MorphReceipt {
    /// Encodes the receipt for P2P network transmission.
    ///
    /// Uses `network_encode` which wraps typed receipts in an additional RLP string header,
    /// as required by the eth wire protocol (eth/66, eth/67).
    fn encode(&self, out: &mut dyn BufMut) {
        self.network_encode(out);
    }

    fn length(&self) -> usize {
        self.network_len()
    }
}

impl alloy_rlp::Decodable for MorphReceipt {
    /// Decodes the receipt from P2P network format.
    ///
    /// Uses `network_decode` which expects typed receipts to be wrapped in an RLP string header.
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::network_decode(buf).map_err(|_| alloy_rlp::Error::Custom("Failed to decode receipt"))
    }
}

impl InMemorySize for MorphReceipt {
    fn size(&self) -> usize {
        self.as_receipt().size()
    }
}

#[cfg(feature = "serde-bincode-compat")]
impl reth_primitives_traits::serde_bincode_compat::RlpBincode for MorphReceipt {}

/// Calculates the receipt root for a header.
///
/// This function computes the Merkle root of receipts using the standard encoding
/// that includes the bloom filter, which is required for consensus validation.
///
/// NOTE: Prefer `alloy_consensus::proofs::calculate_receipt_root` if you have
/// log blooms already memoized (e.g., from bloom validation).
///
/// # Example
///
/// ```
/// use morph_primitives::receipt::{MorphReceipt, calculate_receipt_root_no_memo};
/// use alloy_consensus::Receipt;
///
/// let receipts: Vec<MorphReceipt> = vec![];
/// let receipts_root = calculate_receipt_root_no_memo(&receipts);
/// ```
pub fn calculate_receipt_root_no_memo(receipts: &[MorphReceipt]) -> B256 {
    alloy_consensus::proofs::ordered_trie_root_with_encoder(receipts, |r, buf| {
        r.with_bloom_ref().encode_2718(buf)
    })
}

#[cfg(feature = "reth-codec")]
mod compact {
    use super::*;
    use alloy_primitives::U256;
    use reth_codecs::Compact;
    use std::borrow::Cow;

    /// Compact representation of [`MorphReceipt`] for database storage.
    ///
    /// Note: `tx_type` must be the last field because it's not a known fixed-size type
    /// for the CompactZstd derive macro.
    ///
    /// Note: `fee_token_id` is stored as `u64` instead of `u16` because `u16` doesn't implement
    /// `Compact` in reth_codecs. The conversion is lossless since `u16` fits in `u64`.
    #[derive(reth_codecs::CompactZstd)]
    #[reth_zstd(
        compressor = reth_zstd_compressors::RECEIPT_COMPRESSOR,
        decompressor = reth_zstd_compressors::RECEIPT_DECOMPRESSOR
    )]
    struct CompactMorphReceipt<'a> {
        success: bool,
        cumulative_gas_used: u64,
        #[allow(clippy::owned_cow)]
        logs: Cow<'a, Vec<Log>>,
        l1_fee: Option<U256>,
        /// Stored as u64 for Compact compatibility (u16 doesn't implement Compact)
        fee_token_id: Option<u64>,
        fee_rate: Option<U256>,
        token_scale: Option<U256>,
        fee_limit: Option<U256>,
        /// Must be the last field - not a known fixed-size type
        tx_type: MorphTxType,
    }

    impl<'a> From<&'a MorphReceipt> for CompactMorphReceipt<'a> {
        fn from(receipt: &'a MorphReceipt) -> Self {
            let (l1_fee, fee_token_id, fee_rate, token_scale, fee_limit) = match receipt {
                MorphReceipt::Legacy(r)
                | MorphReceipt::Eip2930(r)
                | MorphReceipt::Eip1559(r)
                | MorphReceipt::Eip7702(r)
                | MorphReceipt::Morph(r) => (
                    (r.l1_fee != U256::ZERO).then_some(r.l1_fee),
                    r.fee_token_id.map(u64::from),
                    r.fee_rate,
                    r.token_scale,
                    r.fee_limit,
                ),
                MorphReceipt::L1Msg(_) => (None, None, None, None, None),
            };

            Self {
                success: receipt.status(),
                cumulative_gas_used: receipt.cumulative_gas_used(),
                logs: Cow::Borrowed(&receipt.as_receipt().logs),
                l1_fee,
                fee_token_id,
                fee_rate,
                token_scale,
                fee_limit,
                tx_type: receipt.tx_type(),
            }
        }
    }

    impl From<CompactMorphReceipt<'_>> for MorphReceipt {
        fn from(receipt: CompactMorphReceipt<'_>) -> Self {
            let CompactMorphReceipt {
                success,
                cumulative_gas_used,
                logs,
                l1_fee,
                fee_token_id,
                fee_rate,
                token_scale,
                fee_limit,
                tx_type,
            } = receipt;

            let inner = Receipt {
                status: success.into(),
                cumulative_gas_used,
                logs: logs.into_owned(),
            };

            // L1Msg uses plain Receipt, others use MorphTransactionReceipt
            if tx_type == MorphTxType::L1Msg {
                return Self::L1Msg(inner);
            }

            let morph_receipt = MorphTransactionReceipt {
                inner,
                l1_fee: l1_fee.unwrap_or_default(),
                fee_token_id: fee_token_id.map(|id| id as u16),
                fee_rate,
                token_scale,
                fee_limit,
            };

            match tx_type {
                MorphTxType::Legacy => Self::Legacy(morph_receipt),
                MorphTxType::Eip2930 => Self::Eip2930(morph_receipt),
                MorphTxType::Eip1559 => Self::Eip1559(morph_receipt),
                MorphTxType::Eip7702 => Self::Eip7702(morph_receipt),
                MorphTxType::L1Msg => unreachable!("L1Msg handled above"),
                MorphTxType::Morph => Self::Morph(morph_receipt),
            }
        }
    }

    impl Compact for MorphReceipt {
        fn to_compact<B>(&self, buf: &mut B) -> usize
        where
            B: bytes::BufMut + AsMut<[u8]>,
        {
            CompactMorphReceipt::from(self).to_compact(buf)
        }

        fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
            let (receipt, buf) = CompactMorphReceipt::from_compact(buf, len);
            (receipt.into(), buf)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{LogData, U256, address, b256, bytes};

    /// Creates a test receipt with logs for encoding/decoding tests.
    fn create_test_receipt() -> MorphReceipt {
        let inner = Receipt {
            status: true.into(),
            cumulative_gas_used: 21000,
            logs: vec![Log {
                address: address!("0000000000000000000000000000000000000011"),
                data: LogData::new_unchecked(
                    vec![b256!(
                        "000000000000000000000000000000000000000000000000000000000000dead"
                    )],
                    bytes!("0100ff"),
                ),
            }],
        };

        MorphReceipt::Eip1559(MorphTransactionReceipt::with_l1_fee(
            inner,
            U256::from(1000),
        ))
    }

    /// Creates a legacy receipt for testing.
    fn create_legacy_receipt() -> MorphReceipt {
        let inner = Receipt {
            status: true.into(),
            cumulative_gas_used: 21000,
            logs: vec![Log {
                address: address!("0000000000000000000000000000000000000011"),
                data: LogData::new_unchecked(
                    vec![b256!(
                        "000000000000000000000000000000000000000000000000000000000000beef"
                    )],
                    bytes!("deadbeef"),
                ),
            }],
        };

        MorphReceipt::Legacy(MorphTransactionReceipt::new(inner))
    }

    /// Creates an L1 message receipt for testing.
    fn create_l1_msg_receipt() -> MorphReceipt {
        MorphReceipt::L1Msg(Receipt {
            status: true.into(),
            cumulative_gas_used: 100000,
            logs: vec![],
        })
    }

    /// Creates a Morph transaction receipt for testing.
    fn create_morph_receipt() -> MorphReceipt {
        let inner = Receipt {
            status: false.into(),
            cumulative_gas_used: 50000,
            logs: vec![],
        };

        MorphReceipt::Morph(MorphTransactionReceipt::with_morph_tx(
            inner,
            U256::from(2000),   // l1_fee
            1,                  // fee_token_id
            U256::from(100),    // fee_rate
            U256::from(18),     // token_scale
            U256::from(500000), // fee_limit
        ))
    }

    #[test]
    fn test_receipt_tx_type() {
        let legacy = MorphReceipt::Legacy(MorphTransactionReceipt::default());
        assert_eq!(legacy.tx_type(), MorphTxType::Legacy);

        let l1_msg = MorphReceipt::L1Msg(Receipt::default());
        assert_eq!(l1_msg.tx_type(), MorphTxType::L1Msg);

        let morph = MorphReceipt::Morph(MorphTransactionReceipt::default());
        assert_eq!(morph.tx_type(), MorphTxType::Morph);
    }

    #[test]
    fn test_receipt_l1_fee() {
        let receipt = create_test_receipt();
        assert_eq!(receipt.l1_fee(), U256::from(1000));

        let l1_msg = MorphReceipt::L1Msg(Receipt::default());
        assert_eq!(l1_msg.l1_fee(), U256::ZERO);
    }

    #[test]
    fn test_receipt_is_l1_message() {
        let receipt = create_test_receipt();
        assert!(!receipt.is_l1_message());

        let l1_msg = MorphReceipt::L1Msg(Receipt::default());
        assert!(l1_msg.is_l1_message());
    }

    #[test]
    fn test_receipt_status() {
        let receipt = create_test_receipt();
        assert!(receipt.status());
    }

    #[test]
    fn test_receipt_in_memory_size() {
        let receipt = create_test_receipt();
        let size = receipt.size();
        assert!(size > 0);
    }

    // ==================== Encode/Decode Tests ====================

    /// Tests that EIP-2718 encoding and decoding roundtrips correctly for EIP-1559 receipt.
    ///
    /// This tests the without-bloom encoding used for DA compression:
    /// - encode_2718: encodes [status, gas, logs] without bloom
    /// - decode_2718: decodes the same format
    #[test]
    fn test_eip1559_receipt_encode_2718_roundtrip() {
        let original = create_test_receipt();

        // Encode using EIP-2718 (without bloom)
        let mut encoded = Vec::new();
        original.encode_2718(&mut encoded);

        // Verify type byte is present for typed receipt
        assert_eq!(encoded[0], MorphTxType::Eip1559 as u8);

        // Decode
        let decoded = MorphReceipt::decode_2718(&mut encoded.as_slice()).unwrap();

        // Verify core fields match (l1_fee is not encoded, so it won't roundtrip)
        assert_eq!(decoded.tx_type(), original.tx_type());
        assert_eq!(decoded.status(), original.status());
        assert_eq!(
            decoded.cumulative_gas_used(),
            original.cumulative_gas_used()
        );
        assert_eq!(decoded.logs().len(), original.logs().len());
    }

    /// Tests that EIP-2718 encoding and decoding roundtrips correctly for legacy receipt.
    ///
    /// Legacy receipts have no type byte prefix.
    #[test]
    fn test_legacy_receipt_encode_2718_roundtrip() {
        let original = create_legacy_receipt();

        // Encode using EIP-2718 (without bloom)
        let mut encoded = Vec::new();
        original.encode_2718(&mut encoded);

        // Verify no type byte for legacy (first byte should be RLP list marker >= 0xc0)
        assert!(
            encoded[0] >= 0xc0,
            "Legacy receipt should start with RLP list marker"
        );

        // Decode
        let decoded = MorphReceipt::decode_2718(&mut encoded.as_slice()).unwrap();

        // Verify fields match
        assert_eq!(decoded.tx_type(), MorphTxType::Legacy);
        assert_eq!(decoded.status(), original.status());
        assert_eq!(
            decoded.cumulative_gas_used(),
            original.cumulative_gas_used()
        );
        assert_eq!(decoded.logs().len(), original.logs().len());
    }

    /// Tests that EIP-2718 encoding and decoding roundtrips correctly for L1 message receipt.
    #[test]
    fn test_l1_msg_receipt_encode_2718_roundtrip() {
        let original = create_l1_msg_receipt();

        // Encode
        let mut encoded = Vec::new();
        original.encode_2718(&mut encoded);

        // Verify type byte
        assert_eq!(encoded[0], MorphTxType::L1Msg as u8);

        // Decode
        let decoded = MorphReceipt::decode_2718(&mut encoded.as_slice()).unwrap();

        // Verify fields
        assert_eq!(decoded.tx_type(), MorphTxType::L1Msg);
        assert_eq!(decoded.status(), original.status());
        assert_eq!(
            decoded.cumulative_gas_used(),
            original.cumulative_gas_used()
        );
        assert!(decoded.is_l1_message());
    }

    /// Tests that EIP-2718 encoding and decoding roundtrips correctly for Morph receipt.
    #[test]
    fn test_morph_receipt_encode_2718_roundtrip() {
        let original = create_morph_receipt();

        // Encode
        let mut encoded = Vec::new();
        original.encode_2718(&mut encoded);

        // Verify type byte
        assert_eq!(encoded[0], MorphTxType::Morph as u8);

        // Decode
        let decoded = MorphReceipt::decode_2718(&mut encoded.as_slice()).unwrap();

        // Verify fields
        assert_eq!(decoded.tx_type(), MorphTxType::Morph);
        assert_eq!(decoded.status(), original.status());
        assert_eq!(
            decoded.cumulative_gas_used(),
            original.cumulative_gas_used()
        );
    }

    /// Tests network encoding (P2P format) roundtrip.
    ///
    /// Network encoding wraps typed receipts in an additional RLP string header,
    /// as required by the eth wire protocol.
    #[test]
    fn test_network_encode_decode_roundtrip() {
        let original = create_test_receipt();

        // Network encode (uses Encodable trait)
        let mut encoded = Vec::new();
        alloy_rlp::Encodable::encode(&original, &mut encoded);

        // Network decode (uses Decodable trait)
        let decoded: MorphReceipt = alloy_rlp::Decodable::decode(&mut encoded.as_slice()).unwrap();

        // Verify
        assert_eq!(decoded.tx_type(), original.tx_type());
        assert_eq!(decoded.status(), original.status());
        assert_eq!(
            decoded.cumulative_gas_used(),
            original.cumulative_gas_used()
        );
    }

    /// Tests that network encoding length calculation is correct.
    #[test]
    fn test_network_encode_length() {
        let receipt = create_test_receipt();

        // Calculate expected length
        let expected_len = alloy_rlp::Encodable::length(&receipt);

        // Actually encode
        let mut encoded = Vec::new();
        alloy_rlp::Encodable::encode(&receipt, &mut encoded);

        assert_eq!(encoded.len(), expected_len);
    }

    /// Tests RLP encoding with bloom (used for P2P and Merkle trie).
    #[test]
    fn test_rlp_encode_with_bloom_roundtrip() {
        let original = create_test_receipt();
        let bloom = original.bloom();

        // Encode with bloom
        let mut encoded = Vec::new();
        RlpEncodableReceipt::rlp_encode_with_bloom(&original, &bloom, &mut encoded);

        // Decode with bloom
        let ReceiptWithBloom {
            receipt: decoded,
            logs_bloom: decoded_bloom,
        }: ReceiptWithBloom<MorphReceipt> =
            RlpDecodableReceipt::rlp_decode_with_bloom(&mut encoded.as_slice()).unwrap();

        // Verify
        assert_eq!(decoded.tx_type(), original.tx_type());
        assert_eq!(decoded.status(), original.status());
        assert_eq!(
            decoded.cumulative_gas_used(),
            original.cumulative_gas_used()
        );
        assert_eq!(decoded_bloom, bloom);
    }

    /// Tests that without-bloom encoding is smaller than with-bloom encoding.
    ///
    /// This verifies the DA compression benefit.
    #[test]
    fn test_without_bloom_is_smaller() {
        let receipt = create_test_receipt();
        let bloom = receipt.bloom();

        // EIP-2718 encoding (without bloom)
        let len_without_bloom = receipt.encode_2718_len();

        // EIP-2718 encoding with bloom
        let len_with_bloom = receipt.eip2718_encoded_length_with_bloom(&bloom);

        // Without bloom should be smaller (bloom is 256 bytes)
        assert!(
            len_without_bloom < len_with_bloom,
            "Without bloom ({len_without_bloom}) should be smaller than with bloom ({len_with_bloom})"
        );

        // The difference should be approximately 256 bytes (bloom size) + some RLP overhead
        let difference = len_with_bloom - len_without_bloom;
        assert!(
            difference >= 256,
            "Difference ({difference}) should be at least 256 bytes (bloom size)"
        );
    }

    /// Tests all transaction types for encode/decode roundtrip.
    #[test]
    fn test_all_tx_types_encode_decode() {
        let receipts = vec![
            (MorphTxType::Legacy, create_legacy_receipt()),
            (MorphTxType::Eip1559, create_test_receipt()),
            (MorphTxType::L1Msg, create_l1_msg_receipt()),
            (MorphTxType::Morph, create_morph_receipt()),
        ];

        for (expected_type, original) in receipts {
            // EIP-2718 roundtrip
            let mut encoded = Vec::new();
            original.encode_2718(&mut encoded);

            let decoded = MorphReceipt::decode_2718(&mut encoded.as_slice())
                .unwrap_or_else(|e| panic!("Failed to decode {expected_type:?}: {e:?}"));

            assert_eq!(
                decoded.tx_type(),
                expected_type,
                "Transaction type mismatch for {expected_type:?}"
            );
            assert_eq!(
                decoded.status(),
                original.status(),
                "Status mismatch for {expected_type:?}"
            );
            assert_eq!(
                decoded.cumulative_gas_used(),
                original.cumulative_gas_used(),
                "Gas mismatch for {expected_type:?}"
            );
        }
    }

    /// Tests that decoding invalid data returns an error.
    #[test]
    fn test_decode_invalid_data() {
        // Empty buffer
        let result = MorphReceipt::decode_2718(&mut [].as_slice());
        assert!(result.is_err());

        // Invalid type byte
        let result = MorphReceipt::decode_2718(&mut [0xff].as_slice());
        assert!(result.is_err());

        // Truncated data
        let mut encoded = Vec::new();
        create_test_receipt().encode_2718(&mut encoded);
        let truncated = &encoded[..encoded.len() / 2];
        let result = MorphReceipt::decode_2718(&mut truncated.to_vec().as_slice());
        assert!(result.is_err());
    }
}
