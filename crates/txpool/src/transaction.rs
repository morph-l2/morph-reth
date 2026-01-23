//! Pool transaction type for Morph L2.

use alloy_consensus::{
    BlobTransactionValidationError, Typed2718, transaction::Recovered, transaction::TxHashRef,
};
use alloy_eips::{
    eip2930::AccessList, eip7594::BlobTransactionSidecarVariant, eip7702::SignedAuthorization,
};
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U256};
use c_kzg::KzgSettings;
use core::fmt::Debug;
use morph_primitives::{MorphTxEnvelope, MorphTxType};
use reth_primitives_traits::InMemorySize;
use reth_transaction_pool::{
    EthBlobTransactionSidecar, EthPoolTransaction, EthPooledTransaction, PoolTransaction,
};
use std::sync::{Arc, OnceLock};

/// Pool transaction for Morph L2.
///
/// This type wraps the actual transaction and caches values that are frequently used by the pool.
/// It provides efficient access to encoded transaction bytes for L1 fee calculation.
#[derive(Debug, Clone, derive_more::Deref)]
pub struct MorphPooledTransaction {
    #[deref]
    inner: EthPooledTransaction<MorphTxEnvelope>,

    /// Cached EIP-2718 encoded bytes of the transaction, lazily computed.
    encoded_2718: OnceLock<Bytes>,
}

impl MorphPooledTransaction {
    /// Create a new instance of [`MorphPooledTransaction`].
    pub fn new(transaction: Recovered<MorphTxEnvelope>, encoded_length: usize) -> Self {
        Self {
            inner: EthPooledTransaction::new(transaction, encoded_length),
            encoded_2718: Default::default(),
        }
    }

    /// Returns lazily computed EIP-2718 encoded bytes of the transaction.
    pub fn encoded_2718(&self) -> &Bytes {
        self.encoded_2718
            .get_or_init(|| self.inner.transaction().rlp())
    }

    /// Returns true if this is an L1 message transaction.
    pub fn is_l1_message(&self) -> bool {
        self.inner.transaction().is_l1_msg()
    }

    /// Returns the queue index for L1 message transactions.
    pub fn queue_index(&self) -> Option<u64> {
        self.inner.transaction().queue_index()
    }

    /// Returns true if this is an AltFee transaction.
    pub fn is_alt_fee(&self) -> bool {
        self.inner.transaction().tx_type() == MorphTxType::AltFee
    }
}

impl PoolTransaction for MorphPooledTransaction {
    type TryFromConsensusError = <MorphTxEnvelope as TryFrom<MorphTxEnvelope>>::Error;
    type Consensus = MorphTxEnvelope;
    type Pooled = MorphTxEnvelope;

    fn clone_into_consensus(&self) -> Recovered<Self::Consensus> {
        self.inner.transaction().clone()
    }

    fn into_consensus(self) -> Recovered<Self::Consensus> {
        self.inner.transaction
    }

    fn from_pooled(tx: Recovered<Self::Pooled>) -> Self {
        let encoded_len = alloy_eips::eip2718::Encodable2718::encode_2718_len(&tx);
        Self::new(tx, encoded_len)
    }

    fn hash(&self) -> &TxHash {
        self.inner.transaction.tx_hash()
    }

    fn sender(&self) -> Address {
        self.inner.transaction.signer()
    }

    fn sender_ref(&self) -> &Address {
        self.inner.transaction.signer_ref()
    }

    fn cost(&self) -> &U256 {
        &self.inner.cost
    }

    fn encoded_length(&self) -> usize {
        self.inner.encoded_length
    }
}

impl Typed2718 for MorphPooledTransaction {
    fn ty(&self) -> u8 {
        self.inner.ty()
    }
}

impl InMemorySize for MorphPooledTransaction {
    fn size(&self) -> usize {
        self.inner.size()
    }
}

impl alloy_consensus::Transaction for MorphPooledTransaction {
    fn chain_id(&self) -> Option<u64> {
        self.inner.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.inner.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.inner.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.inner.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.inner.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.inner.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.inner.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.inner.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.inner.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.inner.kind()
    }

    fn is_create(&self) -> bool {
        self.inner.is_create()
    }

    fn value(&self) -> U256 {
        self.inner.value()
    }

    fn input(&self) -> &Bytes {
        self.inner.input()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.inner.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.inner.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.inner.authorization_list()
    }
}

impl EthPoolTransaction for MorphPooledTransaction {
    fn take_blob(&mut self) -> EthBlobTransactionSidecar {
        EthBlobTransactionSidecar::None
    }

    fn try_into_pooled_eip4844(
        self,
        _sidecar: Arc<BlobTransactionSidecarVariant>,
    ) -> Option<Recovered<Self::Pooled>> {
        None
    }

    fn try_from_eip4844(
        _tx: Recovered<Self::Consensus>,
        _sidecar: BlobTransactionSidecarVariant,
    ) -> Option<Self> {
        None
    }

    fn validate_blob(
        &self,
        _sidecar: &BlobTransactionSidecarVariant,
        _settings: &KzgSettings,
    ) -> Result<(), BlobTransactionValidationError> {
        Err(BlobTransactionValidationError::NotBlobTransaction(
            self.ty(),
        ))
    }
}
