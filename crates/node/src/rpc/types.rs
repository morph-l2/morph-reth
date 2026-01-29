//! Morph-specific RPC types
//!
//! This module provides RPC representations for Morph transactions, receipts, and blocks.

use alloy_primitives::{Address, B256, BlockHash, Bloom, Bytes, U64, U128, U256};
use alloy_rpc_types_eth::{AccessList, Log};
use alloy_serde::OtherFields;
use serde::{Deserialize, Serialize};

/// Morph RPC transaction representation
///
/// Extends standard Ethereum transaction with:
/// - L1Message fields (sender, queueIndex)
/// - AltFee/MorphTx fields (feeTokenID, feeLimit)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphRpcTransaction {
    /// Transaction hash
    pub hash: B256,
    /// Transaction nonce
    pub nonce: U64,
    /// Block hash where this transaction was included
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<BlockHash>,
    /// Block number where this transaction was included
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_number: Option<U256>,
    /// Transaction index in the block
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_index: Option<U256>,
    /// Sender address
    pub from: Address,
    /// Recipient address (None for contract creation)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Address>,
    /// Transferred value
    pub value: U256,
    /// Gas limit
    pub gas: U256,
    /// Gas price (for legacy and access list transactions)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_price: Option<U128>,
    /// Maximum fee per gas (EIP-1559)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fee_per_gas: Option<U128>,
    /// Maximum priority fee per gas (EIP-1559)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_priority_fee_per_gas: Option<U128>,
    /// Transaction input data
    pub input: Bytes,
    /// Transaction type (0x00-0x7f)
    #[serde(rename = "type")]
    pub tx_type: U64,
    /// Access list (EIP-2930)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_list: Option<AccessList>,
    /// Chain ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<U64>,
    /// ECDSA signature v
    pub v: U256,
    /// ECDSA signature r
    pub r: U256,
    /// ECDSA signature s
    pub s: U256,
    /// EIP-2718 y parity
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y_parity: Option<U64>,

    // Morph-specific fields for L1Message transactions (0x7E)
    /// L1 message sender (only for L1Message type 0x7E)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<Address>,
    /// L1 message queue index (only for L1Message type 0x7E)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_index: Option<U64>,

    // Morph-specific fields for AltFee/MorphTx transactions (0x7F)
    /// Token ID for fee payment (only for MorphTx type 0x7F)
    #[serde(rename = "feeTokenID", skip_serializing_if = "Option::is_none")]
    pub fee_token_id: Option<U64>,
    /// Maximum token amount willing to pay for fees (only for MorphTx type 0x7F)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_limit: Option<U256>,

    /// Additional fields for future extensibility
    #[serde(flatten)]
    pub other: OtherFields,
}

/// Morph RPC transaction receipt
///
/// Extends standard Ethereum receipt with L1 fee and token fee fields
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphRpcReceipt {
    /// Transaction hash
    pub transaction_hash: B256,
    /// Transaction index in the block
    pub transaction_index: U64,
    /// Block hash
    pub block_hash: BlockHash,
    /// Block number
    pub block_number: U256,
    /// Sender address
    pub from: Address,
    /// Recipient address (None for contract creation)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Address>,
    /// Cumulative gas used in the block
    pub cumulative_gas_used: U256,
    /// Gas used by this transaction
    pub gas_used: U256,
    /// Effective gas price paid
    pub effective_gas_price: U128,
    /// Contract address created (for contract creation)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_address: Option<Address>,
    /// Logs emitted
    pub logs: Vec<Log>,
    /// Logs bloom filter
    pub logs_bloom: Bloom,
    /// Transaction type
    #[serde(rename = "type")]
    pub tx_type: U64,
    /// Status code (1 for success, 0 for failure)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<U64>,
    /// State root (pre-Byzantium)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<B256>,

    // Morph-specific fields
    /// L1 data fee paid (in wei)
    #[serde(rename = "l1Fee")]
    pub l1_fee: U256,
    /// Fee rate used for token fee calculation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_rate: Option<U256>,
    /// Token scale factor
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_scale: Option<U256>,
    /// Fee limit specified in the transaction
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_limit: Option<U256>,
    /// Token ID used for fee payment
    #[serde(rename = "feeTokenID", skip_serializing_if = "Option::is_none")]
    pub fee_token_id: Option<U64>,

    /// Additional fields for future extensibility
    #[serde(flatten)]
    pub other: OtherFields,
}

/// Sync status information
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    /// Current L2 block number
    pub current_l2_block: U64,
    /// Highest known L2 block
    pub highest_l2_block: U64,
    /// Current L1 sync height
    pub l1_sync_height: U64,
    /// Latest relayed L1 message queue index
    pub latest_relayed_queue_index: U64,
}

/// Disk and header root response
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskAndHeaderRoot {
    /// The state root stored on disk
    pub disk_root: B256,
    /// The state root in the block header
    pub header_root: B256,
}
