//! L1 Message Transaction type for Morph L2.
//!
//! This module defines the TxL1Msg type which represents L1 message
//! transactions that are processed on Morph L2.
//!
//! Reference: <https://github.com/morph-l2/morph/blob/main/prover/crates/primitives/src/types/tx.rs>

use alloy_consensus::{
    SignableTransaction, Transaction,
    transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx},
};
use alloy_eips::{Typed2718, eip2718::Encodable2718};
use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256, keccak256};
use alloy_rlp::{BufMut, Decodable, Encodable, Header};
use core::mem;

/// L1 Message Transaction type ID (0x7E).
pub const L1_TX_TYPE_ID: u8 = 0x7E;

/// L1 Message Transaction for Morph L2.
///
/// This transaction type represents L1 message transactions that are processed on L2,
/// typically including deposit transactions or L1-originated messages.
///
/// Reference: <https://github.com/morph-l2/morph/blob/main/prover/crates/primitives/src/types/tx.rs#L32-L59>
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[cfg_attr(feature = "reth-codec", derive(reth_codecs::Compact))]
pub struct TxL1Msg {
    /// The queue index of the message in the L1 contract queue.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub queue_index: u64,

    /// The 160-bit address of the message call's sender.
    pub from: Address,

    /// A scalar value equal to the number of transactions sent by the sender.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub nonce: u64,

    /// A scalar value equal to the maximum amount of gas that should be used
    /// in executing this transaction. This is paid up-front, before any
    /// computation is done and may not be increased later.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub gas_limit: u128,

    /// The 160-bit address of the message call's recipient or, for a contract
    /// creation transaction, empty.
    pub to: TxKind,

    /// A scalar value equal to the number of Wei to be transferred to the
    /// message call's recipient or, in the case of contract creation, as an
    /// endowment to the newly created account.
    pub value: U256,

    /// Input has two uses depending if transaction is Create or Call (if `to`
    /// field is None or Some).
    /// - init: An unlimited size byte array specifying the EVM-code for the
    ///   account initialisation procedure CREATE.
    /// - data: An unlimited size byte array specifying the input data of the
    ///   message call.
    #[cfg_attr(feature = "serde", serde(default, alias = "data"))]
    pub input: Bytes,
}

impl TxL1Msg {
    /// Get the transaction type
    #[doc(alias = "transaction_type")]
    pub const fn tx_type() -> u8 {
        L1_TX_TYPE_ID
    }

    /// Returns the sender address.
    pub const fn sender(&self) -> Address {
        self.from
    }

    /// Validates the transaction according to the spec rules.
    ///
    /// L1 message transactions have minimal validation requirements.
    pub fn validate(&self) -> Result<(), &'static str> {
        // L1 messages are validated by the L1 contract, minimal validation here
        Ok(())
    }

    /// Calculate the in-memory size of this transaction.
    ///
    /// This accounts for all fields in the struct.
    pub fn size(&self) -> usize {
        mem::size_of::<u64>() + // queue_index
        mem::size_of::<Address>() + // from
        mem::size_of::<u64>() + // nonce
        mem::size_of::<u128>() + // gas_limit
        mem::size_of::<TxKind>() + // to
        mem::size_of::<U256>() + // value
        self.input.len() // input (dynamic size)
    }

    /// Outputs the length of the transaction's fields.
    #[doc(hidden)]
    pub fn fields_len(&self) -> usize {
        let mut len = 0;
        len += self.queue_index.length();
        len += self.nonce.length();
        len += self.gas_limit.length();
        len += self.to.length();
        len += self.value.length();
        len += self.input.0.length();
        len += self.from.length();
        len
    }

    /// Encode the transaction fields (without the RLP header).
    pub fn encode_fields(&self, out: &mut dyn BufMut) {
        self.queue_index.encode(out);
        self.nonce.encode(out);
        self.gas_limit.encode(out);
        self.to.encode(out);
        self.value.encode(out);
        self.input.0.encode(out);
        self.from.encode(out);
    }

    /// Decode the transaction fields (without the RLP header).
    pub fn decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self {
            queue_index: Decodable::decode(buf)?,
            nonce: Decodable::decode(buf)?,
            gas_limit: Decodable::decode(buf)?,
            to: Decodable::decode(buf)?,
            value: Decodable::decode(buf)?,
            input: Decodable::decode(buf)?,
            from: Decodable::decode(buf)?,
        })
    }

    /// Computes the hash used for the transaction.
    ///
    /// For L1 messages, this computes the keccak256 hash of the RLP encoding.
    pub fn signature_hash(&self) -> B256 {
        let mut buf = Vec::with_capacity(self.encode_2718_len());
        self.encode_2718(&mut buf);
        keccak256(&buf)
    }
}

impl Typed2718 for TxL1Msg {
    fn ty(&self) -> u8 {
        L1_TX_TYPE_ID
    }
}

impl Transaction for TxL1Msg {
    fn chain_id(&self) -> Option<ChainId> {
        None
    }

    fn nonce(&self) -> u64 {
        0
    }

    fn gas_limit(&self) -> u64 {
        self.gas_limit as u64
    }

    fn gas_price(&self) -> Option<u128> {
        Some(0)
    }

    fn max_fee_per_gas(&self) -> u128 {
        0
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        None
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        None
    }

    fn priority_fee_or_price(&self) -> u128 {
        0
    }

    fn effective_gas_price(&self, _base_fee: Option<u64>) -> u128 {
        0
    }

    fn is_dynamic_fee(&self) -> bool {
        false
    }

    fn kind(&self) -> TxKind {
        self.to
    }

    fn is_create(&self) -> bool {
        self.to.is_create()
    }

    fn value(&self) -> U256 {
        self.value
    }

    fn input(&self) -> &Bytes {
        &self.input
    }

    fn access_list(&self) -> Option<&alloy_eips::eip2930::AccessList> {
        None
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        None
    }

    fn authorization_list(&self) -> Option<&[alloy_eips::eip7702::SignedAuthorization]> {
        None
    }
}

impl RlpEcdsaEncodableTx for TxL1Msg {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.fields_len()
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.encode_fields(out);
    }
}

impl RlpEcdsaDecodableTx for TxL1Msg {
    const DEFAULT_TX_TYPE: u8 = { Self::tx_type() };

    /// Decodes the inner [TxEip1559] fields from RLP bytes.
    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::decode_fields(buf)
    }
}

impl SignableTransaction<Signature> for TxL1Msg {
    fn set_chain_id(&mut self, _chain_id: ChainId) {}

    fn encode_for_signing(&self, out: &mut dyn alloy_rlp::BufMut) {
        out.put_u8(Self::tx_type());
        self.encode(out)
    }

    fn payload_len_for_signature(&self) -> usize {
        self.length() + 1
    }
}

impl Encodable for TxL1Msg {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_encode(out);
    }

    fn length(&self) -> usize {
        self.rlp_encoded_length()
    }
}

impl Decodable for TxL1Msg {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        let remaining = buf.len();
        if header.payload_length > remaining {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        let queue_index = Decodable::decode(buf)?;
        let nonce = Decodable::decode(buf)?;
        let gas_limit = Decodable::decode(buf)?;
        let to = Decodable::decode(buf)?;
        let value = Decodable::decode(buf)?;
        let input = Decodable::decode(buf)?;
        let from = Decodable::decode(buf)?;

        if buf.len() + header.payload_length != remaining {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        Ok(Self {
            queue_index,
            from,
            nonce,
            gas_limit,
            to,
            value,
            input,
        })
    }
}

impl Encodable2718 for TxL1Msg {
    fn type_flag(&self) -> Option<u8> {
        Some(L1_TX_TYPE_ID)
    }

    fn encode_2718_len(&self) -> usize {
        let payload_length = self.fields_len();
        1 + Header {
            list: true,
            payload_length,
        }
        .length()
            + payload_length
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        L1_TX_TYPE_ID.encode(out);
        let header = Header {
            list: true,
            payload_length: self.fields_len(),
        };
        header.encode(out);
        self.encode_fields(out);
    }
}

impl reth_primitives_traits::InMemorySize for TxL1Msg {
    fn size(&self) -> usize {
        Self::size(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_l1_transaction_default() {
        let tx = TxL1Msg::default();
        assert_eq!(tx.nonce, 0);
        assert_eq!(tx.gas_limit, 0);
        assert_eq!(tx.value, U256::ZERO);
        assert_eq!(tx.from, Address::ZERO);
    }

    #[test]
    fn test_l1_transaction_tx_type() {
        assert_eq!(TxL1Msg::tx_type(), L1_TX_TYPE_ID);
        assert_eq!(TxL1Msg::tx_type(), 0x7E);
    }

    #[test]
    fn test_l1_transaction_validate() {
        let tx = TxL1Msg::default();
        assert!(tx.validate().is_ok());
    }

    #[test]
    fn test_l1_transaction_trait_methods() {
        let tx = TxL1Msg {
            queue_index: 0,
            from: address!("0000000000000000000000000000000000000001"),
            nonce: 0,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            input: Bytes::from(vec![1, 2, 3, 4]),
        };

        // Test Transaction trait methods
        assert_eq!(tx.chain_id(), None);
        assert_eq!(Transaction::nonce(&tx), 0); // nonce is set to 0 in this test case
        assert_eq!(Transaction::gas_limit(&tx), 21_000);
        assert_eq!(tx.gas_price(), Some(0));
        assert_eq!(tx.max_fee_per_gas(), 0);
        assert_eq!(tx.max_priority_fee_per_gas(), None);
        assert_eq!(tx.max_fee_per_blob_gas(), None);
        assert_eq!(tx.priority_fee_or_price(), 0);
        assert_eq!(tx.effective_gas_price(Some(100)), 0);
        assert!(!tx.is_dynamic_fee());
        assert!(!tx.is_create());
        assert_eq!(
            tx.kind(),
            TxKind::Call(address!("0000000000000000000000000000000000000002"))
        );
        assert_eq!(Transaction::value(&tx), U256::from(100u64));
        assert_eq!(Transaction::input(&tx), &Bytes::from(vec![1, 2, 3, 4]));
        assert_eq!(Typed2718::ty(&tx), L1_TX_TYPE_ID);
        assert!(tx.access_list().is_none());
        assert!(tx.blob_versioned_hashes().is_none());
        assert!(tx.authorization_list().is_none());
    }

    #[test]
    fn test_l1_transaction_is_create() {
        let create_tx = TxL1Msg {
            to: TxKind::Create,
            ..Default::default()
        };
        assert!(create_tx.is_create());

        let call_tx = TxL1Msg {
            to: TxKind::Call(address!("0000000000000000000000000000000000000001")),
            ..Default::default()
        };
        assert!(!call_tx.is_create());
    }

    #[test]
    fn test_l1_transaction_sender() {
        let tx = TxL1Msg {
            from: address!("0000000000000000000000000000000000000001"),
            ..Default::default()
        };
        assert_eq!(
            tx.sender(),
            address!("0000000000000000000000000000000000000001")
        );
    }

    #[test]
    fn test_l1_transaction_signature_hash() {
        let tx = TxL1Msg {
            queue_index: 0,
            from: address!("0000000000000000000000000000000000000001"),
            nonce: 1,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            input: Bytes::new(),
        };

        let hash = tx.signature_hash();
        assert_ne!(hash, B256::ZERO);
    }

    #[test]
    fn test_l1_transaction_rlp_roundtrip() {
        let tx = TxL1Msg {
            queue_index: 5,
            from: address!("0000000000000000000000000000000000000001"),
            nonce: 42,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(1_000_000_000_000_000_000u128),
            input: Bytes::from(vec![0x12, 0x34]),
        };

        // Encode
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // Decode
        let decoded = TxL1Msg::decode(&mut buf.as_slice()).expect("Should decode");

        assert_eq!(tx.queue_index, decoded.queue_index);
        assert_eq!(tx.from, decoded.from);
        assert_eq!(tx.nonce, decoded.nonce);
        assert_eq!(tx.gas_limit, decoded.gas_limit);
        assert_eq!(tx.to, decoded.to);
        assert_eq!(tx.value, decoded.value);
        assert_eq!(tx.input, decoded.input);
    }

    #[test]
    fn test_l1_transaction_create() {
        let tx = TxL1Msg {
            queue_index: 0,
            from: address!("0000000000000000000000000000000000000001"),
            nonce: 0,
            gas_limit: 100_000,
            to: TxKind::Create,
            value: U256::ZERO,
            input: Bytes::from(vec![0x60, 0x80, 0x60, 0x40]),
        };

        // Encode
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // Decode
        let decoded = TxL1Msg::decode(&mut buf.as_slice()).expect("Should decode");

        assert_eq!(decoded.to, TxKind::Create);
    }

    #[test]
    fn test_l1_transaction_encode_2718() {
        let tx = TxL1Msg {
            queue_index: 0,
            from: address!("0000000000000000000000000000000000000001"),
            nonce: 1,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            input: Bytes::new(),
        };

        let mut buf = Vec::new();
        tx.encode_2718(&mut buf);

        // First byte should be the type ID
        assert_eq!(buf[0], L1_TX_TYPE_ID);

        // Verify type_flag
        assert_eq!(tx.type_flag(), Some(L1_TX_TYPE_ID));

        // Verify length consistency
        assert_eq!(buf.len(), tx.encode_2718_len());
    }

    #[test]
    fn test_l1_transaction_decode_rejects_malformed_rlp() {
        let tx = TxL1Msg {
            queue_index: 0,
            from: address!("0000000000000000000000000000000000000001"),
            nonce: 42,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(1_000_000_000_000_000_000u128),
            input: Bytes::from(vec![0x12, 0x34]),
        };

        // Encode the transaction
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // Corrupt by truncating
        let original_len = buf.len();
        buf.truncate(original_len - 5);

        let result = TxL1Msg::decode(&mut buf.as_slice());
        assert!(
            result.is_err(),
            "Decoding should fail when data is truncated"
        );
        assert!(matches!(
            result.unwrap_err(),
            alloy_rlp::Error::InputTooShort | alloy_rlp::Error::UnexpectedLength
        ));
    }

    #[test]
    fn test_l1_transaction_size() {
        let tx = TxL1Msg {
            queue_index: 0,
            from: Address::ZERO,
            nonce: 0,
            gas_limit: 0,
            to: TxKind::Create,
            value: U256::ZERO,
            input: Bytes::new(),
        };

        // Calculate expected size manually
        let expected_size = mem::size_of::<u64>() + // queue_index
            mem::size_of::<Address>() + // from
            mem::size_of::<u64>() + // nonce
            mem::size_of::<u128>() + // gas_limit
            mem::size_of::<TxKind>() + // to
            mem::size_of::<U256>(); // value (empty input)

        assert_eq!(tx.size(), expected_size);
    }

    #[test]
    fn test_l1_transaction_fields_len() {
        let tx = TxL1Msg {
            queue_index: 0,
            from: address!("0000000000000000000000000000000000000001"),
            nonce: 1,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            input: Bytes::from(vec![1, 2, 3, 4]),
        };

        let fields_len = tx.fields_len();
        assert!(fields_len > 0);

        // Verify encode_2718_len is consistent
        let encode_2718_len = tx.encode_2718_len();
        assert!(encode_2718_len > fields_len);
    }

    #[test]
    fn test_l1_transaction_encode_fields() {
        let tx = TxL1Msg {
            queue_index: 0,
            from: address!("0000000000000000000000000000000000000001"),
            nonce: 1,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            input: Bytes::new(),
        };

        let mut buf = Vec::new();
        tx.encode_fields(&mut buf);

        // Should have encoded fields
        assert!(!buf.is_empty());
        assert_eq!(buf.len(), tx.fields_len());
    }
}
