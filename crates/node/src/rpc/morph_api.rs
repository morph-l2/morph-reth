//! Morph RPC API implementation
//!
//! This module provides the `morph_` namespace JSON-RPC methods.

use crate::rpc::{DiskAndHeaderRoot, SyncStatus};
use alloy_primitives::{B256, BlockHash, U64};
use alloy_rpc_types_eth::{BlockNumberOrTag, TransactionRequest};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

/// Morph-specific RPC methods under the `morph_` namespace
#[rpc(server, namespace = "morph")]
pub trait MorphApi {
    /// Returns the block by hash with additional Morph-specific fields.
    ///
    /// This is similar to `eth_getBlockByHash` but includes:
    /// - `withdrawTrieRoot`: The withdraw trie root from the L2 message queue
    /// - `batchHash`: The batch hash if this is a batch point
    /// - `nextL1MsgIndex`: The next L1 message index to be processed
    #[method(name = "getBlockByHash")]
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
        full_transactions: bool,
    ) -> RpcResult<Option<serde_json::Value>>;

    /// Returns the block by number with additional Morph-specific fields.
    #[method(name = "getBlockByNumber")]
    async fn get_block_by_number(
        &self,
        number: BlockNumberOrTag,
        full_transactions: bool,
    ) -> RpcResult<Option<serde_json::Value>>;

    /// Estimates the L1 data fee for a transaction.
    ///
    /// The L1 data fee is the cost of posting transaction data to L1.
    /// This is separate from the L2 execution gas fee.
    #[method(name = "estimateL1DataFee")]
    async fn estimate_l1_data_fee(
        &self,
        tx: TransactionRequest,
        block_number: Option<BlockNumberOrTag>,
    ) -> RpcResult<U64>;

    /// Returns the total number of skipped transactions.
    #[method(name = "getNumSkippedTransactions")]
    async fn get_num_skipped_transactions(&self) -> RpcResult<U64>;

    /// Returns the hashes of skipped transactions in a range.
    #[method(name = "getSkippedTransactionHashes")]
    async fn get_skipped_transaction_hashes(&self, from: U64, to: U64) -> RpcResult<Vec<B256>>;

    /// Returns the current L1 sync height.
    #[method(name = "getL1SyncHeight")]
    async fn get_l1_sync_height(&self) -> RpcResult<U64>;

    /// Returns the latest relayed L1 message queue index.
    #[method(name = "getLatestRelayedQueueIndex")]
    async fn get_latest_relayed_queue_index(&self) -> RpcResult<U64>;

    /// Returns the current sync status.
    #[method(name = "syncStatus")]
    async fn sync_status(&self) -> RpcResult<SyncStatus>;

    /// Returns both the disk state root and header root for a block.
    #[method(name = "diskRoot")]
    async fn disk_root(
        &self,
        block_number: Option<BlockNumberOrTag>,
    ) -> RpcResult<DiskAndHeaderRoot>;
}

/// Implementation of the `morph_` namespace RPC methods.
///
/// This is a placeholder implementation. The actual implementation will be
/// completed once the full node components are available.
#[derive(Debug, Clone, Default)]
pub struct MorphRpc {
    // Provider and other dependencies will be added here
}

impl MorphRpc {
    /// Creates a new `MorphRpc` instance.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl MorphApiServer for MorphRpc {
    async fn get_block_by_hash(
        &self,
        _hash: BlockHash,
        _full_transactions: bool,
    ) -> RpcResult<Option<serde_json::Value>> {
        // TODO: Implement with Morph-specific fields (withdrawTrieRoot, batchHash, nextL1MsgIndex)
        Err(jsonrpsee::types::ErrorObject::owned(
            -32601,
            "method not implemented",
            None::<()>,
        ))
    }

    async fn get_block_by_number(
        &self,
        _number: BlockNumberOrTag,
        _full_transactions: bool,
    ) -> RpcResult<Option<serde_json::Value>> {
        // TODO: Implement with Morph-specific fields
        Err(jsonrpsee::types::ErrorObject::owned(
            -32601,
            "method not implemented",
            None::<()>,
        ))
    }

    async fn estimate_l1_data_fee(
        &self,
        _tx: TransactionRequest,
        _block_number: Option<BlockNumberOrTag>,
    ) -> RpcResult<U64> {
        // TODO: Implement L1 data fee estimation using morph-revm
        Err(jsonrpsee::types::ErrorObject::owned(
            -32601,
            "method not implemented",
            None::<()>,
        ))
    }

    async fn get_num_skipped_transactions(&self) -> RpcResult<U64> {
        // TODO: Query from database
        Ok(U64::ZERO)
    }

    async fn get_skipped_transaction_hashes(&self, _from: U64, _to: U64) -> RpcResult<Vec<B256>> {
        // TODO: Query from database
        Ok(vec![])
    }

    async fn get_l1_sync_height(&self) -> RpcResult<U64> {
        // TODO: Query from sync state
        Ok(U64::ZERO)
    }

    async fn get_latest_relayed_queue_index(&self) -> RpcResult<U64> {
        // TODO: Query from database
        Ok(U64::ZERO)
    }

    async fn sync_status(&self) -> RpcResult<SyncStatus> {
        // TODO: Query actual sync status
        Ok(SyncStatus {
            current_l2_block: U64::ZERO,
            highest_l2_block: U64::ZERO,
            l1_sync_height: U64::ZERO,
            latest_relayed_queue_index: U64::ZERO,
        })
    }

    async fn disk_root(
        &self,
        _block_number: Option<BlockNumberOrTag>,
    ) -> RpcResult<DiskAndHeaderRoot> {
        // TODO: Implement disk root lookup
        Err(jsonrpsee::types::ErrorObject::owned(
            -32601,
            "method not implemented",
            None::<()>,
        ))
    }
}
