//! Morph transaction environment.
//!
//! This module defines the Morph-specific transaction environment with token fee support.

use alloy_consensus::{
    EthereumTxEnvelope, SignableTransaction, Transaction as AlloyTransaction, TxEip4844,
};
use alloy_eips::eip2718::Encodable2718;
use alloy_eips::eip2930::AccessList;
use alloy_eips::eip7702::RecoveredAuthority;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use alloy_rlp::Decodable;
use morph_primitives::{L1_TX_TYPE_ID, MORPH_TX_TYPE_ID, MorphTxEnvelope, TxMorph};
use reth_evm::{FromRecoveredTx, FromTxWithEncoded, ToTxEnv, TransactionEnv};
use revm::context::{Transaction, TxEnv};
use revm::context_interface::transaction::{
    AccessListItem, RecoveredAuthorization, SignedAuthorization,
};
use std::ops::{Deref, DerefMut};

/// Re-export Either for authorization list
use alloy_consensus::transaction::Either;

/// Morph transaction environment with token fee support.
///
/// This wraps the standard [`TxEnv`] and adds Morph-specific fields for:
/// - L1 message detection (tx_type 0x7E)
/// - TxMorph with token-based gas payment (tx_type 0x7F)
/// - RLP encoded transaction bytes for L1 data fee calculation
/// - Version, reference, and memo fields for extended MorphTx functionality
///
/// Reference: <https://github.com/morph-l2/go-ethereum/pull/282>
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MorphTxEnv {
    /// Inner transaction environment.
    pub inner: TxEnv,
    /// RLP encoded transaction bytes.
    /// Used only for L1 data fee calculation.
    pub rlp_bytes: Option<Bytes>,
    /// Version of the Morph transaction format.
    pub version: Option<u8>,
    /// Token ID for fee payment (only for TxMorph type 0x7F).
    /// 0 means ETH payment, > 0 means ERC20 token payment.
    pub fee_token_id: Option<u16>,
    /// Maximum amount of tokens the sender is willing to pay as fee.
    pub fee_limit: Option<U256>,
    /// Reference key for the transaction.
    /// Used for indexing and looking up transactions by external systems.
    pub reference: Option<alloy_primitives::B256>,
    /// Memo field for arbitrary data.
    pub memo: Option<Bytes>,
}

impl MorphTxEnv {
    /// Create a new Morph transaction environment from a standard TxEnv.
    pub fn new(inner: TxEnv) -> Self {
        Self {
            inner,
            rlp_bytes: None,
            version: None,
            fee_token_id: None,
            fee_limit: None,
            reference: None,
            memo: None,
        }
    }

    /// Create a new Morph transaction environment with RLP bytes.
    pub fn with_rlp_bytes(mut self, rlp_bytes: Bytes) -> Self {
        self.rlp_bytes = Some(rlp_bytes);
        self
    }

    /// Set the version.
    pub fn with_version(mut self, version: u8) -> Self {
        self.version = Some(version);
        self
    }

    /// Set the fee token ID.
    pub fn with_fee_token_id(mut self, fee_token_id: u16) -> Self {
        self.fee_token_id = Some(fee_token_id);
        self
    }

    /// Set the fee limit.
    pub fn with_fee_limit(mut self, fee_limit: U256) -> Self {
        self.fee_limit = Some(fee_limit);
        self
    }

    /// Set the reference.
    pub fn with_reference(mut self, reference: alloy_primitives::B256) -> Self {
        self.reference = Some(reference);
        self
    }

    /// Set the memo.
    pub fn with_memo(mut self, memo: Bytes) -> Self {
        self.memo = Some(memo);
        self
    }

    /// Encodes this tx env into EIP-2718 bytes for L1 fee accounting.
    ///
    /// This is used by simulation paths (`eth_call`, `eth_estimateGas`) where
    /// we have tx env fields but no pre-encoded transaction bytes.
    pub fn encode_for_l1_fee(&self, fallback_chain_id: u64) -> Bytes {
        // Signature validity is irrelevant for fee sizing, but encoded length matters.
        let placeholder_signature = Signature::new(Default::default(), Default::default(), false);

        match self.build_morph_tx_for_l1_fee(fallback_chain_id) {
            Some(morph_tx) => {
                let signed = morph_tx.into_signed(placeholder_signature);
                MorphTxEnvelope::Morph(signed).rlp()
            }
            None => self
                .build_ethereum_envelope_for_l1_fee(fallback_chain_id, placeholder_signature)
                .rlp(),
        }
    }

    fn build_morph_tx_for_l1_fee(&self, fallback_chain_id: u64) -> Option<TxMorph> {
        let fee_token_id = self.fee_token_id.unwrap_or_default();
        let has_fee_token = fee_token_id > 0;
        let has_reference = self.reference.is_some();
        let has_memo = self.memo.as_ref().is_some_and(|m| !m.is_empty());

        if !has_fee_token && !has_reference && !has_memo {
            return None;
        }

        Some(TxMorph {
            chain_id: self.chain_id().unwrap_or(fallback_chain_id),
            nonce: self.inner.nonce,
            gas_limit: self.gas_limit() as u128,
            max_fee_per_gas: self.max_fee_per_gas(),
            max_priority_fee_per_gas: self.max_priority_fee_per_gas().unwrap_or_default(),
            to: self.kind(),
            value: self.value(),
            access_list: self.access_list.clone(),
            input: self.input().clone(),
            fee_token_id,
            fee_limit: self.fee_limit.unwrap_or_default(),
            version: morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_1,
            reference: self.reference,
            memo: self.memo.clone(),
        })
    }

    fn build_ethereum_envelope_for_l1_fee(
        &self,
        fallback_chain_id: u64,
        signature: Signature,
    ) -> MorphTxEnvelope {
        let chain_id = self.chain_id().unwrap_or(fallback_chain_id);
        let has_dynamic_fees = self.max_priority_fee_per_gas().is_some();
        let has_access_list = !self.access_list.is_empty();

        if !has_dynamic_fees && !has_access_list {
            MorphTxEnvelope::Legacy(
                alloy_consensus::TxLegacy {
                    chain_id: Some(chain_id),
                    nonce: self.inner.nonce,
                    gas_price: self.gas_price(),
                    gas_limit: self.gas_limit(),
                    to: self.kind(),
                    value: self.value(),
                    input: self.input().clone(),
                }
                .into_signed(signature),
            )
        } else if has_access_list && !has_dynamic_fees {
            MorphTxEnvelope::Eip2930(
                alloy_consensus::TxEip2930 {
                    chain_id,
                    nonce: self.inner.nonce,
                    gas_price: self.gas_price(),
                    gas_limit: self.gas_limit(),
                    to: self.kind(),
                    value: self.value(),
                    access_list: self.access_list.clone(),
                    input: self.input().clone(),
                }
                .into_signed(signature),
            )
        } else {
            MorphTxEnvelope::Eip1559(
                alloy_consensus::TxEip1559 {
                    chain_id,
                    nonce: self.inner.nonce,
                    gas_limit: self.gas_limit(),
                    max_fee_per_gas: self.max_fee_per_gas(),
                    max_priority_fee_per_gas: self.max_priority_fee_per_gas().unwrap_or_default(),
                    to: self.kind(),
                    value: self.value(),
                    access_list: self.access_list.clone(),
                    input: self.input().clone(),
                }
                .into_signed(signature),
            )
        }
    }

    /// Create a new Morph transaction environment from a recovered transaction.
    ///
    /// This method:
    /// - Converts the transaction to `TxEnv`
    /// - Extracts the RLP-encoded transaction bytes for L1 data fee calculation
    /// - Extracts fee_token_id for TxMorph (type 0x7F)
    pub fn from_recovered_tx(tx: &MorphTxEnvelope, signer: Address) -> Self {
        // Encode the transaction to RLP bytes for L1 data fee calculation
        let rlp_bytes = tx.encoded_2718();
        Self::from_tx_with_rlp_bytes(tx, signer, Bytes::from(rlp_bytes))
    }

    /// Create a new Morph transaction environment from a transaction with pre-encoded RLP bytes.
    ///
    /// This is the core implementation used by both `from_recovered_tx` and `FromTxWithEncoded`.
    fn from_tx_with_rlp_bytes(tx: &MorphTxEnvelope, signer: Address, rlp_bytes: Bytes) -> Self {
        let tx_type: u8 = tx.tx_type().into();

        // Extract MorphTx fields for TxMorph (type 0x7F)
        let morph_tx_info = if tx_type == MORPH_TX_TYPE_ID {
            extract_morph_tx_fields_from_rlp(&rlp_bytes)
        } else {
            None
        };

        // Build TxEnv from the transaction
        let inner = TxEnv {
            tx_type,
            caller: signer,
            gas_limit: AlloyTransaction::gas_limit(tx),
            gas_price: tx.effective_gas_price(None),
            kind: AlloyTransaction::kind(tx),
            value: AlloyTransaction::value(tx),
            data: AlloyTransaction::input(tx).clone(),
            nonce: AlloyTransaction::nonce(tx),
            chain_id: AlloyTransaction::chain_id(tx),
            access_list: tx.access_list().cloned().unwrap_or_default(),
            gas_priority_fee: AlloyTransaction::max_priority_fee_per_gas(tx),
            blob_hashes: tx
                .blob_versioned_hashes()
                .map(|h| h.to_vec())
                .unwrap_or_default(),
            max_fee_per_blob_gas: AlloyTransaction::max_fee_per_blob_gas(tx).unwrap_or(0),
            authorization_list: tx
                .authorization_list()
                .unwrap_or_default()
                .iter()
                .map(|auth| {
                    let authority = auth
                        .recover_authority()
                        .map_or(RecoveredAuthority::Invalid, RecoveredAuthority::Valid);
                    Either::Right(RecoveredAuthorization::new_unchecked(
                        auth.inner().clone(),
                        authority,
                    ))
                })
                .collect(),
        };

        // Use builder pattern to set Morph-specific fields
        let mut env = Self::new(inner).with_rlp_bytes(rlp_bytes);
        if let Some(info) = morph_tx_info {
            env = env.with_version(info.version);
            env = env.with_fee_token_id(info.fee_token_id);
            env = env.with_fee_limit(info.fee_limit);
            if let Some(reference) = info.reference {
                env = env.with_reference(reference);
            }
            if let Some(memo) = info.memo
                && !memo.is_empty()
            {
                env = env.with_memo(memo);
            }
        }
        env
    }
}

/// Extracted MorphTx fields from RLP-encoded bytes.
struct DecodedMorphTxFields {
    version: u8,
    fee_token_id: u16,
    fee_limit: U256,
    reference: Option<alloy_primitives::B256>,
    memo: Option<Bytes>,
}

/// Extract all MorphTx fields from RLP-encoded TxMorph bytes.
///
/// The bytes should be EIP-2718 encoded (type byte + RLP payload).
/// Returns None if decoding fails.
fn extract_morph_tx_fields_from_rlp(rlp_bytes: &Bytes) -> Option<DecodedMorphTxFields> {
    if rlp_bytes.is_empty() {
        return None;
    }

    // Skip the type byte (0x7F) and decode the TxMorph
    let payload = &rlp_bytes[1..];
    TxMorph::decode(&mut &payload[..])
        .map(|tx| DecodedMorphTxFields {
            version: tx.version,
            fee_token_id: tx.fee_token_id,
            fee_limit: tx.fee_limit,
            reference: tx.reference,
            memo: tx.memo,
        })
        .ok()
}

impl Deref for MorphTxEnv {
    type Target = TxEnv;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for MorphTxEnv {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl From<TxEnv> for MorphTxEnv {
    fn from(inner: TxEnv) -> Self {
        Self::new(inner)
    }
}

impl From<MorphTxEnv> for TxEnv {
    fn from(morph_tx: MorphTxEnv) -> Self {
        morph_tx.inner
    }
}

// Implement ToTxEnv for MorphTxEnv (identity conversion)
impl ToTxEnv<Self> for MorphTxEnv {
    fn to_tx_env(&self) -> Self {
        self.clone()
    }
}

// Implement FromRecoveredTx for MorphTxEnv
impl FromRecoveredTx<MorphTxEnvelope> for MorphTxEnv {
    fn from_recovered_tx(tx: &MorphTxEnvelope, sender: Address) -> Self {
        Self::from_recovered_tx(tx, sender)
    }
}

impl FromRecoveredTx<EthereumTxEnvelope<TxEip4844>> for MorphTxEnv {
    fn from_recovered_tx(tx: &EthereumTxEnvelope<TxEip4844>, sender: Address) -> Self {
        TxEnv::from_recovered_tx(tx, sender).into()
    }
}

impl FromTxWithEncoded<EthereumTxEnvelope<TxEip4844>> for MorphTxEnv {
    fn from_encoded_tx(
        tx: &EthereumTxEnvelope<TxEip4844>,
        sender: Address,
        _encoded: Bytes,
    ) -> Self {
        <Self as FromRecoveredTx<EthereumTxEnvelope<TxEip4844>>>::from_recovered_tx(tx, sender)
    }
}

// Implement FromTxWithEncoded for MorphTxEnv
impl FromTxWithEncoded<MorphTxEnvelope> for MorphTxEnv {
    fn from_encoded_tx(tx: &MorphTxEnvelope, sender: Address, encoded: Bytes) -> Self {
        Self::from_tx_with_rlp_bytes(tx, sender, encoded)
    }
}

// Implement TransactionEnv for MorphTxEnv
impl TransactionEnv for MorphTxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.inner.gas_limit = gas_limit;
    }

    fn nonce(&self) -> u64 {
        self.inner.nonce
    }

    fn set_nonce(&mut self, nonce: u64) {
        self.inner.nonce = nonce;
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        self.inner.access_list = access_list;
    }
}

// Implement the Transaction trait for MorphTxEnv by delegating to inner TxEnv
impl Transaction for MorphTxEnv {
    type AccessListItem<'a>
        = &'a AccessListItem
    where
        Self: 'a;
    type Authorization<'a>
        = &'a Either<SignedAuthorization, RecoveredAuthorization>
    where
        Self: 'a;

    #[inline]
    fn tx_type(&self) -> u8 {
        self.inner.tx_type
    }

    #[inline]
    fn kind(&self) -> TxKind {
        self.inner.kind
    }

    #[inline]
    fn caller(&self) -> Address {
        self.inner.caller
    }

    #[inline]
    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit
    }

    #[inline]
    fn gas_price(&self) -> u128 {
        self.inner.gas_price
    }

    #[inline]
    fn value(&self) -> U256 {
        self.inner.value
    }

    #[inline]
    fn nonce(&self) -> u64 {
        self.inner.nonce
    }

    #[inline]
    fn chain_id(&self) -> Option<u64> {
        self.inner.chain_id
    }

    #[inline]
    fn access_list(&self) -> Option<impl Iterator<Item = Self::AccessListItem<'_>>> {
        Some(self.inner.access_list.0.iter())
    }

    #[inline]
    fn max_fee_per_gas(&self) -> u128 {
        self.inner.gas_price
    }

    #[inline]
    fn max_fee_per_blob_gas(&self) -> u128 {
        self.inner.max_fee_per_blob_gas
    }

    #[inline]
    fn authorization_list_len(&self) -> usize {
        self.inner.authorization_list.len()
    }

    #[inline]
    fn authorization_list(&self) -> impl Iterator<Item = Self::Authorization<'_>> {
        self.inner.authorization_list.iter()
    }

    #[inline]
    fn input(&self) -> &Bytes {
        &self.inner.data
    }

    #[inline]
    fn blob_versioned_hashes(&self) -> &[B256] {
        &self.inner.blob_hashes
    }

    #[inline]
    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.inner.gas_priority_fee
    }
}

/// Extension trait for transaction types to support Morph-specific functionality.
pub trait MorphTxExt {
    /// Returns whether this transaction is an L1 message transaction (type 0x7E).
    fn is_l1_msg(&self) -> bool;

    /// Returns whether this transaction is a TxMorph (type 0x7F).
    /// TxMorph supports ERC20 token-based gas payment.
    fn is_morph_tx(&self) -> bool;

    /// Returns whether this transaction uses token-based gas payment.
    fn uses_token_fee(&self) -> bool {
        false
    }

    /// Returns whether this transaction has a reference.
    fn has_reference(&self) -> bool {
        false
    }
}

impl MorphTxExt for MorphTxEnv {
    #[inline]
    fn is_l1_msg(&self) -> bool {
        self.inner.tx_type == L1_TX_TYPE_ID
    }

    #[inline]
    fn is_morph_tx(&self) -> bool {
        self.inner.tx_type == MORPH_TX_TYPE_ID
    }

    #[inline]
    fn uses_token_fee(&self) -> bool {
        self.fee_token_id.is_some_and(|id| id > 0)
    }

    #[inline]
    fn has_reference(&self) -> bool {
        self.reference.is_some()
    }
}

impl MorphTxExt for TxEnv {
    #[inline]
    fn is_l1_msg(&self) -> bool {
        self.tx_type == L1_TX_TYPE_ID
    }

    #[inline]
    fn is_morph_tx(&self) -> bool {
        self.tx_type == MORPH_TX_TYPE_ID
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l1_msg_detection() {
        let mut tx = MorphTxEnv::default();
        tx.inner.tx_type = L1_TX_TYPE_ID;
        assert!(tx.is_l1_msg());
        assert!(!tx.is_morph_tx());

        let regular_tx = MorphTxEnv::default();
        assert!(!regular_tx.is_l1_msg());
    }

    #[test]
    fn test_morph_tx_detection() {
        let mut tx = MorphTxEnv::default();
        tx.inner.tx_type = MORPH_TX_TYPE_ID;
        assert!(tx.is_morph_tx());
        assert!(!tx.is_l1_msg());
    }

    #[test]
    fn test_txenv_morph_tx_detection() {
        let tx = TxEnv {
            tx_type: MORPH_TX_TYPE_ID,
            ..Default::default()
        };
        assert!(tx.is_morph_tx());

        let regular_tx = TxEnv::default();
        assert!(!regular_tx.is_morph_tx());
    }

    #[test]
    fn test_deref() {
        let tx = MorphTxEnv {
            inner: TxEnv {
                gas_limit: 21000,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(tx.gas_limit, 21000);
        assert_eq!(tx.inner.gas_limit, 21000);
    }

    #[test]
    fn test_transaction_trait() {
        let tx = MorphTxEnv::default();
        // Test that Transaction trait methods work
        assert_eq!(Transaction::gas_limit(&tx), tx.inner.gas_limit);
        assert_eq!(Transaction::caller(&tx), tx.inner.caller);
        assert_eq!(Transaction::value(&tx), tx.inner.value);
    }

    #[test]
    fn test_rlp_bytes() {
        let tx = MorphTxEnv::default();
        assert!(tx.rlp_bytes.is_none());

        let tx_with_rlp = MorphTxEnv::default().with_rlp_bytes(Bytes::from(vec![1, 2, 3]));
        assert_eq!(tx_with_rlp.rlp_bytes, Some(Bytes::from(vec![1, 2, 3])));
    }

    #[test]
    fn test_encode_for_l1_fee_legacy() {
        let tx = MorphTxEnv {
            inner: TxEnv {
                chain_id: Some(53077),
                gas_limit: 21_000,
                gas_price: 1,
                nonce: 1,
                kind: TxKind::Call(Address::ZERO),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!tx.encode_for_l1_fee(53077).is_empty());
    }

    #[test]
    fn test_encode_for_l1_fee_morph_tx() {
        let tx = MorphTxEnv {
            inner: TxEnv {
                tx_type: MORPH_TX_TYPE_ID,
                chain_id: Some(53077),
                gas_limit: 21_000,
                gas_price: 1,
                nonce: 1,
                kind: TxKind::Call(Address::ZERO),
                ..Default::default()
            },
            fee_token_id: Some(1),
            fee_limit: Some(U256::from(1000)),
            ..Default::default()
        };
        assert!(!tx.encode_for_l1_fee(53077).is_empty());
    }
}
