//! Morph transaction environment.
//!
//! This module defines the Morph-specific transaction environment with token fee support.

use alloy_consensus::Transaction as AlloyTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_eips::eip2930::AccessList;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_rlp::Decodable;
use morph_primitives::{L1_TX_TYPE_ID, MORPH_TX_TYPE_ID, MorphTransaction, MorphTxEnvelope};
use reth_evm::{FromRecoveredTx, FromTxWithEncoded, ToTxEnv, TransactionEnv};
use revm::context::{Transaction, TxEnv};
use revm::context_interface::transaction::{
    AccessListItem, RecoveredAuthorization, SignedAuthorization,
};
use std::ops::{Deref, DerefMut};

/// Re-export Either for authorization list
pub use alloy_consensus::transaction::Either;

/// Morph transaction environment with token fee support.
///
/// This wraps the standard [`TxEnv`] and adds Morph-specific fields for:
/// - L1 message detection (tx_type 0x7E)
/// - Morph transaction with token-based gas payment (tx_type 0x7F)
/// - RLP encoded transaction bytes for L1 data fee calculation
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MorphTxEnv {
    /// Inner transaction environment.
    pub inner: TxEnv,
    /// RLP encoded transaction bytes.
    /// Used only for L1 data fee calculation.
    pub rlp_bytes: Option<Bytes>,
    /// Token ID for fee payment (only for MorphTransaction type 0x7F).
    /// 0 means ETH payment, > 0 means ERC20 token payment.
    pub fee_token_id: u16,
}

impl Default for MorphTxEnv {
    fn default() -> Self {
        Self {
            inner: TxEnv::default(),
            rlp_bytes: None,
            fee_token_id: 0,
        }
    }
}

impl MorphTxEnv {
    /// Create a new Morph transaction environment from a standard TxEnv.
    pub fn new(inner: TxEnv) -> Self {
        Self {
            inner,
            rlp_bytes: None,
            fee_token_id: 0,
        }
    }

    /// Create a new Morph transaction environment with RLP bytes.
    pub fn with_rlp_bytes(mut self, rlp_bytes: Bytes) -> Self {
        self.rlp_bytes = Some(rlp_bytes);
        self
    }

    /// Set the fee token ID.
    pub fn with_fee_token_id(mut self, fee_token_id: u16) -> Self {
        self.fee_token_id = fee_token_id;
        self
    }

    /// Create a new Morph transaction environment from a recovered transaction.
    ///
    /// This method:
    /// - Converts the transaction to `TxEnv`
    /// - Extracts the RLP-encoded transaction bytes for L1 data fee calculation
    /// - Extracts fee_token_id for MorphTransaction (type 0x7F)
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

        // Extract fee_token_id for MorphTransaction (type 0x7F)
        let fee_token_id = if tx_type == MORPH_TX_TYPE_ID {
            extract_fee_token_id_from_rlp(&rlp_bytes)
        } else {
            0
        };

        // Build TxEnv from the transaction
        let inner = TxEnv {
            tx_type,
            caller: signer,
            gas_limit: AlloyTransaction::gas_limit(tx) as u64,
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
            authorization_list: Default::default(),
        };

        // Use builder pattern to set Morph-specific fields
        Self::new(inner)
            .with_rlp_bytes(rlp_bytes)
            .with_fee_token_id(fee_token_id)
    }
}

/// Extract fee_token_id from RLP-encoded MorphTransaction bytes.
///
/// The bytes should be EIP-2718 encoded (type byte + RLP payload).
/// Returns 0 if decoding fails.
fn extract_fee_token_id_from_rlp(rlp_bytes: &Bytes) -> u16 {
    if rlp_bytes.is_empty() {
        return 0;
    }

    // Skip the type byte (0x7F) and decode the MorphTransaction
    let payload = &rlp_bytes[1..];
    MorphTransaction::decode(&mut &payload[..])
        .map(|tx| tx.fee_token_id)
        .unwrap_or(0)
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
impl ToTxEnv<MorphTxEnv> for MorphTxEnv {
    fn to_tx_env(&self) -> MorphTxEnv {
        self.clone()
    }
}

// Implement FromRecoveredTx for MorphTxEnv
impl FromRecoveredTx<MorphTxEnvelope> for MorphTxEnv {
    fn from_recovered_tx(tx: &MorphTxEnvelope, sender: Address) -> Self {
        MorphTxEnv::from_recovered_tx(tx, sender)
    }
}

// Implement FromTxWithEncoded for MorphTxEnv
impl FromTxWithEncoded<MorphTxEnvelope> for MorphTxEnv {
    fn from_encoded_tx(tx: &MorphTxEnvelope, sender: Address, encoded: Bytes) -> Self {
        MorphTxEnv::from_tx_with_rlp_bytes(tx, sender, encoded)
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

    /// Returns whether this transaction is a Morph transaction (type 0x7F).
    /// Morph transactions support ERC20 token-based gas payment.
    fn is_morph_tx(&self) -> bool;

    /// Returns whether this transaction uses a token for gas payment.
    /// This is true when tx_type is MORPH_TX_TYPE_ID (0x7F).
    fn uses_token_fee(&self) -> bool {
        self.is_morph_tx()
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
        assert!(!tx.uses_token_fee());

        let regular_tx = MorphTxEnv::default();
        assert!(!regular_tx.is_l1_msg());
    }

    #[test]
    fn test_morph_tx_detection() {
        let mut tx = MorphTxEnv::default();
        tx.inner.tx_type = MORPH_TX_TYPE_ID;
        assert!(tx.is_morph_tx());
        assert!(!tx.is_l1_msg());
        // MorphTransaction (0x7F) automatically enables token fee
        assert!(tx.uses_token_fee());
    }

    #[test]
    fn test_token_fee() {
        // Default tx (type 0) doesn't use token fee
        let tx = MorphTxEnv::default();
        assert!(!tx.uses_token_fee());

        // MorphTransaction type (0x7F) enables token fee
        let mut morph_tx = MorphTxEnv::default();
        morph_tx.inner.tx_type = MORPH_TX_TYPE_ID;
        assert!(morph_tx.uses_token_fee());
    }

    #[test]
    fn test_txenv_morph_tx_detection() {
        let mut tx = TxEnv::default();
        tx.tx_type = MORPH_TX_TYPE_ID;
        assert!(tx.is_morph_tx());
        assert!(tx.uses_token_fee());

        let regular_tx = TxEnv::default();
        assert!(!regular_tx.is_morph_tx());
        assert!(!regular_tx.uses_token_fee());
    }

    #[test]
    fn test_deref() {
        let mut tx = MorphTxEnv::default();
        tx.gas_limit = 21000;
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
}
