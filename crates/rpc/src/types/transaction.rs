//! Morph RPC transaction type.

use alloy_consensus::Transaction as ConsensusTransaction;
use alloy_consensus::Transaction as TransactionTrait;
use alloy_eips::Typed2718;
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, B256, BlockHash, Bytes, TxKind, U64, U256};
use alloy_rpc_types_eth::Transaction as RpcTransaction;
use morph_primitives::MorphTxEnvelope;
use serde::{Deserialize, Serialize};

/// Morph RPC transaction representation.
///
/// Wraps the standard RPC transaction and adds Morph-specific fields:
/// - L1 message sender/queue index
/// - Morph fee token fields (version, fee_token_id, fee_limit, reference, memo)
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, derive_more::Deref, derive_more::DerefMut,
)]
#[serde(rename_all = "camelCase")]
pub struct MorphRpcTransaction {
    /// Standard RPC transaction fields.
    #[serde(flatten)]
    #[deref]
    #[deref_mut]
    pub inner: RpcTransaction<MorphTxEnvelope>,

    /// L1 message sender (only for L1Message type 0x7E).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<Address>,

    /// L1 message queue index (only for L1Message type 0x7E).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_index: Option<U64>,

    /// MorphTx version (only for MorphTx type 0x7F).
    /// 0 = legacy format, 1 = with reference/memo support.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u8>,

    /// Token ID for fee payment (only for MorphTx type 0x7F).
    #[serde(rename = "feeTokenID", skip_serializing_if = "Option::is_none")]
    pub fee_token_id: Option<U64>,

    /// Maximum token amount willing to pay for fees (only for MorphTx type 0x7F).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_limit: Option<U256>,

    /// Reference key for transaction indexing (only for MorphTx type 0x7F).
    /// 32-byte key used for looking up transactions by external systems.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<B256>,

    /// Memo field for arbitrary data (only for MorphTx type 0x7F).
    /// Up to 64 bytes for notes, invoice numbers, or other metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memo: Option<Bytes>,
}

/// Implementation of [`Typed2718`] for Morph RPC transactions.
impl Typed2718 for MorphRpcTransaction {
    fn ty(&self) -> u8 {
        self.inner.ty()
    }
}

/// Implementation of [`ConsensusTransaction`] for Morph RPC transactions.
///
/// Delegates all consensus transaction methods to the inner transaction.
impl ConsensusTransaction for MorphRpcTransaction {
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
        TransactionTrait::gas_price(&self.inner)
    }

    fn max_fee_per_gas(&self) -> u128 {
        TransactionTrait::max_fee_per_gas(&self.inner)
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

    fn to(&self) -> Option<Address> {
        self.inner.to()
    }

    fn value(&self) -> U256 {
        self.inner.value()
    }

    fn input(&self) -> &alloy_primitives::Bytes {
        self.inner.input()
    }

    fn access_list(&self) -> Option<&alloy_eips::eip2930::AccessList> {
        self.inner.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[alloy_primitives::B256]> {
        self.inner.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[alloy_eips::eip7702::SignedAuthorization]> {
        self.inner.authorization_list()
    }
}

/// Implementation of [`TransactionResponse`] for Morph RPC transactions.
///
/// Provides RPC-specific transaction metadata like block hash and index.
impl TransactionResponse for MorphRpcTransaction {
    fn tx_hash(&self) -> alloy_primitives::B256 {
        self.inner.tx_hash()
    }

    fn block_hash(&self) -> Option<BlockHash> {
        self.inner.block_hash()
    }

    fn block_number(&self) -> Option<u64> {
        self.inner.block_number()
    }

    fn transaction_index(&self) -> Option<u64> {
        self.inner.transaction_index()
    }

    fn from(&self) -> Address {
        self.inner.from()
    }
}
