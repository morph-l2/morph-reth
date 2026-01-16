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
use alloy_eips::{Typed2718, eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718}};
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
/// The signature of the L1 message is already verified on the L1 and as such doesn't contain
/// a signature field. Gas for the transaction execution is already paid for on the L1.
///
/// Note: Contract creation is NOT allowed via L1 message transactions.
///
/// Reference: <https://github.com/morph-l2/morph/blob/main/prover/crates/primitives/src/types/tx.rs#L32-L59>
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[cfg_attr(feature = "reth-codec", derive(reth_codecs::Compact))]
pub struct TxL1Msg {
    /// The queue index of the message in the L1 contract queue.
    pub queue_index: u64,

    /// The gas limit for the transaction. Gas is paid for when message is sent from the L1.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity", rename = "gas"))]
    pub gas_limit: u64,

    /// The destination address for the transaction.
    /// Contract creation is NOT allowed via L1 message transactions.
    pub to: Address,

    /// A scalar value equal to the number of Wei to be transferred to the
    /// message call's recipient.
    pub value: U256,

    /// The L1 sender of the transaction.
    pub sender: Address,

    /// The input data of the message call.
    /// Note: This field must be last for reth-codec Compact derive.
    #[cfg_attr(feature = "serde", serde(default, alias = "data"))]
    pub input: Bytes,
}

impl TxL1Msg {
    /// Get the transaction type
    #[doc(alias = "transaction_type")]
    pub const fn tx_type() -> u8 {
        L1_TX_TYPE_ID
    }

    /// Returns an empty signature for the [`TxL1Msg`], which don't include a signature.
    pub const fn signature() -> Signature {
        Signature::new(U256::ZERO, U256::ZERO, false)
    }

    /// Validates the transaction according to the spec rules.
    ///
    /// L1 message transactions have minimal validation requirements.
    pub fn validate(&self) -> Result<(), &'static str> {
        // L1 messages are validated by the L1 contract, minimal validation here
        Ok(())
    }

    /// Calculate the in-memory size of this transaction.
    pub fn size(&self) -> usize {
        mem::size_of::<u64>() + // queue_index
        mem::size_of::<u64>() + // gas_limit
        mem::size_of::<Address>() + // to
        mem::size_of::<U256>() + // value
        self.input.len() + // input (dynamic size)
        mem::size_of::<Address>() // sender
    }

    /// Outputs the length of the transaction's fields.
    /// Field order matches go-ethereum: queue_index, gas_limit, to, value, input, sender
    #[doc(hidden)]
    pub fn fields_len(&self) -> usize {
        self.queue_index.length() +
        self.gas_limit.length() +
        self.to.length() +
        self.value.length() +
        self.input.0.length() +
        self.sender.length()
    }

    /// Encode the transaction fields (without the RLP header).
    /// Field order matches go-ethereum: queue_index, gas_limit, to, value, input, sender
    pub fn encode_fields(&self, out: &mut dyn BufMut) {
        self.queue_index.encode(out);
        self.gas_limit.encode(out);
        self.to.encode(out);
        self.value.encode(out);
        self.input.0.encode(out);
        self.sender.encode(out);
    }

    /// Decode the transaction fields from RLP bytes.
    /// Field order matches go-ethereum: queue_index, gas_limit, to, value, input, sender
    pub fn decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self {
            queue_index: Decodable::decode(buf)?,
            gas_limit: Decodable::decode(buf)?,
            to: Decodable::decode(buf)?,
            value: Decodable::decode(buf)?,
            input: Decodable::decode(buf)?,
            sender: Decodable::decode(buf)?,
        })
    }

    /// Computes the transaction hash.
    ///
    /// For L1 messages, this computes the keccak256 hash of the EIP-2718 encoding.
    pub fn tx_hash(&self) -> B256 {
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
        // L1 messages always have nonce 0
        0
    }

    fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    fn gas_price(&self) -> Option<u128> {
        // L1 messages have no gas price - gas is paid on L1
        None
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
        // L1 messages are always calls, never contract creations
        TxKind::Call(self.to)
    }

    fn is_create(&self) -> bool {
        // Contract creation is NOT allowed via L1 message transactions
        false
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

        let this = Self::decode_fields(buf)?;

        if buf.len() + header.payload_length != remaining {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        Ok(this)
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

impl Decodable2718 for TxL1Msg {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        if ty != L1_TX_TYPE_ID {
            return Err(Eip2718Error::UnexpectedType(ty));
        }
        Self::decode(buf).map_err(Into::into)
    }

    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Self::decode(buf).map_err(Into::into)
    }
}

impl reth_primitives_traits::InMemorySize for TxL1Msg {
    fn size(&self) -> usize {
        Self::size(self)
    }
}

impl alloy_consensus::Sealable for TxL1Msg {
    fn hash_slow(&self) -> B256 {
        self.tx_hash()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_l1_transaction_default() {
        let tx = TxL1Msg::default();
        assert_eq!(tx.queue_index, 0);
        assert_eq!(tx.gas_limit, 0);
        assert_eq!(tx.value, U256::ZERO);
        assert_eq!(tx.sender, Address::ZERO);
        assert_eq!(tx.to, Address::ZERO);
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
            gas_limit: 21_000,
            to: address!("0000000000000000000000000000000000000002"),
            value: U256::from(100u64),
            input: Bytes::from(vec![1, 2, 3, 4]),
            sender: address!("0000000000000000000000000000000000000001"),
        };

        // Test Transaction trait methods
        assert_eq!(tx.chain_id(), None);
        assert_eq!(Transaction::nonce(&tx), 0); // L1 messages always have nonce 0
        assert_eq!(Transaction::gas_limit(&tx), 21_000);
        assert_eq!(tx.gas_price(), None); // L1 messages have no gas price
        assert_eq!(tx.max_fee_per_gas(), 0);
        assert_eq!(tx.max_priority_fee_per_gas(), None);
        assert_eq!(tx.max_fee_per_blob_gas(), None);
        assert_eq!(tx.priority_fee_or_price(), 0);
        assert_eq!(tx.effective_gas_price(Some(100)), 0);
        assert!(!tx.is_dynamic_fee());
        assert!(!tx.is_create()); // L1 messages can never create contracts
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
    fn test_l1_transaction_is_never_create() {
        // L1 messages should never be contract creation
        let tx = TxL1Msg::default();
        assert!(!tx.is_create());

        let tx_with_address = TxL1Msg {
            to: address!("0000000000000000000000000000000000000001"),
            ..Default::default()
        };
        assert!(!tx_with_address.is_create());
    }

    #[test]
    fn test_l1_transaction_sender() {
        let tx = TxL1Msg {
            sender: address!("0000000000000000000000000000000000000001"),
            ..Default::default()
        };
        assert_eq!(
            tx.sender,
            address!("0000000000000000000000000000000000000001")
        );
    }

    #[test]
    fn test_l1_transaction_tx_hash() {
        let tx = TxL1Msg {
            queue_index: 0,
            gas_limit: 21_000,
            to: address!("0000000000000000000000000000000000000002"),
            value: U256::from(100u64),
            input: Bytes::new(),
            sender: address!("0000000000000000000000000000000000000001"),
        };

        let hash = tx.tx_hash();
        assert_ne!(hash, B256::ZERO);
    }

    #[test]
    fn test_l1_transaction_rlp_roundtrip() {
        let tx = TxL1Msg {
            queue_index: 5,
            gas_limit: 21_000,
            to: address!("0000000000000000000000000000000000000002"),
            value: U256::from(1_000_000_000_000_000_000u64),
            input: Bytes::from(vec![0x12, 0x34]),
            sender: address!("0000000000000000000000000000000000000001"),
        };

        // Encode
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // Decode
        let decoded = TxL1Msg::decode(&mut buf.as_slice()).expect("Should decode");

        assert_eq!(tx.queue_index, decoded.queue_index);
        assert_eq!(tx.gas_limit, decoded.gas_limit);
        assert_eq!(tx.to, decoded.to);
        assert_eq!(tx.value, decoded.value);
        assert_eq!(tx.input, decoded.input);
        assert_eq!(tx.sender, decoded.sender);
    }

    #[test]
    fn test_l1_transaction_encode_2718() {
        let tx = TxL1Msg {
            queue_index: 0,
            gas_limit: 21_000,
            to: address!("0000000000000000000000000000000000000002"),
            value: U256::from(100u64),
            input: Bytes::new(),
            sender: address!("0000000000000000000000000000000000000001"),
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
            gas_limit: 21_000,
            to: address!("0000000000000000000000000000000000000002"),
            value: U256::from(1_000_000_000_000_000_000u64),
            input: Bytes::from(vec![0x12, 0x34]),
            sender: address!("0000000000000000000000000000000000000001"),
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
            gas_limit: 0,
            to: Address::ZERO,
            value: U256::ZERO,
            input: Bytes::new(),
            sender: Address::ZERO,
        };

        // Calculate expected size manually
        let expected_size = mem::size_of::<u64>() + // queue_index
            mem::size_of::<u64>() + // gas_limit
            mem::size_of::<Address>() + // to
            mem::size_of::<U256>() + // value
            mem::size_of::<Address>(); // sender
            // Note: input is empty so contributes 0 bytes

        assert_eq!(tx.size(), expected_size);
    }

    #[test]
    fn test_l1_transaction_fields_len() {
        let tx = TxL1Msg {
            queue_index: 0,
            gas_limit: 21_000,
            to: address!("0000000000000000000000000000000000000002"),
            value: U256::from(100u64),
            input: Bytes::from(vec![1, 2, 3, 4]),
            sender: address!("0000000000000000000000000000000000000001"),
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
            gas_limit: 21_000,
            to: address!("0000000000000000000000000000000000000002"),
            value: U256::from(100u64),
            input: Bytes::new(),
            sender: address!("0000000000000000000000000000000000000001"),
        };

        let mut buf = Vec::new();
        tx.encode_fields(&mut buf);

        // Should have encoded fields
        assert!(!buf.is_empty());
        assert_eq!(buf.len(), tx.fields_len());
    }

    #[test]
    fn test_l1_transaction_signature() {
        // L1 messages should return an empty signature
        let sig = TxL1Msg::signature();
        assert_eq!(sig.r(), U256::ZERO);
        assert_eq!(sig.s(), U256::ZERO);
        assert!(!sig.v());
    }
}
