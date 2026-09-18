//! Morph Transaction type for Morph L2.
//!
//! This module defines the TxMorph type which represents Morph-specific transactions
//! that support:
//! - ERC20 tokens for gas payment instead of native ETH
//! - Transaction reference for indexing/lookup
//! - Memo field for arbitrary data
//! - EIP-7702 authorization list (version 2, Celadon onwards)
//!
//! Wire formats (after the `0x7F` type byte):
//! - V0: `RLP([chainId, nonce, gasTipCap, gasFeeCap, gas, to, value, data, accessList, feeTokenID, feeLimit, V, R, S])`
//! - V1: `0x01 || RLP([..., feeTokenID, feeLimit, reference, memo, V, R, S])`
//! - V2: `0x02 || RLP([..., feeTokenID, feeLimit, reference, memo, authorizationList, V, R, S])`
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

/// MorphTx version 2: V1 fields plus an EIP-7702 authorization list.
///
/// The list may be empty, in which case the transaction behaves exactly like a
/// V1 transaction (only the wire version byte and the empty list field differ).
/// The authorization tuples use the standard EIP-7702 structure, encoding and
/// signing domain (`keccak256(0x05 || rlp([chainId, address, nonce]))`), so
/// authority recovery, intrinsic gas and delegation semantics are identical to
/// the `0x04` SetCode transaction.
pub const MORPH_TX_VERSION_2: u8 = 2;

/// Maximum length of the memo field in bytes.
pub const MAX_MEMO_LENGTH: usize = 64;

#[cfg(feature = "serde")]
fn is_morph_tx_version_0(version: &u8) -> bool {
    *version == MORPH_TX_VERSION_0
}

/// Canonical MorphTx-specific fields shared across modules.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct MorphTxFields {
    #[cfg_attr(feature = "serde", serde(default, with = "alloy_serde::quantity"))]
    pub version: u8,
    #[cfg_attr(
        feature = "serde",
        serde(
            default,
            with = "alloy_serde::quantity",
            rename = "feeTokenID",
            alias = "feeTokenId"
        )
    )]
    pub fee_token_id: u16,
    #[cfg_attr(feature = "serde", serde(default))]
    pub fee_limit: U256,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub reference: Option<B256>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub memo: Option<Bytes>,
}

/// Morph Transaction for Morph L2.
///
/// This transaction type extends EIP-1559 style transactions with Morph-specific fields:
/// - Token-based fee payment (ERC20 tokens instead of native ETH)
/// - Transaction reference for indexing/lookup by external systems
/// - Memo field for arbitrary data
///
/// Reference: <https://github.com/morph-l2/go-ethereum/pull/282>
///
/// JSON serialization is implemented by hand (see the `Serialize` impl below)
/// because whether `authorizationList` is emitted depends on the version.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
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
    /// Matches go-ethereum's `AltFeeTx.Gas` (uint64).
    #[cfg_attr(
        feature = "serde",
        serde(with = "alloy_serde::quantity", rename = "gas", alias = "gasLimit")
    )]
    pub gas_limit: u64,

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
    #[cfg_attr(feature = "serde", serde(default, with = "alloy_serde::quantity"))]
    pub version: u8,

    /// Token ID for alternative fee payment.
    /// This corresponds to the token registered in the L2 Token Registry.
    /// 0 means ETH payment, > 0 means ERC20 token payment.
    #[cfg_attr(
        feature = "serde",
        serde(
            default,
            with = "alloy_serde::quantity",
            rename = "feeTokenID",
            alias = "feeTokenId"
        )
    )]
    pub fee_token_id: u16,

    /// Maximum amount of tokens the sender is willing to pay as fee.
    #[cfg_attr(feature = "serde", serde(default))]
    pub fee_limit: U256,

    /// Reference key for the transaction (optional, v1 only).
    /// Used for indexing and looking up transactions by external systems.
    /// This is a 32-byte value that can be used to group related transactions.
    #[cfg_attr(feature = "serde", serde(default))]
    pub reference: Option<B256>,

    /// Memo field for arbitrary data (optional, v1+).
    /// Can be used to attach additional information to the transaction.
    /// Maximum length is 64 bytes.
    #[cfg_attr(feature = "serde", serde(default))]
    pub memo: Option<Bytes>,

    /// EIP-7702 authorization list (v2 only).
    ///
    /// Always empty for V0 and V1. A V2 transaction may carry an empty list, in
    /// which case it behaves exactly like V1; the tuples are standard
    /// [`SignedAuthorization`]s and are applied exactly like an EIP-7702
    /// (`0x04`) transaction's list.
    ///
    /// JSON: every V2 transaction emits the key (an empty list as `[]`) and V0
    /// and V1 never do, matching go-ethereum's `MarshalJSON`. An absent key,
    /// `[]` and `null` all decode to an empty list.
    #[cfg_attr(
        feature = "serde",
        serde(default, deserialize_with = "alloy_serde::null_as_default")
    )]
    pub authorization_list: Vec<SignedAuthorization>,

    /// Input has two uses depending if transaction is Create or Call (if `to`
    /// field is None or Some).
    /// - init: An unlimited size byte array specifying the EVM-code for the
    ///   account initialisation procedure CREATE.
    /// - data: An unlimited size byte array specifying the input data of the
    ///   message call.
    #[cfg_attr(feature = "serde", serde(default, alias = "data"))]
    pub input: Bytes,
}

/// Same field layout as the derived `Deserialize`, except that
/// `authorizationList` follows the version instead of the list length: a V2
/// transaction always emits it, so an empty list serializes as `[]` like
/// go-ethereum does, while V0 and V1 never emit it.
#[cfg(feature = "serde")]
impl serde::Serialize for TxMorph {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Repr<'a> {
            #[serde(with = "alloy_serde::quantity")]
            chain_id: ChainId,
            #[serde(with = "alloy_serde::quantity")]
            nonce: u64,
            #[serde(with = "alloy_serde::quantity", rename = "gas")]
            gas_limit: u64,
            #[serde(with = "alloy_serde::quantity")]
            max_fee_per_gas: u128,
            #[serde(with = "alloy_serde::quantity")]
            max_priority_fee_per_gas: u128,
            to: &'a TxKind,
            value: &'a U256,
            access_list: &'a AccessList,
            #[serde(
                with = "alloy_serde::quantity",
                skip_serializing_if = "is_morph_tx_version_0"
            )]
            version: u8,
            #[serde(with = "alloy_serde::quantity", rename = "feeTokenID")]
            fee_token_id: u16,
            fee_limit: &'a U256,
            #[serde(skip_serializing_if = "Option::is_none")]
            reference: Option<&'a B256>,
            #[serde(skip_serializing_if = "Option::is_none")]
            memo: Option<&'a Bytes>,
            #[serde(skip_serializing_if = "Option::is_none")]
            authorization_list: Option<&'a Vec<SignedAuthorization>>,
            input: &'a Bytes,
        }

        Repr {
            chain_id: self.chain_id,
            nonce: self.nonce,
            gas_limit: self.gas_limit,
            max_fee_per_gas: self.max_fee_per_gas,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas,
            to: &self.to,
            value: &self.value,
            access_list: &self.access_list,
            version: self.version,
            fee_token_id: self.fee_token_id,
            fee_limit: &self.fee_limit,
            reference: self.reference.as_ref(),
            memo: self.memo.as_ref(),
            authorization_list: self.is_v2().then_some(&self.authorization_list),
            input: &self.input,
        }
        .serialize(serializer)
    }
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
    /// - Version 2 (with authorization list): all V1 rules. The authorization
    ///   list may be empty (the transaction then behaves like V1); a non-empty
    ///   list requires `to` to be a call (no CREATE), matching the EIP-7702
    ///   `0x04` static rule
    /// - Versions 0 and 1 must not carry an authorization list
    /// - Other versions: not supported
    pub fn validate_version(&self) -> Result<(), &'static str> {
        match self.version {
            MORPH_TX_VERSION_0 => {
                // Version 0 requires FeeTokenID > 0 (legacy format used for alt-fee transactions)
                if self.fee_token_id == 0 {
                    return Err("version 0 MorphTx requires FeeTokenID > 0");
                }
                // Version 0 treats an all-zero Reference as absent, matching geth's
                // RPC normalization for backward-compatible V0 transactions.
                if self
                    .reference
                    .is_some_and(|reference| reference != B256::ZERO)
                {
                    return Err("version 0 MorphTx does not support Reference field");
                }
                // Version 0 does not support Memo field
                if self.memo.as_ref().is_some_and(|m| !m.is_empty()) {
                    return Err("version 0 MorphTx does not support Memo field");
                }
                if self.has_authorizations() {
                    return Err("version 0 MorphTx does not support authorization list");
                }
            }
            MORPH_TX_VERSION_1 => {
                // Version 1: FeeTokenID, Reference, Memo are all optional
                // If FeeTokenID is 0, FeeLimit must not be set
                if self.fee_token_id == 0 && self.fee_limit > U256::ZERO {
                    return Err("version 1 MorphTx cannot have FeeLimit when FeeTokenID is 0");
                }
                if self.has_authorizations() {
                    return Err("version 1 MorphTx does not support authorization list");
                }
            }
            MORPH_TX_VERSION_2 => {
                if self.fee_token_id == 0 && self.fee_limit > U256::ZERO {
                    return Err("version 2 MorphTx cannot have FeeLimit when FeeTokenID is 0");
                }
                // An empty list is allowed (V2 then behaves like V1). With
                // authorizations the transaction cannot be a CREATE, the same
                // static rule as EIP-7702 SetCode transactions.
                if self.has_authorizations() && self.to.is_create() {
                    return Err("MorphTx with an authorization list cannot create a contract");
                }
            }
            _ => {
                return Err("unsupported MorphTx version");
            }
        }
        Ok(())
    }

    /// The version a MorphTx built from user intent gets.
    ///
    /// V1 is the baseline: it is a superset of V0, so the request layer never
    /// produces V0 anymore (V0 transactions that already exist stay valid).
    /// A non-empty authorization list raises the version to V2; an empty list
    /// is the same as no list.
    ///
    /// Callers that build a [`TxMorph`] by hand must derive `version` through
    /// this or [`Self::with_inferred_version`] instead of filling it in: the
    /// V0 / V1 encodings cannot carry a list, and [`Self::validate`] rejects a
    /// V0 / V1 that does.
    pub const fn inferred_version(has_authorizations: bool) -> u8 {
        if has_authorizations {
            MORPH_TX_VERSION_2
        } else {
            MORPH_TX_VERSION_1
        }
    }

    /// Sets `version` from the transaction content, see [`Self::inferred_version`].
    pub fn with_inferred_version(mut self) -> Self {
        self.version = Self::inferred_version(self.has_authorizations());
        self
    }

    /// Returns true if this is a version 0 (legacy) MorphTx.
    pub const fn is_v0(&self) -> bool {
        self.version == MORPH_TX_VERSION_0
    }

    /// Returns true if this is a version 1 MorphTx (with Reference/Memo).
    pub const fn is_v1(&self) -> bool {
        self.version == MORPH_TX_VERSION_1
    }

    /// Returns true if this is a version 2 MorphTx (with EIP-7702 authorization list).
    pub const fn is_v2(&self) -> bool {
        self.version == MORPH_TX_VERSION_2
    }

    /// Returns true if the authorization list is non-empty.
    ///
    /// This looks at the raw field regardless of `version`; use
    /// [`Transaction::authorization_list`] for the version-gated view.
    pub fn has_authorizations(&self) -> bool {
        !self.authorization_list.is_empty()
    }

    /// Authorization tuples that are part of the V2 wire and signing encodings.
    ///
    /// Only V2 encodes the list (an empty V2 list encodes as the empty RLP list
    /// `0xc0`); V0/V1 must never carry one (see [`Self::validate_version`]).
    /// The debug assertion catches callers that encode such an inconsistent
    /// transaction instead of silently dropping the list from the wire bytes.
    ///
    /// Returns a `Vec` reference because alloy-rlp implements `Encodable` for
    /// `Vec<T>` but not for `[T]`.
    fn encoded_authorization_list(&self) -> &Vec<SignedAuthorization> {
        static EMPTY: Vec<SignedAuthorization> = Vec::new();
        if self.is_v2() {
            &self.authorization_list
        } else {
            debug_assert!(
                !self.has_authorizations(),
                "MorphTx version {} must not carry an authorization list",
                self.version
            );
            &EMPTY
        }
    }

    /// Calculate the in-memory size of this transaction.
    pub fn size(&self) -> usize {
        mem::size_of::<ChainId>() + // chain_id
        mem::size_of::<u64>() + // nonce
        mem::size_of::<u64>() + // gas_limit
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
        mem::size_of::<Vec<SignedAuthorization>>() + // authorization_list
        self.authorization_list.len() * mem::size_of::<SignedAuthorization>() +
        self.input.len() // input
    }

    /// Outputs the length of the transaction's RLP fields, without a RLP header.
    ///
    /// Note: For V1+, the version byte is NOT included here - it's encoded as a prefix byte
    /// before the RLP data, similar to txType.
    ///
    /// V0 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit
    /// V1 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit, Reference, Memo
    /// V2 format: V1 fields, AuthorizationList
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
        if self.is_v2() {
            // V2 format: adds the EIP-7702 authorization list after Memo
            len += self.encoded_authorization_list().length();
        }
        len
    }

    /// Encodes only the transaction's RLP fields into the desired buffer, without a RLP header.
    ///
    /// Note: For V1+, the version byte is NOT included here - it's encoded as a prefix byte
    /// before the RLP data by the caller (encode_2718).
    ///
    /// V0 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit
    /// V1 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit, Reference, Memo
    /// V2 format: V1 fields, AuthorizationList
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
        if self.is_v2() {
            // V2 format: EIP-7702 authorization list, encoded exactly like TxEip7702
            self.encoded_authorization_list().encode(out);
        }
    }

    /// Determines the wire-format version from the first byte after the txType byte.
    ///
    /// - `0x00` or an RLP list prefix (`>= 0xC0`): V0 (no version byte; matches
    ///   go-ethereum's `decode()` which routes `firstByte == 0` to V0)
    /// - `0x01`: V1
    /// - `0x02`: V2
    /// - anything else: unsupported
    ///
    /// Returns the version and whether a version byte must be skipped.
    fn wire_version(first_byte: u8) -> alloy_rlp::Result<(u8, bool)> {
        if first_byte == MORPH_TX_VERSION_0 || first_byte >= 0xC0 {
            Ok((MORPH_TX_VERSION_0, false))
        } else if first_byte == MORPH_TX_VERSION_1 || first_byte == MORPH_TX_VERSION_2 {
            Ok((first_byte, true))
        } else {
            Err(alloy_rlp::Error::Custom("unsupported morph tx version"))
        }
    }

    /// Decodes the inner fields from RLP bytes (after txType byte is consumed).
    ///
    /// Version detection based on first byte:
    /// - V0 format: first byte is 0 or RLP list prefix (>= 0xC0) → direct RLP decode
    /// - V1/V2 format: first byte is version (0x01 / 0x02) → skip version byte, then RLP decode
    ///
    /// V0 RLP: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit
    /// V1 RLP: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit, Reference, Memo
    /// V2 RLP: V1 fields, AuthorizationList
    pub fn decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        let (version, has_version_byte) = Self::wire_version(buf[0])?;
        if has_version_byte {
            *buf = &buf[1..];
        }

        match version {
            MORPH_TX_VERSION_0 => Self::decode_fields_v0(buf),
            MORPH_TX_VERSION_1 => Self::decode_fields_v1(buf),
            _ => Self::decode_fields_v2(buf),
        }
    }

    /// Decodes V0 format fields (for decode_fields, includes RLP header handling).
    ///
    /// V0 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit
    fn decode_fields_v0(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::decode_exact_list(buf, Self::decode_fields_v0_inner)
    }

    /// Decodes V1 format fields (for decode_fields, includes RLP header handling).
    ///
    /// V1 format (after version byte is consumed):
    /// ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit, Reference, Memo
    ///
    /// Note: Version is NOT in the RLP - it was already consumed as a prefix byte.
    fn decode_fields_v1(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::decode_fields_versioned(buf, MORPH_TX_VERSION_1)
    }

    /// Decodes V2 format fields (for decode_fields, includes RLP header handling).
    ///
    /// V2 format (after version byte is consumed): V1 fields, AuthorizationList
    fn decode_fields_v2(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::decode_fields_versioned(buf, MORPH_TX_VERSION_2)
    }

    /// Decodes V1/V2 format fields, including the RLP list header.
    fn decode_fields_versioned(buf: &mut &[u8], version: u8) -> alloy_rlp::Result<Self> {
        Self::decode_exact_list(buf, |buf| Self::decode_fields_versioned_inner(buf, version))
    }

    /// Decodes an RLP list header followed by `decode_inner`, requiring the list
    /// to be consumed exactly: surplus elements are rejected with
    /// [`alloy_rlp::Error::ListLengthMismatch`], as in [`Decodable::decode`].
    fn decode_exact_list(
        buf: &mut &[u8],
        decode_inner: impl FnOnce(&mut &[u8]) -> alloy_rlp::Result<Self>,
    ) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        let remaining = buf.len();
        if header.payload_length > remaining {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        let tx = decode_inner(buf)?;

        if buf.len() + header.payload_length != remaining {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: remaining - buf.len(),
            });
        }

        Ok(tx)
    }

    /// Decodes V1 format fields (inner, assumes RLP header already consumed).
    fn decode_fields_v1_inner(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::decode_fields_versioned_inner(buf, MORPH_TX_VERSION_1)
    }

    /// Decodes V2 format fields (inner, assumes RLP header already consumed).
    fn decode_fields_v2_inner(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::decode_fields_versioned_inner(buf, MORPH_TX_VERSION_2)
    }

    /// Decodes V1/V2 format fields (inner, assumes RLP header already consumed).
    ///
    /// V2 reads one extra field, the EIP-7702 authorization list, after Memo.
    /// An empty V2 list is valid and decodes as an empty list (behaving like V1).
    fn decode_fields_versioned_inner(buf: &mut &[u8], version: u8) -> alloy_rlp::Result<Self> {
        debug_assert!(
            version == MORPH_TX_VERSION_1 || version == MORPH_TX_VERSION_2,
            "versioned decoder only handles V1 and V2"
        );
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

        // V2 only: authorization list, same RLP shape as TxEip7702 (may be empty).
        let authorization_list = if version == MORPH_TX_VERSION_2 {
            Vec::<SignedAuthorization>::decode(buf)?
        } else {
            Vec::new()
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
            version,
            fee_token_id,
            fee_limit,
            reference,
            memo,
            authorization_list,
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
            authorization_list: Vec::new(),
        })
    }

    /// Computes the hash used for signing the transaction.
    ///
    /// Note: The sigHash encoding differs from transaction encoding for V1+:
    /// - Transaction encoding: `[version byte] + RLP([..., FeeTokenID, FeeLimit, Reference, Memo, (AuthorizationList)])`
    /// - SigHash encoding: `TxType + RLP([..., FeeTokenID, FeeLimit, Version, Reference, Memo, (AuthorizationList)])`
    ///
    /// For V1+, Version is included IN the RLP for signing, not as a prefix.
    /// V2 appends the authorization list after Memo in both encodings.
    pub fn signature_hash(&self) -> B256 {
        let mut buf = Vec::new();
        self.encode_for_sig_hash(&mut buf);
        keccak256(&buf)
    }

    /// Encodes the transaction for signature hash calculation.
    ///
    /// V0 format: TxType + RLP([..., FeeTokenID, FeeLimit])
    /// V1 format: TxType + RLP([..., FeeTokenID, FeeLimit, Version, Reference, Memo])
    /// V2 format: TxType + RLP([..., FeeTokenID, FeeLimit, Version, Reference, Memo, AuthorizationList])
    ///
    /// Note: For V1+, Version is included in the RLP (after FeeLimit), not as a prefix byte.
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
            // V1+ sigHash: includes Version, Reference, Memo IN the RLP
            len += self.version.length();
            len += self
                .reference
                .as_ref()
                .map_or(0usize.length(), |r| r.0.length());
            len += self.memo.as_ref().map_or(0usize.length(), |m| m.0.length());
        }
        if self.is_v2() {
            // V2 sigHash: authorization list is covered by the signature
            len += self.encoded_authorization_list().length();
        }
        len
    }

    /// Encodes fields for signature hash calculation.
    ///
    /// V0 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit
    /// V1 format: ChainID, Nonce, GasTipCap, GasFeeCap, Gas, To, Value, Data, AccessList, FeeTokenID, FeeLimit, Version, Reference, Memo
    /// V2 format: V1 fields, AuthorizationList
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
            // V1+ sigHash: includes Version, Reference, Memo IN the RLP
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
        if self.is_v2() {
            // V2 sigHash: authorization list is covered by the signature
            self.encoded_authorization_list().encode(out);
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
        self.gas_limit
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

    /// Returns the EIP-7702 authorization list of a V2 transaction, if any.
    ///
    /// `None` for V0/V1 (even if the raw field is populated on an invalid
    /// in-memory value) and for a V2 transaction with an empty list, so the
    /// txpool authority tracking and the EVM authorization application only
    /// ever see lists that will actually be applied.
    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        if self.is_v2() && !self.authorization_list.is_empty() {
            Some(&self.authorization_list)
        } else {
            None
        }
    }
}

impl RlpEcdsaEncodableTx for TxMorph {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.fields_len()
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.encode_fields(out);
    }

    /// Override: For V1+, include the version byte prefix before the RLP list.
    ///
    /// Wire format:
    /// - V0: `RLP([fields..., V, R, S])`
    /// - V1: `version_byte(0x01) + RLP([fields..., V, R, S])`
    /// - V2: `version_byte(0x02) + RLP([fields..., authorizationList, V, R, S])`
    fn rlp_encode_signed(&self, signature: &Signature, out: &mut dyn BufMut) {
        if !self.is_v0() {
            out.put_u8(self.version);
        }
        self.rlp_header_signed(signature).encode(out);
        self.rlp_encode_fields(out);
        signature.write_rlp_vrs(out, signature.v());
    }

    /// Override: Account for the version byte prefix in V1 length calculation.
    fn rlp_encoded_length_with_signature(&self, signature: &Signature) -> usize {
        let base = self.rlp_header_signed(signature).length_with_payload();
        if self.is_v0() {
            base
        } else {
            base + 1 // version byte
        }
    }
}

impl RlpEcdsaDecodableTx for TxMorph {
    const DEFAULT_TX_TYPE: u8 = { Self::tx_type() };

    /// Decodes the inner TxMorph fields from RLP bytes.
    ///
    /// Note: This is only used as a fallback; the primary decode path goes through
    /// the overridden `rlp_decode_with_signature` which handles the V1 version byte.
    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::decode_fields(buf)
    }

    /// Override: Handle the V1/V2 version byte before the RLP list.
    ///
    /// Wire format (after txType byte is consumed):
    /// - V0: `RLP([fields_v0..., V, R, S])`
    /// - V1: `version_byte(0x01) + RLP([fields_v1..., V, R, S])`
    /// - V2: `version_byte(0x02) + RLP([fields_v2..., V, R, S])`
    ///
    /// The default implementation assumes the buffer starts with an RLP list header,
    /// which fails for V1+ because the first byte is the version byte.
    ///
    /// Each version has a fixed number of list elements; a payload with extra
    /// elements (e.g. a V1 prefix followed by V2 fields) fails the trailing
    /// [`alloy_rlp::Error::ListLengthMismatch`] check, matching go-ethereum's
    /// `rlp: input list has too many elements`.
    fn rlp_decode_with_signature(buf: &mut &[u8]) -> alloy_rlp::Result<(Self, Signature)> {
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        let (version, has_version_byte) = Self::wire_version(buf[0])?;
        if has_version_byte {
            *buf = &buf[1..];
        }

        // Now decode: RLP([fields..., V, R, S])
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        let remaining = buf.len();

        // Decode fields based on version
        let tx = match version {
            MORPH_TX_VERSION_0 => Self::decode_fields_v0_inner(buf)?,
            MORPH_TX_VERSION_1 => Self::decode_fields_v1_inner(buf)?,
            _ => Self::decode_fields_v2_inner(buf)?,
        };

        let signature = Signature::decode_rlp_vrs(buf, bool::decode)?;

        if buf.len() + header.payload_length != remaining {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: remaining - buf.len(),
            });
        }

        Ok((tx, signature))
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
    /// For V1+: [version byte] + RLP([fields...])
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
    /// Decodes an unsigned TxMorph from RLP bytes (after txType byte is consumed).
    ///
    /// This handles all formats:
    /// - V0: RLP list directly
    /// - V1/V2: version byte + RLP list
    ///
    /// Like the signed path, the list must be consumed exactly: extra trailing
    /// elements (e.g. an authorization list on a V1 prefix) are rejected with
    /// [`alloy_rlp::Error::ListLengthMismatch`].
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        let (version, has_version_byte) = Self::wire_version(buf[0])?;
        if has_version_byte {
            *buf = &buf[1..];
        }

        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        let remaining = buf.len();
        if header.payload_length > remaining {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        let tx = match version {
            MORPH_TX_VERSION_0 => Self::decode_fields_v0_inner(buf)?,
            MORPH_TX_VERSION_1 => Self::decode_fields_v1_inner(buf)?,
            _ => Self::decode_fields_v2_inner(buf)?,
        };

        if buf.len() + header.payload_length != remaining {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: remaining - buf.len(),
            });
        }

        Ok(tx)
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
mod compact_txmorph {
    use super::*;
    use alloy_eips::eip2930::AccessList;
    use alloy_primitives::{Bytes, ChainId, TxKind, U256};
    use reth_codecs::Compact;

    /// Helper struct for deriving `Compact` instead of manually managing bitfields.
    ///
    /// Follows the same pattern as reth's `TxEip1559` compact helper
    /// (see `reth-codecs/src/alloy/transaction/eip1559.rs`).
    ///
    /// - `version` and `fee_token_id` are stored as `u64` because `u8`/`u16` don't
    ///   implement `Compact` in reth_codecs. The conversion is lossless.
    /// - `memo` and `input` are packed into a single `Bytes` field (`data`) because
    ///   the derive macro only allows one `Bytes` field and it must be last.
    ///   Format: `[memo_len: u8][memo_bytes][input_bytes]`.
    /// - `authorization_list` (V2) was appended after `reference`. It only adds a
    ///   single presence bit to the struct flags (44 → 45 bits, still 6 flag
    ///   bytes), so rows written before V2 decode unchanged (empty list). The
    ///   layout is locked by `test_compact_decodes_pre_v2_bytes`; do not reorder.
    #[derive(Debug, Clone, PartialEq, Eq, Hash, Compact)]
    #[reth_codecs(crate = "reth_codecs")]
    struct TxMorphCompact {
        chain_id: ChainId,
        nonce: u64,
        gas_limit: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        to: TxKind,
        value: U256,
        access_list: AccessList,
        /// Stored as u64 for Compact compatibility (u8 doesn't implement Compact)
        version: u64,
        /// Stored as u64 for Compact compatibility (u16 doesn't implement Compact)
        fee_token_id: u64,
        fee_limit: U256,
        reference: Option<B256>,
        /// V2 EIP-7702 authorization list; `None` for V0/V1 rows and for V2
        /// rows whose list is empty.
        authorization_list: Option<Vec<SignedAuthorization>>,
        /// Packed: `[memo_len: u8][memo_bytes][input_bytes]` (must be last)
        data: Bytes,
    }

    impl Compact for TxMorph {
        fn to_compact<B>(&self, buf: &mut B) -> usize
        where
            B: bytes::BufMut + AsMut<[u8]>,
        {
            // Pack memo + input into a single Bytes field
            let memo_slice = self.memo.as_deref().map_or(&[] as &[u8], |v| v);
            let mut data = Vec::with_capacity(1 + memo_slice.len() + self.input.len());
            data.push(memo_slice.len() as u8); // memo max 64 bytes, fits in u8
            data.extend_from_slice(memo_slice);
            data.extend_from_slice(&self.input);

            let helper = TxMorphCompact {
                chain_id: self.chain_id,
                nonce: self.nonce,
                gas_limit: self.gas_limit,
                max_fee_per_gas: self.max_fee_per_gas,
                max_priority_fee_per_gas: self.max_priority_fee_per_gas,
                to: self.to,
                value: self.value,
                access_list: self.access_list.clone(),
                version: u64::from(self.version),
                fee_token_id: u64::from(self.fee_token_id),
                fee_limit: self.fee_limit,
                reference: self.reference,
                authorization_list: (!self.authorization_list.is_empty())
                    .then(|| self.authorization_list.clone()),
                data: data.into(),
            };
            helper.to_compact(buf)
        }

        fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
            let (helper, remaining) = TxMorphCompact::from_compact(buf, len);

            // Unpack memo + input from the combined data field
            let memo_len = helper.data[0] as usize;
            let memo = if memo_len == 0 {
                None
            } else {
                Some(Bytes::copy_from_slice(&helper.data[1..1 + memo_len]))
            };
            let input = Bytes::copy_from_slice(&helper.data[1 + memo_len..]);

            let tx = Self {
                chain_id: helper.chain_id,
                nonce: helper.nonce,
                gas_limit: helper.gas_limit,
                max_fee_per_gas: helper.max_fee_per_gas,
                max_priority_fee_per_gas: helper.max_priority_fee_per_gas,
                to: helper.to,
                value: helper.value,
                access_list: helper.access_list,
                version: helper.version as u8,
                fee_token_id: helper.fee_token_id as u16,
                fee_limit: helper.fee_limit,
                reference: helper.reference,
                memo,
                authorization_list: helper.authorization_list.unwrap_or_default(),
                input,
            };
            (tx, remaining)
        }
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

        // Valid: V0 with a zero Reference is treated as absent for geth compatibility.
        let v0_with_zero_ref = TxMorph {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 50,
            version: MORPH_TX_VERSION_0,
            fee_token_id: 1,
            reference: Some(B256::ZERO),
            ..Default::default()
        };
        assert!(v0_with_zero_ref.validate().is_ok());

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
            authorization_list: Vec::new(),
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
            authorization_list: Vec::new(),
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
            authorization_list: Vec::new(),
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
            authorization_list: Vec::new(),
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
            authorization_list: Vec::new(),
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
            authorization_list: Vec::new(),
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

    /// Issue-1: V0 payload with leading zero byte must be routed to V0
    /// decoding, matching go-ethereum's `decode()` which routes `firstByte == 0`
    /// to V0. The resulting error should be about RLP (not "unsupported version").
    #[test]
    fn test_decode_fields_accepts_zero_byte_as_v0() {
        // A single zero byte is not valid RLP, but the routing should go to V0
        // (not produce "unsupported morph tx version").
        let mut buf: &[u8] = &[0x00];
        let result = TxMorph::decode_fields(&mut buf);
        assert!(result.is_err());
        // Must not be the "unsupported version" error.
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            !err_msg.contains("unsupported"),
            "expected RLP-level error, got: {err_msg}"
        );
    }

    #[test]
    fn test_signed_decode_accepts_zero_byte_as_v0() {
        let mut buf: &[u8] = &[0x00];
        let result = TxMorph::rlp_decode_with_signature(&mut buf);

        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            !err_msg.contains("unsupported"),
            "expected RLP-level error, got: {err_msg}"
        );
    }

    #[test]
    fn test_decodable_accepts_zero_byte_as_v0() {
        let mut buf: &[u8] = &[0x00];
        let result = <TxMorph as Decodable>::decode(&mut buf);

        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            !err_msg.contains("unsupported"),
            "expected RLP-level error, got: {err_msg}"
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
            authorization_list: Vec::new(),
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
            authorization_list: Vec::new(),
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
            authorization_list: Vec::new(),
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
            authorization_list: Vec::new(),
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
            authorization_list: Vec::new(),
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
            authorization_list: Vec::new(),
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
            authorization_list: Vec::new(),
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

        // V2 transaction - neither is_v0 nor is_v1
        let v2_tx = TxMorph {
            version: MORPH_TX_VERSION_2,
            ..Default::default()
        };
        assert!(!v2_tx.is_v0());
        assert!(!v2_tx.is_v1()); // is_v1 uses == not >=, so version 2 is not v1
        assert!(v2_tx.is_v2());

        // Unsupported version (e.g., 3) - none of the helpers match
        let v3_tx = TxMorph {
            version: 3,
            ..Default::default()
        };
        assert!(!v3_tx.is_v0());
        assert!(!v3_tx.is_v1());
        assert!(!v3_tx.is_v2());
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
        21000u64.encode(&mut inner_buf); // gas_limit
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

    #[test]
    fn test_morph_signed_v1_decode_2718_roundtrip() {
        use alloy_consensus::Signed;
        use alloy_consensus::transaction::RlpEcdsaDecodableTx;
        use alloy_consensus::transaction::RlpEcdsaEncodableTx;
        use alloy_eips::eip2718::Decodable2718;

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
            version: 1, // V1
            fee_token_id: 0,
            fee_limit: U256::ZERO,
            reference: Some(reference),
            memo: Some(memo.clone()),
            authorization_list: Vec::new(),
        };

        // Create a dummy signature for testing
        let signature = Signature::new(U256::from(1u64), U256::from(2u64), false);

        // Test rlp_encode_signed → rlp_decode_with_signature roundtrip
        let mut signed_buf = Vec::new();
        tx.rlp_encode_signed(&signature, &mut signed_buf);

        // First byte should be version byte for V1
        assert_eq!(
            signed_buf[0], MORPH_TX_VERSION_1,
            "First byte should be version byte (0x01) for V1"
        );
        // Second byte should be RLP list prefix
        assert!(
            signed_buf[1] >= 0xC0,
            "Second byte should be RLP list prefix, got 0x{:02x}",
            signed_buf[1]
        );

        // Verify length consistency
        let expected_len = tx.rlp_encoded_length_with_signature(&signature);
        assert_eq!(
            signed_buf.len(),
            expected_len,
            "Encoded length should match rlp_encoded_length_with_signature"
        );

        // Decode
        let (decoded_tx, decoded_sig) =
            TxMorph::rlp_decode_with_signature(&mut signed_buf.as_slice())
                .expect("Should decode V1 signed tx");

        assert_eq!(decoded_tx.version, MORPH_TX_VERSION_1);
        assert_eq!(decoded_tx.chain_id, tx.chain_id);
        assert_eq!(decoded_tx.nonce, tx.nonce);
        assert_eq!(decoded_tx.gas_limit, tx.gas_limit);
        assert_eq!(decoded_tx.max_fee_per_gas, tx.max_fee_per_gas);
        assert_eq!(
            decoded_tx.max_priority_fee_per_gas,
            tx.max_priority_fee_per_gas
        );
        assert_eq!(decoded_tx.to, tx.to);
        assert_eq!(decoded_tx.value, tx.value);
        assert_eq!(decoded_tx.input, tx.input);
        assert_eq!(decoded_tx.fee_token_id, tx.fee_token_id);
        assert_eq!(decoded_tx.fee_limit, tx.fee_limit);
        assert_eq!(decoded_tx.reference, Some(reference));
        assert_eq!(decoded_tx.memo, Some(memo.clone()));
        assert_eq!(decoded_sig, signature);

        // Test full EIP-2718 roundtrip: encode_2718 → decode_2718
        // This is the path used by MorphTxEnvelope::decode_2718
        let signed_tx = Signed::new_unhashed(tx.clone(), signature);
        let mut eip2718_buf = Vec::new();
        signed_tx.encode_2718(&mut eip2718_buf);

        // First byte should be txType (0x7F)
        assert_eq!(eip2718_buf[0], MORPH_TX_TYPE_ID);
        // Second byte should be version (0x01) for V1
        assert_eq!(eip2718_buf[1], MORPH_TX_VERSION_1);
        // Third byte should be RLP list prefix
        assert!(
            eip2718_buf[2] >= 0xC0,
            "Third byte should be RLP list prefix, got 0x{:02x}",
            eip2718_buf[2]
        );

        // Decode via decode_2718
        let decoded_signed = Signed::<TxMorph>::decode_2718(&mut eip2718_buf.as_slice())
            .expect("Should decode V1 signed tx via decode_2718");

        assert_eq!(decoded_signed.tx().version, MORPH_TX_VERSION_1);
        assert_eq!(decoded_signed.tx().chain_id, 1);
        assert_eq!(decoded_signed.tx().nonce, 42);
        assert_eq!(decoded_signed.tx().reference, Some(reference));
        assert_eq!(decoded_signed.tx().memo, Some(memo));
        assert!(decoded_signed.tx().is_v1());
    }

    #[test]
    fn test_morph_signed_v0_decode_2718_roundtrip() {
        use alloy_consensus::Signed;
        use alloy_consensus::transaction::RlpEcdsaEncodableTx;
        use alloy_eips::eip2718::Decodable2718;

        let tx = TxMorph {
            chain_id: 1,
            nonce: 10,
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
            authorization_list: Vec::new(),
        };

        let signature = Signature::new(U256::from(1u64), U256::from(2u64), false);

        // Test rlp_encode_signed for V0 (no version byte prefix)
        let mut signed_buf = Vec::new();
        tx.rlp_encode_signed(&signature, &mut signed_buf);

        // First byte should be RLP list prefix (no version byte for V0)
        assert!(
            signed_buf[0] >= 0xC0,
            "First byte should be RLP list prefix for V0, got 0x{:02x}",
            signed_buf[0]
        );

        // Full EIP-2718 roundtrip
        let signed_tx = Signed::new_unhashed(tx.clone(), signature);
        let mut eip2718_buf = Vec::new();
        signed_tx.encode_2718(&mut eip2718_buf);

        // First byte should be txType (0x7F)
        assert_eq!(eip2718_buf[0], MORPH_TX_TYPE_ID);
        // Second byte should be RLP list prefix (NO version byte for V0)
        assert!(
            eip2718_buf[1] >= 0xC0,
            "Second byte should be RLP list prefix for V0, got 0x{:02x}",
            eip2718_buf[1]
        );

        // Decode via decode_2718
        let decoded_signed = Signed::<TxMorph>::decode_2718(&mut eip2718_buf.as_slice())
            .expect("Should decode V0 signed tx via decode_2718");

        assert_eq!(decoded_signed.tx().version, MORPH_TX_VERSION_0);
        assert_eq!(decoded_signed.tx().chain_id, 1);
        assert_eq!(decoded_signed.tx().nonce, 10);
        assert_eq!(decoded_signed.tx().fee_token_id, 1);
        assert_eq!(decoded_signed.tx().fee_limit, U256::from(1000u64));
        assert_eq!(decoded_signed.tx().reference, None);
        assert_eq!(decoded_signed.tx().memo, None);
        assert!(decoded_signed.tx().is_v0());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_tx_morph_serde_defaults_for_legacy_fields() {
        // Legacy V0 JSON that omits version, feeTokenId, and feeLimit.
        let json = r#"{
            "chainId": "0x1",
            "nonce": "0x0",
            "gasLimit": "0x5208",
            "maxFeePerGas": "0x64",
            "maxPriorityFeePerGas": "0x1",
            "to": "0x0000000000000000000000000000000000000002",
            "value": "0x0",
            "accessList": [],
            "input": "0x"
        }"#;

        let tx: TxMorph = serde_json::from_str(json).unwrap();
        assert_eq!(tx.version, MORPH_TX_VERSION_0);
        assert_eq!(tx.fee_token_id, 0);
        assert_eq!(tx.fee_limit, U256::ZERO);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_morph_tx_fields_serde_defaults_for_legacy_fields() {
        // MorphTxFields should also accept JSON with omitted fields.
        let json = r#"{}"#;

        let fields: MorphTxFields = serde_json::from_str(json).unwrap();
        assert_eq!(fields.version, 0);
        assert_eq!(fields.fee_token_id, 0);
        assert_eq!(fields.fee_limit, U256::ZERO);
        assert_eq!(fields.reference, None);
        assert_eq!(fields.memo, None);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_morph_tx_fields_serde_uses_canonical_fee_token_id_key() {
        let fields = MorphTxFields {
            version: 1,
            fee_token_id: 7,
            fee_limit: U256::from(999u64),
            reference: None,
            memo: None,
        };

        let json = serde_json::to_value(fields).unwrap();
        assert_eq!(json.get("feeTokenID"), Some(&serde_json::json!("0x7")));
        assert!(json.get("feeTokenId").is_none());
    }

    #[cfg(feature = "reth-codec")]
    #[test]
    fn test_compact_roundtrip_v1_with_memo() {
        use reth_codecs::Compact;

        let tx = TxMorph {
            chain_id: 2818,
            nonce: 42,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(1_000_000_000_000_000_000u128),
            access_list: AccessList::default(),
            version: 1,
            fee_token_id: 7,
            fee_limit: U256::from(999u64),
            reference: Some(B256::from([0xab; 32])),
            memo: Some(Bytes::from(vec![0xca, 0xfe, 0xba, 0xbe])),
            authorization_list: Vec::new(),
            input: Bytes::from(vec![0x12, 0x34, 0x56]),
        };

        let mut buf = Vec::new();
        tx.to_compact(&mut buf);
        let (decoded, remaining) = TxMorph::from_compact(&buf, buf.len());

        assert!(remaining.is_empty());
        assert_eq!(tx, decoded);
    }

    #[cfg(feature = "reth-codec")]
    #[test]
    fn test_compact_roundtrip_v0_no_memo() {
        use reth_codecs::Compact;

        let tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 50_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Create,
            value: U256::ZERO,
            access_list: AccessList::default(),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::from(500u64),
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            input: Bytes::from(vec![0x60, 0x80, 0x60, 0x40]),
        };

        let mut buf = Vec::new();
        tx.to_compact(&mut buf);
        let (decoded, remaining) = TxMorph::from_compact(&buf, buf.len());

        assert!(remaining.is_empty());
        assert_eq!(tx, decoded);
    }

    // =========================================================================
    // V2 (EIP-7702 authorization list) tests
    // =========================================================================

    use alloy_eips::eip7702::Authorization;

    /// A syntactically valid authorization tuple (the signature is not
    /// recoverable; recovery only matters at execution time).
    fn sample_authorization(nonce: u64) -> SignedAuthorization {
        Authorization {
            chain_id: U256::from(2818),
            address: address!("2222222222222222222222222222222222222222"),
            nonce,
        }
        .into_signed(Signature::new(
            U256::from(0x1111u64),
            U256::from(0x2222u64),
            true,
        ))
    }

    fn sample_v2_tx(fee_token_id: u16) -> TxMorph {
        TxMorph {
            chain_id: 2818,
            nonce: 26,
            gas_limit: 3_000_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(address!("1111111111111111111111111111111111111111")),
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: Bytes::new(),
            version: MORPH_TX_VERSION_2,
            fee_token_id,
            fee_limit: if fee_token_id > 0 {
                U256::from(1_000_000_000_000_000_000u128)
            } else {
                U256::ZERO
            },
            reference: Some(B256::from([0x01; 32])),
            memo: Some(Bytes::from_static(b"invoice-1")),
            authorization_list: vec![sample_authorization(27), sample_authorization(28)],
        }
    }

    #[test]
    fn test_morph_transaction_v2_validate_rules() {
        // Valid V2 with token fee and with ETH fee.
        assert!(sample_v2_tx(1).validate().is_ok());
        assert!(sample_v2_tx(0).validate().is_ok());

        // V2 may carry an empty list (it then behaves like V1).
        let empty = TxMorph {
            authorization_list: Vec::new(),
            ..sample_v2_tx(0)
        };
        assert!(empty.validate().is_ok());
        assert!(!empty.has_authorizations());

        // With authorizations V2 cannot create a contract (same rule as EIP-7702
        // SetCode); without them CREATE is allowed exactly like V1.
        let create = TxMorph {
            to: TxKind::Create,
            input: Bytes::from_static(&[0x60, 0x80]),
            ..sample_v2_tx(0)
        };
        assert_eq!(
            create.validate().unwrap_err(),
            "MorphTx with an authorization list cannot create a contract"
        );
        let create_without_authorizations = TxMorph {
            authorization_list: Vec::new(),
            ..create
        };
        assert!(create_without_authorizations.validate().is_ok());

        // V1 fee rule still applies to V2.
        let fee_limit_without_token = TxMorph {
            fee_token_id: 0,
            fee_limit: U256::from(1u64),
            ..sample_v2_tx(0)
        };
        assert_eq!(
            fee_limit_without_token.validate().unwrap_err(),
            "version 2 MorphTx cannot have FeeLimit when FeeTokenID is 0"
        );

        // V0 / V1 must not carry a list.
        let v1_with_list = TxMorph {
            version: MORPH_TX_VERSION_1,
            ..sample_v2_tx(0)
        };
        assert_eq!(
            v1_with_list.validate().unwrap_err(),
            "version 1 MorphTx does not support authorization list"
        );
        let v0_with_list = TxMorph {
            version: MORPH_TX_VERSION_0,
            fee_token_id: 1,
            reference: None,
            memo: None,
            ..sample_v2_tx(1)
        };
        assert_eq!(
            v0_with_list.validate().unwrap_err(),
            "version 0 MorphTx does not support authorization list"
        );

        // An empty list on V1 is the normal state.
        let v1_empty_list = TxMorph {
            version: MORPH_TX_VERSION_1,
            authorization_list: Vec::new(),
            ..sample_v2_tx(0)
        };
        assert!(v1_empty_list.validate().is_ok());
    }

    /// V1 is the baseline for anything built from user intent; only a
    /// non-empty authorization list raises it to V2, and a hand-filled version
    /// is overwritten.
    #[test]
    fn inferred_version_is_v1_unless_authorizations_are_present() {
        assert_eq!(TxMorph::inferred_version(false), MORPH_TX_VERSION_1);
        assert_eq!(TxMorph::inferred_version(true), MORPH_TX_VERSION_2);

        let with_list = TxMorph {
            version: MORPH_TX_VERSION_0,
            ..sample_v2_tx(1)
        }
        .with_inferred_version();
        assert_eq!(with_list.version, MORPH_TX_VERSION_2);
        assert!(with_list.validate().is_ok());

        let without_list = TxMorph {
            version: MORPH_TX_VERSION_2,
            authorization_list: Vec::new(),
            ..sample_v2_tx(1)
        }
        .with_inferred_version();
        assert_eq!(without_list.version, MORPH_TX_VERSION_1);
        assert!(without_list.validate().is_ok());
    }

    #[test]
    fn test_morph_transaction_authorization_list_accessor_is_version_gated() {
        let v2 = sample_v2_tx(0);
        assert_eq!(
            Transaction::authorization_list(&v2).map(<[SignedAuthorization]>::len),
            Some(2)
        );

        // Even if an (invalid) V1 value carries the raw field, the trait view is None,
        // so pool authority tracking and the EVM never see it.
        let v1 = TxMorph {
            version: MORPH_TX_VERSION_1,
            ..sample_v2_tx(0)
        };
        assert!(Transaction::authorization_list(&v1).is_none());
        assert!(v1.has_authorizations());

        // A V2 with an empty list has nothing to apply: the trait view is None
        // (like a plain V1), so nothing downstream treats it as a 7702 carrier.
        let v2_empty = TxMorph {
            authorization_list: Vec::new(),
            ..sample_v2_tx(0)
        };
        assert!(Transaction::authorization_list(&v2_empty).is_none());
        assert!(!v2_empty.has_authorizations());
    }

    #[test]
    fn test_morph_transaction_rlp_roundtrip_v2() {
        let tx = sample_v2_tx(1);

        let mut buf = Vec::new();
        tx.encode(&mut buf);
        assert_eq!(buf[0], MORPH_TX_VERSION_2, "V2 wire prefix byte");
        assert!(buf[1] >= 0xC0, "RLP list header follows the version byte");
        assert_eq!(buf.len(), tx.length());

        let decoded = TxMorph::decode(&mut buf.as_slice()).expect("Should decode V2");
        assert_eq!(decoded, tx);
        assert!(decoded.is_v2());

        // decode_fields (the RlpEcdsaDecodableTx fallback) takes the same route.
        let via_fields = TxMorph::decode_fields(&mut buf.as_slice()).expect("decode_fields V2");
        assert_eq!(via_fields, tx);
    }

    #[test]
    fn test_morph_signed_v2_decode_2718_roundtrip() {
        use alloy_consensus::Signed;
        use alloy_consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
        use alloy_eips::eip2718::Decodable2718;

        let tx = sample_v2_tx(1);
        let signature = Signature::new(U256::from(1u64), U256::from(2u64), false);

        let mut signed_buf = Vec::new();
        tx.rlp_encode_signed(&signature, &mut signed_buf);
        assert_eq!(signed_buf[0], MORPH_TX_VERSION_2);
        assert_eq!(
            signed_buf.len(),
            tx.rlp_encoded_length_with_signature(&signature)
        );

        let (decoded_tx, decoded_sig) =
            TxMorph::rlp_decode_with_signature(&mut signed_buf.as_slice())
                .expect("Should decode V2 signed tx");
        assert_eq!(decoded_tx, tx);
        assert_eq!(decoded_sig, signature);

        // Full EIP-2718 roundtrip: 0x7f || 0x02 || rlp([...])
        let signed_tx = Signed::new_unhashed(tx.clone(), signature);
        let mut eip2718_buf = Vec::new();
        signed_tx.encode_2718(&mut eip2718_buf);
        assert_eq!(eip2718_buf[0], MORPH_TX_TYPE_ID);
        assert_eq!(eip2718_buf[1], MORPH_TX_VERSION_2);
        assert_eq!(eip2718_buf.len(), signed_tx.encode_2718_len());

        let decoded_signed = Signed::<TxMorph>::decode_2718(&mut eip2718_buf.as_slice())
            .expect("Should decode V2 signed tx via decode_2718");
        assert_eq!(decoded_signed.tx(), &tx);
        assert_eq!(decoded_signed.hash(), signed_tx.hash());
    }

    /// Locks the V2 wire layout: the authorization list sits between `memo`
    /// and the transaction signature, and the signing payload carries the
    /// version inside the RLP list (no `0x02` prefix) followed by the list.
    #[test]
    fn test_morph_transaction_v2_wire_and_sig_hash_layout() {
        use alloy_consensus::transaction::RlpEcdsaEncodableTx;

        let tx = sample_v2_tx(1);
        let signature = Signature::new(U256::from(1u64), U256::from(2u64), false);
        let auth_list = tx.authorization_list.clone();

        // Common prefix shared by the wire and signing encodings.
        let mut common = Vec::new();
        tx.chain_id.encode(&mut common);
        tx.nonce.encode(&mut common);
        tx.max_priority_fee_per_gas.encode(&mut common);
        tx.max_fee_per_gas.encode(&mut common);
        tx.gas_limit.encode(&mut common);
        tx.to.encode(&mut common);
        tx.value.encode(&mut common);
        tx.input.encode(&mut common);
        tx.access_list.encode(&mut common);
        tx.fee_token_id.encode(&mut common);
        tx.fee_limit.encode(&mut common);

        let mut tail = Vec::new();
        tx.reference.unwrap().0.encode(&mut tail);
        tx.memo.clone().unwrap().encode(&mut tail);
        auth_list.encode(&mut tail);

        // Wire: 0x02 || rlp([common..., reference, memo, authorizationList, yParity, r, s])
        let mut wire_payload = common.clone();
        wire_payload.extend_from_slice(&tail);
        signature.write_rlp_vrs(&mut wire_payload, signature.v());
        let mut expected_wire = vec![MORPH_TX_VERSION_2];
        Header {
            list: true,
            payload_length: wire_payload.len(),
        }
        .encode(&mut expected_wire);
        expected_wire.extend_from_slice(&wire_payload);

        let mut actual_wire = Vec::new();
        tx.rlp_encode_signed(&signature, &mut actual_wire);
        assert_eq!(actual_wire, expected_wire, "V2 wire layout");

        // Signing: 0x7f || rlp([common..., version, reference, memo, authorizationList])
        let mut sig_payload = common;
        tx.version.encode(&mut sig_payload);
        sig_payload.extend_from_slice(&tail);
        let mut expected_sig_preimage = vec![MORPH_TX_TYPE_ID];
        Header {
            list: true,
            payload_length: sig_payload.len(),
        }
        .encode(&mut expected_sig_preimage);
        expected_sig_preimage.extend_from_slice(&sig_payload);

        let mut actual_sig_preimage = Vec::new();
        tx.encode_for_signing(&mut actual_sig_preimage);
        assert_eq!(
            actual_sig_preimage, expected_sig_preimage,
            "V2 sigHash layout"
        );
        assert_eq!(tx.signature_hash(), keccak256(&expected_sig_preimage));
        assert_eq!(tx.payload_len_for_signature(), expected_sig_preimage.len());
    }

    #[test]
    fn test_morph_transaction_v2_signature_hash_covers_authorization_list() {
        let tx = sample_v2_tx(0);
        let other_list = TxMorph {
            authorization_list: vec![sample_authorization(99)],
            ..tx.clone()
        };
        assert_ne!(tx.signature_hash(), other_list.signature_hash());

        // Same base fields as V1: the version and the list both move the hash.
        let v1 = TxMorph {
            version: MORPH_TX_VERSION_1,
            authorization_list: Vec::new(),
            ..tx.clone()
        };
        assert_ne!(tx.signature_hash(), v1.signature_hash());
    }

    /// V1 payloads have a fixed element count: an appended authorization list
    /// (i.e. V2 fields behind a V1 prefix) must be rejected, not silently
    /// ignored, on both the signed and the unsigned decode paths.
    #[test]
    fn test_v1_wire_with_trailing_authorization_list_rejected() {
        use alloy_consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};

        let tx = sample_v2_tx(0);

        // Unsigned path: rewrite the version byte so a V1 decoder sees 14 fields.
        let mut unsigned = Vec::new();
        tx.encode(&mut unsigned);
        unsigned[0] = MORPH_TX_VERSION_1;
        let err = TxMorph::decode(&mut unsigned.as_slice()).unwrap_err();
        assert!(
            matches!(err, alloy_rlp::Error::ListLengthMismatch { .. }),
            "unsigned V1 decode must reject trailing elements, got {err:?}"
        );
        let err = TxMorph::decode_fields(&mut unsigned.as_slice()).unwrap_err();
        assert!(
            matches!(err, alloy_rlp::Error::ListLengthMismatch { .. }),
            "V1 decode_fields must reject trailing elements, got {err:?}"
        );

        // Signed path: the V1 decoder reads the list header where yParity should be.
        let signature = Signature::new(U256::from(1u64), U256::from(2u64), false);
        let mut signed = Vec::new();
        tx.rlp_encode_signed(&signature, &mut signed);
        signed[0] = MORPH_TX_VERSION_1;
        let err = TxMorph::rlp_decode_with_signature(&mut signed.as_slice()).unwrap_err();
        assert!(
            !err.to_string().contains("unsupported"),
            "expected an RLP-level error, got {err}"
        );
    }

    /// `decode_fields` (behind `rlp_decode_fields`) consumes the list exactly
    /// for every version, like `Decodable::decode`: a surplus element is
    /// rejected rather than left unread.
    #[test]
    fn test_decode_fields_rejects_surplus_list_elements() {
        fn with_extra_element(mut list: &[u8]) -> Vec<u8> {
            let header = Header::decode(&mut list).unwrap();
            assert!(header.list);
            let mut payload = list[..header.payload_length].to_vec();
            payload.push(alloy_rlp::EMPTY_STRING_CODE);
            let mut out = Vec::new();
            Header {
                list: true,
                payload_length: payload.len(),
            }
            .encode(&mut out);
            out.extend_from_slice(&payload);
            out
        }

        let v2 = sample_v2_tx(1);
        let v0 = TxMorph {
            version: MORPH_TX_VERSION_0,
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            ..v2.clone()
        };
        for tx in [v0, v2] {
            let mut encoded = Vec::new();
            tx.encode(&mut encoded);
            // V1+ carries the version byte in front of the list.
            let prefix_len = usize::from(!tx.is_v0());
            let mut surplus = encoded[..prefix_len].to_vec();
            surplus.extend(with_extra_element(&encoded[prefix_len..]));

            for result in [
                TxMorph::decode_fields(&mut surplus.as_slice()),
                TxMorph::decode(&mut surplus.as_slice()),
            ] {
                assert!(
                    matches!(result, Err(alloy_rlp::Error::ListLengthMismatch { .. })),
                    "version {}: surplus element must be rejected, got {result:?}",
                    tx.version
                );
            }
        }
    }

    /// A V2 with an empty list is valid and encodes as the V1 field list plus
    /// one empty RLP list (`0xc0`): same payload as V1, version byte `0x02`.
    #[test]
    fn test_v2_wire_with_empty_authorization_list_is_v1_layout_plus_empty_list() {
        let v2 = TxMorph {
            authorization_list: Vec::new(),
            ..sample_v2_tx(0)
        };
        let v1 = TxMorph {
            version: MORPH_TX_VERSION_1,
            ..v2.clone()
        };
        assert!(v2.validate().is_ok());

        let mut v2_buf = Vec::new();
        v2.encode(&mut v2_buf);
        let mut v1_buf = Vec::new();
        v1.encode(&mut v1_buf);
        assert_eq!(v2_buf[0], MORPH_TX_VERSION_2);
        assert_eq!(v1_buf[0], MORPH_TX_VERSION_1);

        // Strip the version byte and the list header from both encodings.
        let mut v2_payload = &v2_buf[1..];
        let v2_header = Header::decode(&mut v2_payload).unwrap();
        let mut v1_payload = &v1_buf[1..];
        let v1_header = Header::decode(&mut v1_payload).unwrap();
        assert!(v2_header.list && v1_header.list);
        assert_eq!(v2_header.payload_length, v1_header.payload_length + 1);
        assert_eq!(
            v2_payload,
            [v1_payload, &[alloy_rlp::EMPTY_LIST_CODE][..]].concat(),
            "V2 with an empty list = V1 fields + 0xc0"
        );

        // Round trip: the empty list decodes as empty and the value is unchanged.
        let decoded = TxMorph::decode(&mut v2_buf.as_slice()).expect("V2 with empty list decodes");
        assert_eq!(decoded, v2);
        assert!(decoded.authorization_list.is_empty());
        assert!(decoded.validate().is_ok());

        // The version still moves the signature hash even though the list is empty.
        assert_ne!(v2.signature_hash(), v1.signature_hash());
    }

    #[test]
    fn test_morph_transaction_rejects_unknown_version_byte() {
        let mut buf: &[u8] = &[0x03, 0xc0];
        let err = TxMorph::decode(&mut buf).unwrap_err();
        assert!(err.to_string().contains("unsupported morph tx version"));
    }

    #[test]
    fn test_morph_transaction_size_counts_authorizations() {
        let v2 = sample_v2_tx(0);
        let v1 = TxMorph {
            version: MORPH_TX_VERSION_1,
            authorization_list: Vec::new(),
            ..v2.clone()
        };
        assert!(v2.size() > v1.size());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_tx_morph_serde_v2_outputs_authorization_list() {
        let v2 = sample_v2_tx(1);
        let json = serde_json::to_value(&v2).unwrap();
        assert_eq!(json["version"], serde_json::json!("0x2"));
        let list = json["authorizationList"]
            .as_array()
            .expect("V2 JSON carries authorizationList");
        assert_eq!(list.len(), 2);
        for key in ["chainId", "address", "nonce", "yParity", "r", "s"] {
            assert!(
                list[0].get(key).is_some(),
                "authorization tuple must have `{key}` (same shape as 0x04)"
            );
        }

        let roundtrip: TxMorph = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, v2);

        // V0 / V1 never emit the key, and the hand-written serializer stays in
        // step with the derived deserializer for every version.
        for version in [MORPH_TX_VERSION_0, MORPH_TX_VERSION_1] {
            let tx = TxMorph {
                version,
                authorization_list: Vec::new(),
                ..sample_v2_tx(1)
            };
            let json = serde_json::to_value(&tx).unwrap();
            assert!(json.get("authorizationList").is_none());
            assert_eq!(serde_json::from_value::<TxMorph>(json).unwrap(), tx);
        }

        // A V2 with an empty list still emits the key, as `[]` (go-ethereum
        // does the same), and `[]`, an absent key and `null` all decode back
        // to the same (empty) value.
        let v2_empty = TxMorph {
            authorization_list: Vec::new(),
            ..sample_v2_tx(1)
        };
        let mut json = serde_json::to_value(&v2_empty).unwrap();
        assert_eq!(json["version"], serde_json::json!("0x2"));
        assert_eq!(json["authorizationList"], serde_json::json!([]));
        let empty: TxMorph = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(empty, v2_empty);
        json.as_object_mut().unwrap().remove("authorizationList");
        let absent: TxMorph = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(absent, v2_empty);
        json["authorizationList"] = serde_json::Value::Null;
        let null: TxMorph = serde_json::from_value(json).unwrap();
        assert_eq!(null, v2_empty);
    }

    #[cfg(feature = "reth-codec")]
    #[test]
    fn test_compact_roundtrip_v2_with_authorization_list() {
        use reth_codecs::Compact;

        let tx = sample_v2_tx(1);
        let mut buf = Vec::new();
        tx.to_compact(&mut buf);
        let (decoded, remaining) = TxMorph::from_compact(&buf, buf.len());

        assert!(remaining.is_empty());
        assert_eq!(tx, decoded);
    }

    /// A V2 with an empty list stores the list as absent (same bytes as a V1
    /// row apart from the version) and decodes back to an empty list.
    #[cfg(feature = "reth-codec")]
    #[test]
    fn test_compact_roundtrip_v2_with_empty_authorization_list() {
        use reth_codecs::Compact;

        let v2_empty = TxMorph {
            authorization_list: Vec::new(),
            ..sample_v2_tx(1)
        };
        let mut buf = Vec::new();
        v2_empty.to_compact(&mut buf);
        let (decoded, remaining) = TxMorph::from_compact(&buf, buf.len());
        assert!(remaining.is_empty());
        assert_eq!(decoded, v2_empty);
        assert!(decoded.authorization_list.is_empty());

        let v1 = TxMorph {
            version: MORPH_TX_VERSION_1,
            ..v2_empty
        };
        let mut v1_buf = Vec::new();
        v1.to_compact(&mut v1_buf);
        assert_eq!(
            buf.len(),
            v1_buf.len(),
            "an empty list adds no storage bytes"
        );
    }

    /// Storage layout lock: rows written before the V2 field existed must keep
    /// decoding byte-for-byte, and pre-V2 transactions must still encode to the
    /// exact same bytes (the new presence bit only occupies previously unused
    /// flag padding). Vectors were produced by the pre-V2 `Compact` impl.
    #[cfg(feature = "reth-codec")]
    #[test]
    fn test_compact_decodes_pre_v2_bytes() {
        use alloy_primitives::hex;
        use reth_codecs::Compact;

        let v1 = TxMorph {
            chain_id: 2818,
            nonce: 42,
            gas_limit: 21_000,
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(1_000_000_000_000_000_000u128),
            access_list: AccessList::default(),
            version: 1,
            fee_token_id: 7,
            fee_limit: U256::from(999u64),
            reference: Some(B256::from([0xab; 32])),
            memo: Some(Bytes::from(vec![0xca, 0xfe, 0xba, 0xbe])),
            authorization_list: Vec::new(),
            input: Bytes::from(vec![0x12, 0x34, 0x56]),
        };
        let v1_bytes = hex::decode(
            "1252482442080b022a5208174876e8007735940000000000000000000000000000000000000000020de0b6b3a764000000010703e7abababababababababababababababababababababababababababababababab04cafebabe123456",
        )
        .unwrap();

        let v0 = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 50_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Create,
            value: U256::ZERO,
            access_list: AccessList::default(),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::from(500u64),
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            input: Bytes::from(vec![0x60, 0x80, 0x60, 0x40]),
        };
        let v0_bytes =
            hex::decode("0253080042000b020186a00ba43b74003b9aca00000101f40060806040").unwrap();

        for (name, tx, bytes) in [("v1", v1, v1_bytes), ("v0", v0, v0_bytes)] {
            let (decoded, remaining) = TxMorph::from_compact(&bytes, bytes.len());
            assert!(remaining.is_empty(), "{name}: pre-V2 bytes fully consumed");
            assert_eq!(decoded, tx, "{name}: pre-V2 bytes decode unchanged");

            let mut reencoded = Vec::new();
            tx.to_compact(&mut reencoded);
            assert_eq!(
                reencoded, bytes,
                "{name}: pre-V2 rows re-encode identically"
            );
        }
    }
}
