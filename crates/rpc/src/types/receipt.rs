//! Morph RPC receipt type.

use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, BlockHash, U64, U256};
use alloy_rpc_types_eth::{Log, TransactionReceipt};
use morph_primitives::MorphReceiptEnvelope;
use serde::{Deserialize, Serialize};

/// Morph RPC transaction receipt representation.
///
/// Wraps the standard RPC transaction receipt and adds Morph-specific fields:
/// - L1 fee and fee token metadata
/// - Custom tx type for L1 message / Morph tx receipts
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphRpcReceipt {
    /// Standard RPC receipt fields.
    #[serde(flatten)]
    pub inner: TransactionReceipt<MorphReceiptEnvelope<Log>>,

    /// L1 data fee paid (in wei).
    #[serde(rename = "l1Fee")]
    pub l1_fee: U256,

    /// Fee rate used for token fee calculation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_rate: Option<U256>,

    /// Token scale factor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_scale: Option<U256>,

    /// Fee limit specified in the transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_limit: Option<U256>,

    /// Token ID used for fee payment.
    #[serde(rename = "feeTokenID", skip_serializing_if = "Option::is_none")]
    pub fee_token_id: Option<U64>,
}

impl ReceiptResponse for MorphRpcReceipt {
    fn contract_address(&self) -> Option<Address> {
        self.inner.contract_address
    }

    fn status(&self) -> bool {
        self.inner.inner.status()
    }

    fn block_hash(&self) -> Option<BlockHash> {
        self.inner.block_hash
    }

    fn block_number(&self) -> Option<u64> {
        self.inner.block_number
    }

    fn transaction_hash(&self) -> alloy_primitives::B256 {
        self.inner.transaction_hash
    }

    fn transaction_index(&self) -> Option<u64> {
        self.inner.transaction_index()
    }

    fn gas_used(&self) -> u64 {
        self.inner.gas_used()
    }

    fn effective_gas_price(&self) -> u128 {
        self.inner.effective_gas_price()
    }

    fn blob_gas_used(&self) -> Option<u64> {
        self.inner.blob_gas_used()
    }

    fn blob_gas_price(&self) -> Option<u128> {
        self.inner.blob_gas_price()
    }

    fn from(&self) -> Address {
        self.inner.from()
    }

    fn to(&self) -> Option<Address> {
        self.inner.to()
    }

    fn cumulative_gas_used(&self) -> u64 {
        self.inner.cumulative_gas_used()
    }

    fn state_root(&self) -> Option<alloy_primitives::B256> {
        self.inner.state_root()
    }
}
