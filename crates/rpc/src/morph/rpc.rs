//! jsonrpsee trait for the `morph_` RPC namespace.

use alloy_primitives::B256;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use morph_reference_index::ReferenceTransactionResult;
use serde::{Deserialize, Serialize};

/// Parameters for `morph_getTransactionHashesByReference`.
///
/// `offset`/`limit` use the geth `hexutil.Uint64` wire format (hex-encoded
/// quantity strings like `"0x0"`, `"0x64"`).  Plain integers are also accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceQueryArgs {
    /// 32-byte Morph transaction reference key.
    pub reference: B256,
    /// Starting offset (default 0, max 10 000).
    #[serde(default, with = "alloy_serde::quantity::opt")]
    pub offset: Option<u64>,
    /// Maximum number of results (default 100, max 100).
    #[serde(default, with = "alloy_serde::quantity::opt")]
    pub limit: Option<u64>,
}

/// `morph_` RPC trait.
#[rpc(server, namespace = "morph")]
pub trait MorphRpc {
    /// Return all MorphTx transactions carrying the given `reference` key,
    /// ordered by (block_number, tx_index) ascending, with offset-based
    /// pagination.
    ///
    /// Returns `-32000 "reference index initializing"` while the startup
    /// backfill/reconcile has not yet completed.
    ///
    /// Returns `-32000 "reference index is behind"` when the index lags the
    /// current chain tip by more than the configured threshold (default 16).
    ///
    /// Marked `blocking` because the handler runs synchronous MDBX reads.
    #[method(name = "getTransactionHashesByReference", blocking)]
    fn get_transaction_hashes_by_reference(
        &self,
        args: ReferenceQueryArgs,
    ) -> RpcResult<Vec<ReferenceTransactionResult>>;
}
