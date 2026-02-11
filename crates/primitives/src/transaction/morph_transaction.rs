//! Morph Transaction type for Morph L2.
//!
//! This module defines the TxMorph type which represents Morph-specific transactions
//! that support:
//! - ERC20 tokens for gas payment instead of native ETH
//! - Transaction reference for indexing/lookup
//! - Memo field for arbitrary data
//!
//! Reference: <https://github.com/morph-l2/go-ethereum/pull/282>

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

/// Morph Transaction type ID (0x7F).
pub const MORPH_TX_TYPE_ID: u8 = 0x7F;

/// MorphTx version 0: original format without Version, Reference, Memo fields.
pub const MORPH_TX_VERSION_0: u8 = 0;

/// MorphTx version 1: includes Version, Reference, Memo fields.
pub const MORPH_TX_VERSION_1: u8 = 1;

/// Maximum length of the memo field in bytes.
pub const MAX_MEMO_LENGTH: usize = 64;

/// Morph Transaction for Morph L2.
///
/// This transaction type extends EIP-1559 style transactions with Morph-specific fields:
/// - Token-based fee payment (ERC20 tokens instead of native ETH)
/// - Transaction reference for indexing/lookup by external systems
/// - Memo field for arbitrary data
///
/// Reference: <https://github.com/morph-l2/go-ethereum/pull/282>
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TxMorph {
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

    /// Version of the Morph transaction format.
    /// Used for future extensibility.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub version: u8,

    /// Token ID for alternative fee payment.
    /// This corresponds to the token registered in the L2 Token Registry.
    /// 0 means ETH payment, > 0 means ERC20 token payment.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub fee_token_id: u16,

    /// Maximum amount of tokens the sender is willing to pay as fee.
    pub fee_limit: U256,

    /// Reference key for the transaction (optional, v1 only).
    /// Used for indexing and looking up transactions by external systems.
    /// This is a 32-byte value that can be used to group related transactions.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub reference: Option<B256>,

    /// Memo field for arbitrary data (optional, v1 only).
    /// Can be used to attach additional information to the transaction.
    /// Maximum length is 64 bytes.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub memo: Option<Bytes>,

    /// Input has two uses depending if transaction is Create or Call (if `to`
    /// field is None or Some).
    /// - init: An unlimited size byte array specifying the EVM-code for the
    ///   account initialisation procedure CREATE.
    /// - data: An unlimited size byte array specifying the input data of the
    ///   message call.
    #[cfg_attr(feature = "serde", serde(default, alias = "data"))]
    pub input: Bytes,
}

impl TxMorph {
    /// Get the transaction type.
    #[doc(alias = "transaction_type")]
    pub const fn tx_type() -> u8 {
        MORPH_TX_TYPE_ID
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
        // Validate memo length
        if let Some(memo) = &self.memo
            && memo.len() > MAX_MEMO_LENGTH
        {
            return Err("memo exceeds maximum length of 64 bytes");
        }
        // Validate version-specific rules
        self.validate_version()
    }

    /// Validates the MorphTx version and its associated field requirements.
    ///
    /// Rules:
    /// - Version 0 (legacy format): FeeTokenID must be > 0, Reference and Memo must not be set
    /// - Version 1 (with Reference/Memo): FeeTokenID, Reference, Memo are all optional;
    ///   if FeeTokenID is 0, FeeLimit must not be set
    /// - Other versions: not supported
    pub fn validate_version(&self) -> Result<(), &'static str> {
        match self.version {
            MORPH_TX_VERSION_0 => {
                // Version 0 requires FeeTokenID > 0 (legacy format used for alt-fee transactions)
                if self.fee_token_id == 0 {
                    return Err("version 0 MorphTx requires FeeTokenID > 0");
                }
                // Version 0 does not support Reference field
                if self.reference.is_some() {
                    return Err("version 0 MorphTx does not support Reference field");
                }
                // Version 0 does not support Memo field
                if self.memo.as_ref().is_some_and(|m| !m.is_empty()) {
                    return Err("version 0 MorphTx does not support Memo field");
                }
            }
            MORPH_TX_VERSION_1 => {
                // Version 1: FeeTokenID, Reference, Memo are all optional
                // If FeeTokenID is 0, FeeLimit must not be set
                if self.fee_token_id == 0 && self.fee_limit > U256::ZERO {
                    return Err("version 1 MorphTx cannot have FeeLimit when FeeTokenID is 0");
                }
            }
            _ => {
                return Err("unsupported MorphTx version");
            }
        }
        Ok(())
    }

    /// Returns true if this is a version 0 (legacy) MorphTx.
    pub const fn is_v0(&self) -> bool {
        self.version == MORPH_TX_VERSION_0
    }

    /// Returns true if this is a version 1 MorphTx (with Reference/Memo).
    pub const fn is_v1(&self) -> bool {
        self.version == MORPH_TX_VERSION_1
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
        mem::size_of::<u8>() + // version
        mem::size_of::<u16>() + // fee_token_id
        mem::size_of::<U256>() + // fee_limit
        mem::size_of::<Option<B256>>() + // reference
        self.memo.as_ref().map_or(0, |m| m.len()) + // memo
        self.input.len() // input
    }

    /// Outputs the length of the transaction's RLP fields, without a RLP header.
    ///
    /// Note: For V1, the version byte is NOT included here - it's encoded as a prefix byte
    /// before the RLP data, similar to txType.
    ///
    /// V0 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit
    /// V1 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit, Reference, Memo
    #[doc(hidden)]
    pub fn fields_len(&self) -> usize {
        let mut len = 0;
        // Common fields
        len += self.chain_id.length();
        len += self.nonce.length();
        len += self.max_priority_fee_per_gas.length();
        len += self.max_fee_per_gas.length();
        len += self.gas_limit.length();
        len += self.to.length();
        len += self.value.length();
        len += self.input.0.length();
        len += self.access_list.length();

        // FeeTokenID and FeeLimit are always present
        len += self.fee_token_id.length();
        len += self.fee_limit.length();

        if !self.is_v0() {
            // V1 format: adds Reference, Memo (Version is prefix byte, not in RLP)
            // Reference is Option<B256> - encoded as 32 bytes or empty bytes
            len += self
                .reference
                .as_ref()
                .map_or(0usize.length(), |r| r.0.length());
            // Memo is Option<Bytes> - encoded as RLP bytes or empty
            len += self.memo.as_ref().map_or(0usize.length(), |m| m.0.length());
        }
        len
    }

    /// Encodes only the transaction's RLP fields into the desired buffer, without a RLP header.
    ///
    /// Note: For V1, the version byte is NOT included here - it's encoded as a prefix byte
    /// before the RLP data by the caller (encode_2718).
    ///
    /// V0 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit
    /// V1 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit, Reference, Memo
    pub fn encode_fields(&self, out: &mut dyn BufMut) {
        // Common fields
        self.chain_id.encode(out);
        self.nonce.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
        self.gas_limit.encode(out);
        self.to.encode(out);
        self.value.encode(out);
        self.input.0.encode(out);
        self.access_list.encode(out);

        // FeeTokenID and FeeLimit are always present
        self.fee_token_id.encode(out);
        self.fee_limit.encode(out);

        if !self.is_v0() {
            // V1 format: adds Reference, Memo (Version is prefix byte, not in RLP)
            // Reference is Option<B256> - encode as 32 bytes or empty bytes
            if let Some(ref r) = self.reference {
                r.0.encode(out);
            } else {
                Bytes::new().encode(out); // Encode empty bytes for None
            }
            // Memo is Option<Bytes> - encode inner bytes or empty
            if let Some(ref memo) = self.memo {
                memo.0.encode(out);
            } else {
                Bytes::new().encode(out); // Encode empty bytes for None
            }
        }
    }

    /// Decodes the inner fields from RLP bytes (after txType byte is consumed).
    ///
    /// Version detection based on first byte:
    /// - V0 format: first byte is 0 or RLP list prefix (>= 0xC0) → direct RLP decode
    /// - V1+ format: first byte is version (0x01, 0x02, ...) → skip version byte, then RLP decode
    ///
    /// V0 RLP: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit
    /// V1 RLP: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit, Reference, Memo
    pub fn decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        let first_byte = buf[0];

        // Check first byte to determine version:
        // - V0 format (legacy AltFeeTx): first byte is 0 or RLP list prefix (0xC0-0xFF)
        // - V1+ format: first byte is version (0x01, 0x02, ...) followed by RLP
        if first_byte == 0 || first_byte >= 0xC0 {
            // V0 format: direct RLP decode (legacy compatible)
            Self::decode_fields_v0(buf)
        } else if first_byte == MORPH_TX_VERSION_1 {
            // V1 format: first byte is version, rest is RLP
            // Skip the version byte
            *buf = &buf[1..];
            Self::decode_fields_v1(buf)
        } else {
            Err(alloy_rlp::Error::Custom("unsupported morph tx version"))
        }
    }

    /// Decodes V0 format fields (for decode_fields, includes RLP header handling).
    ///
    /// V0 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit
    fn decode_fields_v0(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        // Need to decode RLP header first
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        Self::decode_fields_v0_inner(buf)
    }

    /// Decodes V1 format fields (for decode_fields, includes RLP header handling).
    ///
    /// V1 format (after version byte is consumed):
    /// ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit, Reference, Memo
    ///
    /// Note: Version is NOT in the RLP - it was already consumed as a prefix byte.
    fn decode_fields_v1(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        // Need to decode RLP header first
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        Self::decode_fields_v1_inner(buf)
    }

    /// Decodes V1 format fields (inner, assumes RLP header already consumed).
    fn decode_fields_v1_inner(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let chain_id = Decodable::decode(buf)?;
        let nonce = Decodable::decode(buf)?;
        let max_priority_fee_per_gas = Decodable::decode(buf)?;
        let max_fee_per_gas = Decodable::decode(buf)?;
        let gas_limit = Decodable::decode(buf)?;
        let to = Decodable::decode(buf)?;
        let value = Decodable::decode(buf)?;
        let input = Decodable::decode(buf)?;
        let access_list = Decodable::decode(buf)?;
        let fee_token_id = Decodable::decode(buf)?;
        let fee_limit = Decodable::decode(buf)?;

        // Decode reference: empty bytes means None, 32 bytes means Some(B256)
        let reference_bytes: Bytes = Decodable::decode(buf)?;
        let reference = if reference_bytes.is_empty() {
            None
        } else if reference_bytes.len() == 32 {
            Some(B256::from_slice(&reference_bytes))
        } else {
            return Err(alloy_rlp::Error::Custom("invalid reference length"));
        };

        // Decode memo: bytes -> Option<Bytes>
        let memo_bytes: Bytes = Decodable::decode(buf)?;
        let memo = if memo_bytes.is_empty() {
            None
        } else if memo_bytes.len() > MAX_MEMO_LENGTH {
            return Err(alloy_rlp::Error::Custom("memo exceeds maximum length"));
        } else {
            Some(memo_bytes)
        };

        Ok(Self {
            chain_id,
            nonce,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            gas_limit,
            to,
            value,
            input,
            access_list,
            version: MORPH_TX_VERSION_1,
            fee_token_id,
            fee_limit,
            reference,
            memo,
        })
    }

    /// Decodes V0 format fields (inner, assumes RLP header already consumed).
    fn decode_fields_v0_inner(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let chain_id = Decodable::decode(buf)?;
        let nonce = Decodable::decode(buf)?;
        let max_priority_fee_per_gas = Decodable::decode(buf)?;
        let max_fee_per_gas = Decodable::decode(buf)?;
        let gas_limit = Decodable::decode(buf)?;
        let to = Decodable::decode(buf)?;
        let value = Decodable::decode(buf)?;
        let input = Decodable::decode(buf)?;
        let access_list = Decodable::decode(buf)?;
        let fee_token_id: u16 = Decodable::decode(buf)?;
        let fee_limit = Decodable::decode(buf)?;

        // V0 requires FeeTokenID > 0
        if fee_token_id == 0 {
            return Err(alloy_rlp::Error::Custom(
                "invalid fee token id, expected non-zero for V0",
            ));
        }

        Ok(Self {
            chain_id,
            nonce,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            gas_limit,
            to,
            value,
            input,
            access_list,
            version: MORPH_TX_VERSION_0,
            fee_token_id,
            fee_limit,
            reference: None,
            memo: None,
        })
    }

    /// Computes the hash used for signing the transaction.
    ///
    /// Note: The sigHash encoding differs from transaction encoding for V1:
    /// - Transaction encoding: `[version byte] + RLP([..., FeeTokenID, FeeLimit, Reference, Memo])`
    /// - SigHash encoding: `TxType + RLP([..., FeeTokenID, FeeLimit, Version, Reference, Memo])`
    ///
    /// For V1, Version is included IN the RLP for signing, not as a prefix.
    pub fn signature_hash(&self) -> B256 {
        let mut buf = Vec::new();
        self.encode_for_sig_hash(&mut buf);
        keccak256(&buf)
    }

    /// Encodes the transaction for signature hash calculation.
    ///
    /// V0 format: TxType + RLP([..., FeeTokenID, FeeLimit])
    /// V1 format: TxType + RLP([..., FeeTokenID, FeeLimit, Version, Reference, Memo])
    ///
    /// Note: For V1, Version is included in the RLP (after FeeLimit), not as a prefix byte.
    fn encode_for_sig_hash(&self, out: &mut dyn BufMut) {
        // Write txType
        out.put_u8(MORPH_TX_TYPE_ID);

        // Write RLP header and fields for signing
        let payload_length = self.sig_hash_fields_len();
        let header = Header {
            list: true,
            payload_length,
        };
        header.encode(out);
        self.encode_sig_hash_fields(out);
    }

    /// Outputs the length of fields for signature hash encoding.
    fn sig_hash_fields_len(&self) -> usize {
        let mut len = 0;
        // Common fields
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

        if !self.is_v0() {
            // V1 sigHash: includes Version, Reference, Memo IN the RLP
            len += self.version.length();
            len += self
                .reference
                .as_ref()
                .map_or(0usize.length(), |r| r.0.length());
            len += self.memo.as_ref().map_or(0usize.length(), |m| m.0.length());
        }
        len
    }

    /// Encodes fields for signature hash calculation.
    ///
    /// V0 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit
    /// V1 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit, Version, Reference, Memo
    fn encode_sig_hash_fields(&self, out: &mut dyn BufMut) {
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

        if !self.is_v0() {
            // V1 sigHash: includes Version, Reference, Memo IN the RLP
            self.version.encode(out);
            if let Some(ref r) = self.reference {
                r.0.encode(out);
            } else {
                Bytes::new().encode(out);
            }
            if let Some(ref memo) = self.memo {
                memo.0.encode(out);
            } else {
                Bytes::new().encode(out);
            }
        }
    }
}

impl Typed2718 for TxMorph {
    fn ty(&self) -> u8 {
        MORPH_TX_TYPE_ID
    }
}

impl Transaction for TxMorph {
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

impl RlpEcdsaEncodableTx for TxMorph {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.fields_len()
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.encode_fields(out);
    }
}

impl RlpEcdsaDecodableTx for TxMorph {
    const DEFAULT_TX_TYPE: u8 = { Self::tx_type() };

    /// Decodes the inner [TxEip1559] fields from RLP bytes.
    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::decode_fields(buf)
    }
}

impl SignableTransaction<Signature> for TxMorph {
    fn set_chain_id(&mut self, chain_id: ChainId) {
        self.chain_id = chain_id;
    }

    fn encode_for_signing(&self, out: &mut dyn alloy_rlp::BufMut) {
        // Use the dedicated sigHash encoding which includes Version IN the RLP for V1
        self.encode_for_sig_hash(out);
    }

    fn payload_len_for_signature(&self) -> usize {
        // txType (1 byte) + RLP header + sig hash fields
        let payload_length = self.sig_hash_fields_len();
        1 + Header {
            list: true,
            payload_length,
        }
        .length()
            + payload_length
    }
}

impl Encodable for TxMorph {
    /// Encodes TxMorph to RLP.
    ///
    /// For V0: RLP([fields...])
    /// For V1: [version byte] + RLP([fields...])
    fn encode(&self, out: &mut dyn BufMut) {
        if !self.is_v0() {
            // V1+: write version byte before RLP
            out.put_u8(self.version);
        }
        self.rlp_encode(out);
    }

    fn length(&self) -> usize {
        if self.is_v0() {
            self.rlp_encoded_length()
        } else {
            // V1+: version byte + RLP
            1 + self.rlp_encoded_length()
        }
    }
}

impl Decodable for TxMorph {
    /// Decodes TxMorph from RLP bytes (after txType byte is consumed).
    ///
    /// This handles both V0 and V1 formats:
    /// - V0: RLP list directly
    /// - V1: version byte + RLP list
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        let first_byte = buf[0];

        // Check if this is a version prefix (V1+) or RLP list header (V0)
        if first_byte == MORPH_TX_VERSION_1 {
            // V1: skip version byte, then decode RLP
            *buf = &buf[1..];
        } else if first_byte != 0 && first_byte < 0xC0 {
            // Invalid: not a version we support and not an RLP list
            return Err(alloy_rlp::Error::Custom("unsupported morph tx version"));
        }
        // V0: first_byte is 0 or >= 0xC0 (RLP list prefix)

        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        let remaining = buf.len();
        if header.payload_length > remaining {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        // Determine version based on what we saw
        if first_byte == MORPH_TX_VERSION_1 {
            Self::decode_fields_v1_inner(buf)
        } else {
            Self::decode_fields_v0_inner(buf)
        }
    }
}

impl Encodable2718 for TxMorph {
    fn type_flag(&self) -> Option<u8> {
        Some(MORPH_TX_TYPE_ID)
    }

    fn encode_2718_len(&self) -> usize {
        // txType (1 byte) + encode() (which includes version prefix for V1)
        1 + self.length()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        // Write txType first
        out.put_u8(MORPH_TX_TYPE_ID);
        // encode() now includes version prefix for V1
        self.encode(out);
    }
}

impl reth_primitives_traits::InMemorySize for TxMorph {
    fn size(&self) -> usize {
        Self::size(self)
    }
}

#[cfg(feature = "reth-codec")]
impl reth_codecs::Compact for TxMorph {
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
        len += (self.version as u64).to_compact(buf);
        len += (self.fee_token_id as u64).to_compact(buf);
        len += self.fee_limit.to_compact(buf);
        len += self.reference.to_compact(buf);
        // Memo is Option<Bytes>, convert to Bytes for Compact
        let memo_bytes = self.memo.clone().unwrap_or_default();
        len += memo_bytes.to_compact(buf);
        len += self.input.to_compact(buf);
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
        let (version, buf) = u64::from_compact(buf, len);
        let (fee_token_id, buf) = u64::from_compact(buf, len);
        let (fee_limit, buf) = U256::from_compact(buf, len);
        let (reference, buf) = Option::<B256>::from_compact(buf, len);
        let (memo_bytes, buf) = Bytes::from_compact(buf, len);
        let (input, buf) = Bytes::from_compact(buf, len);

        // Convert Bytes to Option<Bytes> (empty = None)
        let memo = if memo_bytes.is_empty() {
            None
        } else {
            Some(memo_bytes)
        };

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
                version: version as u8,
                fee_token_id: fee_token_id as u16,
                fee_limit,
                reference,
                memo,
                input,
            },
            buf,
        )
    }
}

/// Extension trait for [`TxMorph`] to access Morph-specific fields.
pub trait TxMorphExt {
    /// Returns the version of the Morph transaction format.
    fn version(&self) -> u8;

    /// Returns the token ID used for fee payment.
    fn fee_token_id(&self) -> u16;

    /// Returns the maximum token amount for fee payment.
    fn fee_limit(&self) -> U256;

    /// Returns the reference key for the transaction.
    fn reference(&self) -> Option<B256>;

    /// Returns the memo field.
    fn memo(&self) -> Option<&Bytes>;

    /// Returns true if this transaction uses token-based fee payment.
    fn uses_token_fee(&self) -> bool {
        self.fee_token_id() > 0
    }

    /// Returns true if this transaction has a reference.
    fn has_reference(&self) -> bool {
        self.reference().is_some()
    }

    /// Returns true if this transaction has a memo.
    fn has_memo(&self) -> bool {
        self.memo().is_some_and(|m| !m.is_empty())
    }
}

impl TxMorphExt for TxMorph {
    fn version(&self) -> u8 {
        self.version
    }

    fn fee_token_id(&self) -> u16 {
        self.fee_token_id
    }

    fn fee_limit(&self) -> U256 {
        self.fee_limit
    }

    fn reference(&self) -> Option<B256> {
        self.reference
    }

    fn memo(&self) -> Option<&Bytes> {
        self.memo.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_morph_transaction_default() {
        let tx = TxMorph::default();
        assert_eq!(tx.chain_id, 0);
        assert_eq!(tx.nonce, 0);
        assert_eq!(tx.gas_limit, 0);
        assert_eq!(tx.max_fee_per_gas, 0);
        assert_eq!(tx.max_priority_fee_per_gas, 0);
        assert_eq!(tx.value, U256::ZERO);
        assert_eq!(tx.version, 0);
        assert_eq!(tx.fee_token_id, 0);
        assert_eq!(tx.fee_limit, U256::ZERO);
        assert_eq!(tx.reference, None);
        assert_eq!(tx.memo, None);
        assert!(tx.is_v0());
        assert!(!tx.is_v1());
    }

    #[test]
    fn test_morph_transaction_tx_type() {
        assert_eq!(TxMorph::tx_type(), MORPH_TX_TYPE_ID);
        assert_eq!(TxMorph::tx_type(), 0x7F);
    }

    #[test]
    fn test_morph_transaction_validate() {
        // Valid V1 tx (no fee token required)
        let valid_v1 = TxMorph {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 50,
            version: MORPH_TX_VERSION_1,
            ..Default::default()
        };
        assert!(valid_v1.validate().is_ok());

        // Valid V0 tx (requires fee_token_id > 0)
        let valid_v0 = TxMorph {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 50,
            version: MORPH_TX_VERSION_0,
            fee_token_id: 1, // Required for V0
            ..Default::default()
        };
        assert!(valid_v0.validate().is_ok());

        // Invalid: priority fee > max fee
        let invalid_priority = TxMorph {
            max_fee_per_gas: 50,
            max_priority_fee_per_gas: 100,
            version: MORPH_TX_VERSION_1,
            ..Default::default()
        };
        assert!(invalid_priority.validate().is_err());

        // Invalid: V0 without fee_token_id
        let invalid_v0 = TxMorph {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 50,
            version: MORPH_TX_VERSION_0,
            fee_token_id: 0, // Invalid for V0
            ..Default::default()
        };
        assert!(invalid_v0.validate().is_err());
        assert_eq!(
            invalid_v0.validate().unwrap_err(),
            "version 0 MorphTx requires FeeTokenID > 0"
        );

        // Invalid: V0 with Reference
        let v0_with_ref = TxMorph {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 50,
            version: MORPH_TX_VERSION_0,
            fee_token_id: 1,
            reference: Some(B256::from([0x42; 32])),
            ..Default::default()
        };
        assert!(v0_with_ref.validate().is_err());
        assert_eq!(
            v0_with_ref.validate().unwrap_err(),
            "version 0 MorphTx does not support Reference field"
        );

        // Invalid: V0 with Memo
        let v0_with_memo = TxMorph {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 50,
            version: MORPH_TX_VERSION_0,
            fee_token_id: 1,
            memo: Some(Bytes::from(vec![0xca, 0xfe])),
            ..Default::default()
        };
        assert!(v0_with_memo.validate().is_err());
        assert_eq!(
            v0_with_memo.validate().unwrap_err(),
            "version 0 MorphTx does not support Memo field"
        );

        // Invalid: V1 with FeeLimit but no FeeTokenID
        let v1_fee_limit_no_token = TxMorph {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 50,
            version: MORPH_TX_VERSION_1,
            fee_token_id: 0,
            fee_limit: U256::from(1000u64), // Invalid when fee_token_id is 0
            ..Default::default()
        };
        assert!(v1_fee_limit_no_token.validate().is_err());
        assert_eq!(
            v1_fee_limit_no_token.validate().unwrap_err(),
            "version 1 MorphTx cannot have FeeLimit when FeeTokenID is 0"
        );

        // Valid: V1 with FeeTokenID=0 and FeeLimit=0
        let v1_no_fee = TxMorph {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 50,
            version: MORPH_TX_VERSION_1,
            fee_token_id: 0,
            fee_limit: U256::ZERO,
            reference: Some(B256::from([0x42; 32])),
            ..Default::default()
        };
        assert!(v1_no_fee.validate().is_ok());

        // Invalid: unsupported version
        let invalid_version = TxMorph {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 50,
            version: 99,
            ..Default::default()
        };
        assert!(invalid_version.validate().is_err());
        assert_eq!(
            invalid_version.validate().unwrap_err(),
            "unsupported MorphTx version"
        );
    }

    #[test]
    fn test_morph_transaction_effective_gas_price() {
        let tx = TxMorph {
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
    fn test_morph_transaction_trait_methods() {
        let reference = B256::from([0x42u8; 32]);
        let memo = Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]);
        let tx = TxMorph {
            chain_id: 1,
            nonce: 42,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 20,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::from(vec![1, 2, 3, 4]),
            version: 1,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: Some(reference),
            memo: Some(memo.clone()),
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
        assert_eq!(Typed2718::ty(&tx), MORPH_TX_TYPE_ID);
        assert!(tx.access_list().is_some());
        assert!(tx.blob_versioned_hashes().is_none());
        assert!(tx.authorization_list().is_none());

        // Test TxMorphExt trait methods
        assert_eq!(tx.version(), 1);
        assert_eq!(tx.fee_token_id(), 1);
        assert_eq!(tx.fee_limit(), U256::from(1000u64));
        assert_eq!(tx.reference(), Some(reference));
        assert_eq!(tx.memo(), Some(&memo));
        assert!(tx.uses_token_fee());
        assert!(tx.has_reference());
        assert!(tx.has_memo());
        assert!(tx.is_v1());
    }

    #[test]
    fn test_morph_transaction_is_create() {
        let create_tx = TxMorph {
            to: TxKind::Create,
            ..Default::default()
        };
        assert!(create_tx.is_create());

        let call_tx = TxMorph {
            to: TxKind::Call(address!("0000000000000000000000000000000000000001")),
            ..Default::default()
        };
        assert!(!call_tx.is_create());
    }

    #[test]
    fn test_morph_transaction_rlp_roundtrip_v1() {
        let reference = B256::from([0xab; 32]);
        let memo = Bytes::from(vec![0xca, 0xfe]);
        let tx = TxMorph {
            chain_id: 1,
            nonce: 42,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(1_000_000_000_000_000_000u128),
            access_list: AccessList::default(),
            input: Bytes::from(vec![0x12, 0x34]),
            version: 1, // V1 format
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: Some(reference),
            memo: Some(memo),
        };

        // Encode
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // Decode
        let decoded = TxMorph::decode(&mut buf.as_slice()).expect("Should decode V1");

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
        assert_eq!(tx.version, decoded.version);
        assert_eq!(tx.fee_token_id, decoded.fee_token_id);
        assert_eq!(tx.fee_limit, decoded.fee_limit);
        assert_eq!(tx.reference, decoded.reference);
        assert_eq!(tx.memo, decoded.memo);
        assert!(decoded.is_v1());
    }

    #[test]
    fn test_morph_transaction_rlp_roundtrip_v0() {
        let tx = TxMorph {
            chain_id: 1,
            nonce: 42,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(1_000_000_000_000_000_000u128),
            access_list: AccessList::default(),
            input: Bytes::from(vec![0x12, 0x34]),
            version: 0, // V0 format (legacy)
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: None, // V0 has no reference
            memo: None,      // V0 has no memo
        };

        // Encode
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // Decode
        let decoded = TxMorph::decode(&mut buf.as_slice()).expect("Should decode V0");

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
        assert_eq!(decoded.version, MORPH_TX_VERSION_0);
        assert_eq!(tx.fee_token_id, decoded.fee_token_id);
        assert_eq!(tx.fee_limit, decoded.fee_limit);
        assert_eq!(decoded.reference, None);
        assert_eq!(decoded.memo, None);
        assert!(decoded.is_v0());
    }

    #[test]
    fn test_morph_transaction_create() {
        let tx = TxMorph {
            chain_id: 1,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Create,
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: Bytes::from(vec![0x60, 0x80, 0x60, 0x40]),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
        };

        // Encode
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // Decode
        let decoded = TxMorph::decode(&mut buf.as_slice()).expect("Should decode");

        assert_eq!(decoded.to, TxKind::Create);
    }

    #[test]
    fn test_morph_transaction_encode_2718() {
        let tx = TxMorph {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
        };

        let mut buf = Vec::new();
        tx.encode_2718(&mut buf);

        // First byte should be the type ID
        assert_eq!(buf[0], MORPH_TX_TYPE_ID);

        // Verify type_flag
        assert_eq!(tx.type_flag(), Some(MORPH_TX_TYPE_ID));

        // Verify length consistency
        assert_eq!(buf.len(), tx.encode_2718_len());
    }

    #[test]
    fn test_morph_transaction_decode_rejects_malformed_rlp() {
        let tx = TxMorph {
            chain_id: 1,
            nonce: 42,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(1_000_000_000_000_000_000u128),
            access_list: AccessList::default(),
            input: Bytes::from(vec![0x12, 0x34]),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
        };

        // Encode the transaction
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // Corrupt by truncating
        let original_len = buf.len();
        buf.truncate(original_len - 5);

        let result = TxMorph::decode(&mut buf.as_slice());
        assert!(
            result.is_err(),
            "Decoding should fail when data is truncated"
        );
    }

    #[test]
    fn test_morph_transaction_size() {
        let tx = TxMorph {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 20,
            to: TxKind::Create,
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: Bytes::new(),
            version: 0,
            fee_token_id: 0,
            fee_limit: U256::ZERO,
            reference: None,
            memo: None,
        };

        let size = tx.size();
        assert!(size > 0);
    }

    #[test]
    fn test_morph_transaction_fields_len() {
        let tx = TxMorph {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::from(vec![1, 2, 3, 4]),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
        };

        let fields_len = tx.fields_len();
        assert!(fields_len > 0);

        // Verify encode_2718_len is consistent
        let encode_2718_len = tx.encode_2718_len();
        assert!(encode_2718_len > fields_len);
    }

    #[test]
    fn test_morph_transaction_encode_fields() {
        let tx = TxMorph {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
        };

        let mut buf = Vec::new();
        tx.encode_fields(&mut buf);

        // Should have encoded fields
        assert!(!buf.is_empty());
        assert_eq!(buf.len(), tx.fields_len());
    }

    #[test]
    fn test_morph_transaction_uses_token_fee() {
        let tx_with_token = TxMorph {
            fee_token_id: 1,
            ..Default::default()
        };
        assert!(tx_with_token.uses_token_fee());

        let tx_without_token = TxMorph {
            fee_token_id: 0,
            ..Default::default()
        };
        assert!(!tx_without_token.uses_token_fee());
    }

    #[test]
    fn test_morph_transaction_signature_hash() {
        let tx = TxMorph {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
        };

        let hash = tx.signature_hash();
        assert_ne!(hash, B256::ZERO);
    }

    #[test]
    fn test_morph_transaction_with_reference_and_memo() {
        let reference = B256::from([0x42u8; 32]);
        let memo = Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]);

        let tx = TxMorph {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
            version: 1,
            fee_token_id: 0, // No token fee, but still a MorphTx
            fee_limit: U256::ZERO,
            reference: Some(reference),
            memo: Some(memo.clone()),
        };

        // Test trait methods
        assert_eq!(tx.version(), 1);
        assert_eq!(tx.reference(), Some(reference));
        assert_eq!(tx.memo(), Some(&memo));
        assert!(!tx.uses_token_fee()); // fee_token_id is 0
        assert!(tx.has_reference());
        assert!(tx.has_memo());
        assert!(tx.is_v1());

        // Test RLP roundtrip
        let mut buf = Vec::new();
        tx.encode(&mut buf);
        let decoded = TxMorph::decode(&mut buf.as_slice()).expect("Should decode");

        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.reference, Some(reference));
        assert_eq!(decoded.memo, Some(memo));
        assert!(decoded.is_v1());
    }

    #[test]
    fn test_morph_transaction_memo_validation() {
        // Valid memo (under 64 bytes) - use V1 since it doesn't require fee_token_id
        let valid_tx = TxMorph {
            version: MORPH_TX_VERSION_1,
            memo: Some(Bytes::from(vec![0u8; 64])),
            ..Default::default()
        };
        assert!(valid_tx.validate().is_ok());

        // Invalid memo (over 64 bytes)
        let invalid_tx = TxMorph {
            version: MORPH_TX_VERSION_1,
            memo: Some(Bytes::from(vec![0u8; 65])),
            ..Default::default()
        };
        assert!(invalid_tx.validate().is_err());
        assert_eq!(
            invalid_tx.validate().unwrap_err(),
            "memo exceeds maximum length of 64 bytes"
        );
    }

    #[test]
    fn test_morph_transaction_v0_v1_encoding_difference() {
        // V0 transaction
        let v0_tx = TxMorph {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000001")),
            value: U256::from(100u64),
            version: 0, // V0
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            ..Default::default()
        };

        // V1 transaction with same base fields but with reference/memo
        let v1_tx = TxMorph {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000001")),
            value: U256::from(100u64),
            version: 1, // V1
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: Some(B256::from([0xab; 32])),
            memo: Some(Bytes::from(vec![0xca, 0xfe])),
            ..Default::default()
        };

        let mut v0_buf = Vec::new();
        v0_tx.encode(&mut v0_buf);

        let mut v1_buf = Vec::new();
        v1_tx.encode(&mut v1_buf);

        // V1 should be longer due to version byte prefix, Reference, and Memo fields
        assert!(
            v1_buf.len() > v0_buf.len(),
            "V1 encoding ({}) should be longer than V0 ({})",
            v1_buf.len(),
            v0_buf.len()
        );

        // Both should decode correctly
        let decoded_v0 = TxMorph::decode(&mut v0_buf.as_slice()).expect("V0 decode");
        let decoded_v1 = TxMorph::decode(&mut v1_buf.as_slice()).expect("V1 decode");

        assert!(decoded_v0.is_v0());
        assert!(decoded_v1.is_v1());
        assert_eq!(decoded_v1.reference, Some(B256::from([0xab; 32])));
        assert_eq!(decoded_v1.memo, Some(Bytes::from(vec![0xca, 0xfe])));
    }

    #[test]
    fn test_morph_transaction_encode_2718_v1_with_version_prefix() {
        // V1 transaction
        let tx = TxMorph {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
            version: 1, // V1
            fee_token_id: 0,
            fee_limit: U256::ZERO,
            reference: Some(B256::from([0xab; 32])),
            memo: Some(Bytes::from(vec![0xca, 0xfe])),
        };

        let mut buf = Vec::new();
        tx.encode_2718(&mut buf);

        // First byte should be txType (0x7F)
        assert_eq!(buf[0], MORPH_TX_TYPE_ID);

        // Second byte should be version (0x01) for V1
        assert_eq!(buf[1], MORPH_TX_VERSION_1);

        // Third byte should be RLP list prefix (>= 0xC0)
        assert!(
            buf[2] >= 0xC0,
            "Third byte should be RLP list prefix, got 0x{:02x}",
            buf[2]
        );

        // Verify length consistency
        assert_eq!(buf.len(), tx.encode_2718_len());
    }

    #[test]
    fn test_morph_transaction_encode_2718_v0_no_version_prefix() {
        // V0 transaction
        let tx = TxMorph {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(100u64),
            access_list: AccessList::default(),
            input: Bytes::new(),
            version: 0, // V0
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
        };

        let mut buf = Vec::new();
        tx.encode_2718(&mut buf);

        // First byte should be txType (0x7F)
        assert_eq!(buf[0], MORPH_TX_TYPE_ID);

        // Second byte should be RLP list prefix (>= 0xC0) - NO version byte for V0
        assert!(
            buf[1] >= 0xC0,
            "Second byte should be RLP list prefix for V0, got 0x{:02x}",
            buf[1]
        );

        // Verify length consistency
        assert_eq!(buf.len(), tx.encode_2718_len());
    }

    #[test]
    fn test_morph_transaction_v0_requires_fee_token_id() {
        // V0 with fee_token_id = 0 should fail to decode
        let tx = TxMorph {
            chain_id: 1,
            nonce: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000001")),
            value: U256::from(100u64),
            version: 0,
            fee_token_id: 0, // Invalid for V0
            fee_limit: U256::ZERO,
            ..Default::default()
        };

        // Validation should fail
        assert!(tx.validate_version().is_err());

        // We can still encode it (encoding doesn't validate)
        let mut buf = Vec::new();
        tx.encode(&mut buf);

        // But decoding should fail because V0 requires fee_token_id > 0
        let result = TxMorph::decode(&mut buf.as_slice());
        assert!(
            result.is_err(),
            "V0 with fee_token_id=0 should fail to decode"
        );
    }

    #[test]
    fn test_morph_transaction_version_helpers() {
        // V0 transaction
        let v0_tx = TxMorph {
            version: MORPH_TX_VERSION_0,
            fee_token_id: 1,
            ..Default::default()
        };
        assert!(v0_tx.is_v0());
        assert!(!v0_tx.is_v1());

        // V1 transaction
        let v1_tx = TxMorph {
            version: MORPH_TX_VERSION_1,
            ..Default::default()
        };
        assert!(!v1_tx.is_v0());
        assert!(v1_tx.is_v1());

        // Unsupported version (e.g., 2) - neither is_v0 nor is_v1
        let v2_tx = TxMorph {
            version: 2,
            ..Default::default()
        };
        assert!(!v2_tx.is_v0());
        assert!(!v2_tx.is_v1()); // is_v1 uses == not >=, so version 2 is not v1
    }

    #[test]
    fn test_morph_transaction_v0_no_reference_memo() {
        // V0 with Reference should fail validation
        let v0_with_ref = TxMorph {
            version: MORPH_TX_VERSION_0,
            fee_token_id: 1,
            reference: Some(B256::from([0x42; 32])),
            ..Default::default()
        };
        assert!(v0_with_ref.validate_version().is_err());
        assert_eq!(
            v0_with_ref.validate_version().unwrap_err(),
            "version 0 MorphTx does not support Reference field"
        );

        // V0 with Memo should fail validation
        let v0_with_memo = TxMorph {
            version: MORPH_TX_VERSION_0,
            fee_token_id: 1,
            memo: Some(Bytes::from(vec![0xca, 0xfe])),
            ..Default::default()
        };
        assert!(v0_with_memo.validate_version().is_err());
        assert_eq!(
            v0_with_memo.validate_version().unwrap_err(),
            "version 0 MorphTx does not support Memo field"
        );

        // V0 with empty Memo should pass (empty is treated as not set)
        let v0_empty_memo = TxMorph {
            version: MORPH_TX_VERSION_0,
            fee_token_id: 1,
            memo: Some(Bytes::new()), // Empty memo
            ..Default::default()
        };
        assert!(v0_empty_memo.validate_version().is_ok());
    }

    #[test]
    fn test_morph_transaction_v1_fee_limit_validation() {
        // V1 with FeeTokenID=0 and FeeLimit>0 should fail
        let v1_invalid = TxMorph {
            version: MORPH_TX_VERSION_1,
            fee_token_id: 0,
            fee_limit: U256::from(1000u64),
            ..Default::default()
        };
        assert!(v1_invalid.validate_version().is_err());
        assert_eq!(
            v1_invalid.validate_version().unwrap_err(),
            "version 1 MorphTx cannot have FeeLimit when FeeTokenID is 0"
        );

        // V1 with FeeTokenID=0 and FeeLimit=0 should pass
        let v1_valid_no_fee = TxMorph {
            version: MORPH_TX_VERSION_1,
            fee_token_id: 0,
            fee_limit: U256::ZERO,
            reference: Some(B256::from([0x42; 32])),
            memo: Some(Bytes::from(vec![0xca, 0xfe])),
            ..Default::default()
        };
        assert!(v1_valid_no_fee.validate_version().is_ok());

        // V1 with FeeTokenID>0 and FeeLimit>0 should pass
        let v1_valid_with_fee = TxMorph {
            version: MORPH_TX_VERSION_1,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: Some(B256::from([0x42; 32])),
            ..Default::default()
        };
        assert!(v1_valid_with_fee.validate_version().is_ok());
    }

    #[test]
    fn test_morph_transaction_decode_rejects_oversized_memo() {
        use alloy_rlp::Encodable;

        // Create a valid V1 transaction with oversized memo (65 bytes, exceeds MAX_MEMO_LENGTH=64)
        let oversized_memo = vec![0xab; 65];

        // Manually construct RLP with oversized memo
        // V1 format: version_byte + RLP([chain_id, nonce, max_priority_fee, max_fee, gas_limit,
        //            to, value, input, access_list, fee_token_id, fee_limit, reference, memo])
        let mut inner_buf = Vec::new();

        // Encode all fields
        1u64.encode(&mut inner_buf); // chain_id
        0u64.encode(&mut inner_buf); // nonce
        0u128.encode(&mut inner_buf); // max_priority_fee_per_gas
        1000u128.encode(&mut inner_buf); // max_fee_per_gas
        21000u128.encode(&mut inner_buf); // gas_limit
        alloy_primitives::TxKind::Create.encode(&mut inner_buf); // to
        U256::ZERO.encode(&mut inner_buf); // value
        Bytes::new().encode(&mut inner_buf); // input
        alloy_eips::eip2930::AccessList::default().encode(&mut inner_buf); // access_list
        1u16.encode(&mut inner_buf); // fee_token_id
        U256::from(1000u64).encode(&mut inner_buf); // fee_limit
        Bytes::new().encode(&mut inner_buf); // reference (empty)
        Bytes::from(oversized_memo).encode(&mut inner_buf); // memo (oversized!)

        // Wrap in RLP list
        let header = alloy_rlp::Header {
            list: true,
            payload_length: inner_buf.len(),
        };
        let mut rlp_buf = Vec::new();
        header.encode(&mut rlp_buf);
        rlp_buf.extend_from_slice(&inner_buf);

        // Try to decode as V1
        let result = TxMorph::decode_fields_v1(&mut rlp_buf.as_slice());
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), alloy_rlp::Error::Custom(_)));
    }
}
