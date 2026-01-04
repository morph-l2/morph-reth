//! Altfee Transaction type for Morph L2.
//!
//! This module defines the TxAltFee type which represents transactions that
//! use ERC20 tokens for gas payment instead of native ETH.
//!
//! Reference: <https://github.com/morph-l2/morph/blob/main/prover/crates/primitives/src/types/tx_alt_fee.rs>

use alloy_consensus::{
    SignableTransaction, Transaction,
    transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx},
};
use alloy_eips::{
    Typed2718, eip2718::Encodable2718, eip2930::AccessList, eip7702::SignedAuthorization,
};
use alloy_primitives::{B256, Bytes, ChainId, Signature, TxKind, U256, keccak256};
use alloy_rlp::{BufMut, Decodable, Encodable, Header};
use core::mem;

/// Altfee Transaction type ID (0x7F).
pub const ALT_FEE_TX_TYPE_ID: u8 = 0x7F;

/// Altfee Transaction for Morph L2.
///
/// This transaction type allows users to pay gas fees using ERC20 tokens
/// instead of native ETH. It extends EIP-1559 style transactions with
/// additional fields for token-based fee payment.
///
/// Reference: <https://github.com/morph-l2/morph/blob/main/prover/crates/primitives/src/types/tx_alt_fee.rs>
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TxAltFee {
    /// EIP-155: Simple replay attack protection.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub chain_id: ChainId,

    /// A scalar value equal to the number of transactions sent by the sender.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub nonce: u64,

    /// A scalar value equal to the maximum amount of gas that should be used
    /// in executing this transaction. This is paid up-front, before any
    /// computation is done and may not be increased later.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub gas_limit: u128,

    /// A scalar value equal to the maximum amount of gas that should be used
    /// in executing this transaction. This is paid up-front, before any
    /// computation is done and may not be increased later.
    ///
    /// This is also known as `GasFeeCap`.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_fee_per_gas: u128,

    /// Max Priority fee that transaction is paying.
    ///
    /// This is also known as `GasTipCap`.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_priority_fee_per_gas: u128,

    /// The 160-bit address of the message call's recipient or, for a contract
    /// creation transaction, empty.
    pub to: TxKind,

    /// A scalar value equal to the number of Wei to be transferred to the
    /// message call's recipient or, in the case of contract creation, as an
    /// endowment to the newly created account.
    pub value: U256,

    /// The accessList specifies a list of addresses and storage keys;
    /// these addresses and storage keys are added into the `accessed_addresses`
    /// and `accessed_storage_keys` global sets (introduced in EIP-2929).
    /// A gas cost is charged, though at a discount relative to the cost of
    /// accessing outside the list.
    pub access_list: AccessList,

    /// Maximum amount of tokens the sender is willing to pay as fee.
    pub fee_limit: U256,

    /// Input has two uses depending if transaction is Create or Call (if `to`
    /// field is None or Some).
    /// - init: An unlimited size byte array specifying the EVM-code for the
    ///   account initialisation procedure CREATE.
    /// - data: An unlimited size byte array specifying the input data of the
    ///   message call.
    #[cfg_attr(feature = "serde", serde(default, alias = "data"))]
    pub input: Bytes,

    /// Token ID for alternative fee payment.
    /// This corresponds to the token registered in the L2 Token Registry.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub fee_token_id: u16,
}

impl TxAltFee {
    /// Get the transaction type.
    #[doc(alias = "transaction_type")]
    pub const fn tx_type() -> u8 {
        ALT_FEE_TX_TYPE_ID
    }

    /// Returns the effective gas price for the given `base_fee`.
    pub const fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        match base_fee {
            None => self.max_fee_per_gas,
            Some(base_fee) => {
                // If the tip is greater than the max priority fee per gas, set it to the max
                // priority fee per gas + base fee
                let tip = self.max_fee_per_gas.saturating_sub(base_fee as u128);
                if tip > self.max_priority_fee_per_gas {
                    self.max_priority_fee_per_gas + base_fee as u128
                } else {
                    // Otherwise return the max fee per gas
                    self.max_fee_per_gas
                }
            }
        }
    }

    /// Validates the transaction according to the spec rules.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_priority_fee_per_gas > self.max_fee_per_gas {
            return Err("max priority fee per gas exceeds max fee per gas");
        }
        Ok(())
    }

    /// Calculate the in-memory size of this transaction.
    pub fn size(&self) -> usize {
        mem::size_of::<ChainId>() + // chain_id
        mem::size_of::<u64>() + // nonce
        mem::size_of::<u128>() + // gas_limit
        mem::size_of::<u128>() + // max_fee_per_gas
        mem::size_of::<u128>() + // max_priority_fee_per_gas
        self.to.size() + // to
        mem::size_of::<U256>() + // value
        self.access_list.size() + // access_list
        self.input.len() + // input
        mem::size_of::<u16>() + // fee_token_id
        mem::size_of::<U256>() // fee_limit
    }

    /// Outputs the length of the transaction's fields, without a RLP header.
    #[doc(hidden)]
    pub fn fields_len(&self) -> usize {
        let mut len = 0;
        len += self.chain_id.length();
        len += self.nonce.length();
        len += self.max_priority_fee_per_gas.length();
        len += self.max_fee_per_gas.length();
        len += self.gas_limit.length();
        len += self.to.length();
        len += self.value.length();
        len += self.input.0.length();
        len += self.access_list.length();
        len += self.fee_token_id.length();
        len += self.fee_limit.length();
        len
    }

    /// Encodes only the transaction's fields into the desired buffer, without a RLP header.
    pub fn encode_fields(&self, out: &mut dyn BufMut) {
        self.chain_id.encode(out);
        self.nonce.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
        self.gas_limit.encode(out);
        self.to.encode(out);
        self.value.encode(out);
        self.input.0.encode(out);
        self.access_list.encode(out);
        self.fee_token_id.encode(out);
        self.fee_limit.encode(out);
    }

    /// Decodes the inner fields from RLP bytes.
    ///
    /// NOTE: This assumes a RLP header has already been decoded, and _just_ decodes the following
    /// RLP fields in the following order:
    ///
    /// - `chain_id`
    /// - `nonce`
    /// - `max_priority_fee_per_gas`
    /// - `max_fee_per_gas`
    /// - `gas_limit`
    /// - `to`
    /// - `value`
    /// - `data` (`input`)
    /// - `access_list`
    /// - `fee_token_id`
    /// - `fee_limit`
    pub fn decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self {
            chain_id: Decodable::decode(buf)?,
            nonce: Decodable::decode(buf)?,
            max_priority_fee_per_gas: Decodable::decode(buf)?,
            max_fee_per_gas: Decodable::decode(buf)?,
            gas_limit: Decodable::decode(buf)?,
            to: Decodable::decode(buf)?,
            value: Decodable::decode(buf)?,
            input: Decodable::decode(buf)?,
            access_list: Decodable::decode(buf)?,
            fee_token_id: Decodable::decode(buf)?,
            fee_limit: Decodable::decode(buf)?,
        })
    }

    /// Computes the hash used for signing the transaction.
    pub fn signature_hash(&self) -> B256 {
        let mut buf = Vec::with_capacity(self.encode_2718_len());
        self.encode_2718(&mut buf);
        keccak256(&buf)
    }

    /// Returns the RLP header for this transaction.
    fn rlp_header(&self) -> Header {
        Header {
            list: true,
            payload_length: self.fields_len(),
        }
    }
}

impl Typed2718 for TxAltFee {
    fn ty(&self) -> u8 {
        ALT_FEE_TX_TYPE_ID
    }
}

impl Transaction for TxAltFee {
    fn chain_id(&self) -> Option<ChainId> {
        Some(self.chain_id)
    }

    fn nonce(&self) -> u64 {
        self.nonce
    }

    fn gas_limit(&self) -> u64 {
        self.gas_limit as u64
    }

    fn gas_price(&self) -> Option<u128> {
        None
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        Some(self.max_priority_fee_per_gas)
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        None
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.max_priority_fee_per_gas
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        true
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

    fn access_list(&self) -> Option<&AccessList> {
        Some(&self.access_list)
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        None
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        None
    }
}

impl RlpEcdsaEncodableTx for TxAltFee {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.fields_len()
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.encode_fields(out);
    }
}

impl RlpEcdsaDecodableTx for TxAltFee {
    const DEFAULT_TX_TYPE: u8 = { Self::tx_type() as u8 };

    /// Decodes the inner [TxEip1559] fields from RLP bytes.
    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::decode_fields(buf)
    }
}

impl SignableTransaction<Signature> for TxAltFee {
    fn set_chain_id(&mut self, chain_id: ChainId) {
        self.chain_id = chain_id;
    }

    fn encode_for_signing(&self, out: &mut dyn alloy_rlp::BufMut) {
        out.put_u8(Self::tx_type() as u8);
        self.encode(out)
    }

    fn payload_len_for_signature(&self) -> usize {
        self.length() + 1
    }
}

impl Encodable for TxAltFee {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_header().encode(out);
        self.encode_fields(out);
    }

    fn length(&self) -> usize {
        self.rlp_header().length_with_payload()
    }
}

impl Decodable for TxAltFee {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        let remaining = buf.len();
        if header.payload_length > remaining {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        Self::decode_fields(buf)
    }
}

impl Encodable2718 for TxAltFee {
    fn type_flag(&self) -> Option<u8> {
        Some(ALT_FEE_TX_TYPE_ID)
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
        ALT_FEE_TX_TYPE_ID.encode(out);
        let header = Header {
            list: true,
            payload_length: self.fields_len(),
        };
        header.encode(out);
        self.encode_fields(out);
    }
}

impl reth_primitives_traits::InMemorySize for TxAltFee {
    fn size(&self) -> usize {
        Self::size(self)
    }
}

#[cfg(feature = "reth-codec")]
impl reth_codecs::Compact for TxAltFee {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let mut len = 0;
        len += self.chain_id.to_compact(buf);
        len += self.nonce.to_compact(buf);
        len += self.gas_limit.to_compact(buf);
        len += self.max_fee_per_gas.to_compact(buf);
        len += self.max_priority_fee_per_gas.to_compact(buf);
        len += self.to.to_compact(buf);
        len += self.value.to_compact(buf);
        len += self.access_list.to_compact(buf);
        len += self.fee_limit.to_compact(buf);
        len += self.input.to_compact(buf);
        len += (self.fee_token_id as u64).to_compact(buf);
        len
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (chain_id, buf) = ChainId::from_compact(buf, len);
        let (nonce, buf) = u64::from_compact(buf, len);
        let (gas_limit, buf) = u128::from_compact(buf, len);
        let (max_fee_per_gas, buf) = u128::from_compact(buf, len);
        let (max_priority_fee_per_gas, buf) = u128::from_compact(buf, len);
        let (to, buf) = TxKind::from_compact(buf, len);
        let (value, buf) = U256::from_compact(buf, len);
        let (access_list, buf) = AccessList::from_compact(buf, len);
        let (fee_limit, buf) = U256::from_compact(buf, len);
        let (input, buf) = Bytes::from_compact(buf, len);
        let (fee_token_id, buf) = u64::from_compact(buf, len);

        (
            Self {
                chain_id,
                nonce,
                gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                to,
                value,
                access_list,
                fee_limit,
                input,
                fee_token_id: fee_token_id as u16,
            },
            buf,
        )
    }
}

/// Extension trait for [`TxAltFee`] to access token fee fields.
pub trait TxAltFeeExt {
    /// Returns the token ID used for fee payment.
    fn fee_token_id(&self) -> u16;

    /// Returns the maximum token amount for fee payment.
    fn fee_limit(&self) -> U256;

    /// Returns true if this transaction uses token-based fee payment.
    fn uses_token_fee(&self) -> bool {
        self.fee_token_id() > 0
    }
}

impl TxAltFeeExt for TxAltFee {
    fn fee_token_id(&self) -> u16 {
        self.fee_token_id
    }

    fn fee_limit(&self) -> U256 {
        self.fee_limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_tx_alt_fee_default() {
        let tx = TxAltFee::default();
        assert_eq!(tx.chain_id, 0);
        assert_eq!(tx.nonce, 0);
        assert_eq!(tx.gas_limit, 0);
        assert_eq!(tx.max_fee_per_gas, 0);
        assert_eq!(tx.max_priority_fee_per_gas, 0);
        assert_eq!(tx.value, U256::ZERO);
        assert_eq!(tx.fee_token_id, 0);
        assert_eq!(tx.fee_limit, U256::ZERO);
    }

    #[test]
    fn test_tx_alt_fee_tx_type() {
        assert_eq!(TxAltFee::tx_type(), ALT_FEE_TX_TYPE_ID);
        assert_eq!(TxAltFee::tx_type(), 0x7F);
    }

    #[test]
    fn test_tx_alt_fee_validate() {
        let valid_tx = TxAltFee {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 50,
            ..Default::default()
        };
        assert!(valid_tx.validate().is_ok());

        let invalid_tx = TxAltFee {
            max_fee_per_gas: 50,
            max_priority_fee_per_gas: 100,
            ..Default::default()
        };
        assert!(invalid_tx.validate().is_err());
    }

    #[test]
    fn test_tx_alt_fee_effective_gas_price() {
        let tx = TxAltFee {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 20,
            ..Default::default()
        };

        // Without base fee
        assert_eq!(tx.effective_gas_price(None), 100);

        // With base fee (tip > max_priority_fee_per_gas)
        assert_eq!(tx.effective_gas_price(Some(50)), 70); // 20 + 50

        // With base fee (tip <= max_priority_fee_per_gas)
        assert_eq!(tx.effective_gas_price(Some(90)), 100); // max_fee_per_gas
    }

    #[test]
    fn test_tx_alt_fee_trait_methods() {
        let tx = TxAltFee {
            chain_id: 1,
            nonce: 42,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 20,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::from(vec![1, 2, 3, 4]),
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
        };

        // Test Transaction trait methods
        assert_eq!(tx.chain_id(), Some(1));
        assert_eq!(Transaction::nonce(&tx), 42);
        assert_eq!(Transaction::gas_limit(&tx), 21_000);
        assert_eq!(tx.gas_price(), None);
        assert_eq!(tx.max_fee_per_gas(), 100);
        assert_eq!(tx.max_priority_fee_per_gas(), Some(20));
        assert_eq!(tx.max_fee_per_blob_gas(), None);
        assert_eq!(tx.priority_fee_or_price(), 20);
        assert!(tx.is_dynamic_fee());
        assert!(!tx.is_create());
        assert_eq!(
            tx.kind(),
            TxKind::Call(address!("0000000000000000000000000000000000000002"))
        );
        assert_eq!(Transaction::value(&tx), U256::from(100u64));
        assert_eq!(Transaction::input(&tx), &Bytes::from(vec![1, 2, 3, 4]));
        assert_eq!(Typed2718::ty(&tx), ALT_FEE_TX_TYPE_ID);
        assert!(tx.access_list().is_some());
        assert!(tx.blob_versioned_hashes().is_none());
        assert!(tx.authorization_list().is_none());

        // Test TxAltFeeExt trait methods
        assert_eq!(tx.fee_token_id(), 1);
        assert_eq!(tx.fee_limit(), U256::from(1000u64));
        assert!(tx.uses_token_fee());
    }

    #[test]
    fn test_tx_alt_fee_is_create() {
        let create_tx = TxAltFee {
            to: TxKind::Create,
            ..Default::default()
        };
        assert!(create_tx.is_create());

        let call_tx = TxAltFee {
            to: TxKind::Call(address!("0000000000000000000000000000000000000001")),
            ..Default::default()
        };
        assert!(!call_tx.is_create());
    }

    #[test]
    fn test_tx_alt_fee_rlp_roundtrip() {
        let tx = TxAltFee {
            chain_id: 1,
            nonce: 42,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(1_000_000_000_000_000_000u128),
            access_list: AccessList::default(),
            input: Bytes::from(vec![0x12, 0x34]),
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
        };

        // Encode
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // Decode
        let decoded = TxAltFee::decode(&mut buf.as_slice()).expect("Should decode");

        assert_eq!(tx.chain_id, decoded.chain_id);
        assert_eq!(tx.nonce, decoded.nonce);
        assert_eq!(tx.gas_limit, decoded.gas_limit);
        assert_eq!(tx.max_fee_per_gas, decoded.max_fee_per_gas);
        assert_eq!(
            tx.max_priority_fee_per_gas,
            decoded.max_priority_fee_per_gas
        );
        assert_eq!(tx.to, decoded.to);
        assert_eq!(tx.value, decoded.value);
        assert_eq!(tx.input, decoded.input);
        assert_eq!(tx.fee_token_id, decoded.fee_token_id);
        assert_eq!(tx.fee_limit, decoded.fee_limit);
    }

    #[test]
    fn test_tx_alt_fee_create() {
        let tx = TxAltFee {
            chain_id: 1,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Create,
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: Bytes::from(vec![0x60, 0x80, 0x60, 0x40]),
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
        };

        // Encode
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // Decode
        let decoded = TxAltFee::decode(&mut buf.as_slice()).expect("Should decode");

        assert_eq!(decoded.to, TxKind::Create);
    }

    #[test]
    fn test_tx_alt_fee_encode_2718() {
        let tx = TxAltFee {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
        };

        let mut buf = Vec::new();
        tx.encode_2718(&mut buf);

        // First byte should be the type ID
        assert_eq!(buf[0], ALT_FEE_TX_TYPE_ID);

        // Verify type_flag
        assert_eq!(tx.type_flag(), Some(ALT_FEE_TX_TYPE_ID));

        // Verify length consistency
        assert_eq!(buf.len(), tx.encode_2718_len());
    }

    #[test]
    fn test_tx_alt_fee_decode_rejects_malformed_rlp() {
        let tx = TxAltFee {
            chain_id: 1,
            nonce: 42,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(1_000_000_000_000_000_000u128),
            access_list: AccessList::default(),
            input: Bytes::from(vec![0x12, 0x34]),
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
        };

        // Encode the transaction
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // Corrupt by truncating
        let original_len = buf.len();
        buf.truncate(original_len - 5);

        let result = TxAltFee::decode(&mut buf.as_slice());
        assert!(
            result.is_err(),
            "Decoding should fail when data is truncated"
        );
    }

    #[test]
    fn test_tx_alt_fee_size() {
        let tx = TxAltFee {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 20,
            to: TxKind::Create,
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: Bytes::new(),
            fee_token_id: 0,
            fee_limit: U256::ZERO,
        };

        let size = tx.size();
        assert!(size > 0);
    }

    #[test]
    fn test_tx_alt_fee_fields_len() {
        let tx = TxAltFee {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::from(vec![1, 2, 3, 4]),
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
        };

        let fields_len = tx.fields_len();
        assert!(fields_len > 0);

        // Verify encode_2718_len is consistent
        let encode_2718_len = tx.encode_2718_len();
        assert!(encode_2718_len > fields_len);
    }

    #[test]
    fn test_tx_alt_fee_encode_fields() {
        let tx = TxAltFee {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
        };

        let mut buf = Vec::new();
        tx.encode_fields(&mut buf);

        // Should have encoded fields
        assert!(!buf.is_empty());
        assert_eq!(buf.len(), tx.fields_len());
    }

    #[test]
    fn test_tx_alt_fee_uses_token_fee() {
        let tx_with_token = TxAltFee {
            fee_token_id: 1,
            ..Default::default()
        };
        assert!(tx_with_token.uses_token_fee());

        let tx_without_token = TxAltFee {
            fee_token_id: 0,
            ..Default::default()
        };
        assert!(!tx_without_token.uses_token_fee());
    }

    #[test]
    fn test_tx_alt_fee_signature_hash() {
        let tx = TxAltFee {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
        };

        let hash = tx.signature_hash();
        assert_ne!(hash, B256::ZERO);
    }
}
