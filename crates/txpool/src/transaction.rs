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

    /// Returns true if this is a Morph transaction.
    pub fn is_morph_tx(&self) -> bool {
        self.inner.transaction().tx_type() == MorphTxType::Morph
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Sealed, Signed, Transaction, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_eips::eip4844::BlobTransactionSidecar;
    use alloy_primitives::{Bytes, Signature, U256};
    use morph_primitives::transaction::TxL1Msg;
    use reth_transaction_pool::PoolTransaction;

    fn create_legacy_pooled_tx() -> MorphPooledTransaction {
        let tx = TxLegacy {
            chain_id: Some(1337),
            nonce: 5,
            gas_price: 1_000_000_000,
            gas_limit: 21000,
            to: TxKind::Call(Address::repeat_byte(0x01)),
            value: U256::from(100u64),
            input: Bytes::new(),
        };
        let sig = Signature::test_signature();
        let envelope = MorphTxEnvelope::Legacy(Signed::new_unhashed(tx, sig));
        let recovered = Recovered::new_unchecked(envelope, Address::repeat_byte(0xaa));
        let len = recovered.encode_2718_len();
        MorphPooledTransaction::new(recovered, len)
    }

    fn create_l1_msg_pooled_tx(queue_index: u64) -> MorphPooledTransaction {
        let tx = TxL1Msg {
            queue_index,
            gas_limit: 21000,
            to: Address::ZERO,
            value: U256::ZERO,
            input: Bytes::default(),
            sender: Address::repeat_byte(0xbb),
        };
        let envelope = MorphTxEnvelope::L1Msg(Sealed::new(tx));
        let recovered = Recovered::new_unchecked(envelope, Address::repeat_byte(0xbb));
        let len = recovered.encode_2718_len();
        MorphPooledTransaction::new(recovered, len)
    }

    fn create_morph_pooled_tx() -> MorphPooledTransaction {
        use morph_primitives::TxMorph;
        let tx = TxMorph {
            chain_id: 1337,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(Address::repeat_byte(0x02)),
            value: U256::ZERO,
            access_list: Default::default(),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
            input: Bytes::new(),
        };
        let sig = Signature::test_signature();
        let envelope = MorphTxEnvelope::Morph(Signed::new_unhashed(tx, sig));
        let recovered = Recovered::new_unchecked(envelope, Address::repeat_byte(0xcc));
        let len = recovered.encode_2718_len();
        MorphPooledTransaction::new(recovered, len)
    }

    #[test]
    fn test_is_l1_message() {
        let l1_tx = create_l1_msg_pooled_tx(0);
        assert!(l1_tx.is_l1_message());
        assert_eq!(l1_tx.queue_index(), Some(0));

        let legacy_tx = create_legacy_pooled_tx();
        assert!(!legacy_tx.is_l1_message());
        assert_eq!(legacy_tx.queue_index(), None);
    }

    #[test]
    fn test_is_morph_tx() {
        let morph_tx = create_morph_pooled_tx();
        assert!(morph_tx.is_morph_tx());

        let legacy_tx = create_legacy_pooled_tx();
        assert!(!legacy_tx.is_morph_tx());
    }

    #[test]
    fn test_pool_transaction_sender() {
        let tx = create_legacy_pooled_tx();
        assert_eq!(tx.sender(), Address::repeat_byte(0xaa));
    }

    #[test]
    fn test_pool_transaction_nonce() {
        let tx = create_legacy_pooled_tx();
        assert_eq!(tx.nonce(), 5);
    }

    #[test]
    fn test_pool_transaction_value() {
        let tx = create_legacy_pooled_tx();
        assert_eq!(tx.value(), U256::from(100u64));
    }

    #[test]
    fn test_pool_transaction_gas_limit() {
        let tx = create_legacy_pooled_tx();
        assert_eq!(tx.gas_limit(), 21000);
    }

    #[test]
    fn test_encoded_2718_is_cached() {
        let tx = create_legacy_pooled_tx();
        let bytes1 = tx.encoded_2718().clone();
        let bytes2 = tx.encoded_2718().clone();
        assert_eq!(bytes1, bytes2, "cached encoding should be identical");
        assert!(!bytes1.is_empty());
    }

    #[test]
    fn test_from_pooled_roundtrip() {
        let original = create_legacy_pooled_tx();
        let hash = *original.hash();
        let sender = original.sender();

        let consensus = original.into_consensus();
        assert_eq!(consensus.signer(), sender);

        let recreated = MorphPooledTransaction::from_pooled(consensus);
        assert_eq!(*recreated.hash(), hash);
        assert_eq!(recreated.sender(), sender);
    }

    #[test]
    fn test_take_blob_returns_none() {
        let mut tx = create_legacy_pooled_tx();
        let blob = tx.take_blob();
        assert!(matches!(blob, EthBlobTransactionSidecar::None));
    }

    #[test]
    fn test_try_into_pooled_eip4844_returns_none() {
        let tx = create_legacy_pooled_tx();
        let sidecar = Arc::new(BlobTransactionSidecarVariant::Eip4844(
            BlobTransactionSidecar::default(),
        ));
        let result = tx.try_into_pooled_eip4844(sidecar);
        assert!(result.is_none());
    }

    #[test]
    fn test_try_from_eip4844_returns_none() {
        // Morph doesn't support blob transactions, so try_from_eip4844 always returns None
        let tx = create_legacy_pooled_tx();
        let recovered = tx.into_consensus();
        let sidecar = BlobTransactionSidecar::default();
        let result = MorphPooledTransaction::try_from_eip4844(
            recovered,
            BlobTransactionSidecarVariant::Eip4844(sidecar),
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_encoded_length_matches() {
        let tx = create_legacy_pooled_tx();
        // encoded_length is set during construction
        assert!(tx.encoded_length() > 0);
    }
}
