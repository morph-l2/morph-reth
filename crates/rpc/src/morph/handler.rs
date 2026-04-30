//! `MorphRpc` handler implementation.

use crate::morph::rpc::{MorphRpcServer, ReferenceQueryArgs};
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObjectOwned},
};
use morph_reference_index::{
    ReferenceIndexError, ReferenceIndexReader, ReferenceQuery, ReferenceTransactionResult,
};
use reth_storage_api::BlockNumReader;
use tracing;

// ── Context ──────────────────────────────────────────────────────────────────

/// `morph_` namespace context.  All dependencies are required; no `Option<>`.
///
/// `Provider` must implement [`BlockNumReader`] so the handler can compare the
/// current canonical tip against the index's `indexed_to` for lag detection.
#[derive(Debug, Clone)]
pub struct MorphRpc<Provider> {
    pub reference_index: ReferenceIndexReader,
    pub provider: Provider,
}

impl<Provider: BlockNumReader + Clone + Send + Sync + 'static> MorphRpc<Provider> {
    pub const fn new(reference_index: ReferenceIndexReader, provider: Provider) -> Self {
        Self {
            reference_index,
            provider,
        }
    }
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Handler that wraps [`MorphRpc`] and implements the jsonrpsee server trait.
#[derive(Debug, Clone)]
pub struct MorphRpcHandler<Provider> {
    ctx: MorphRpc<Provider>,
}

impl<Provider: BlockNumReader + Clone + Send + Sync + 'static> MorphRpcHandler<Provider> {
    pub const fn new(ctx: MorphRpc<Provider>) -> Self {
        Self { ctx }
    }
}

impl<Provider: BlockNumReader + Clone + Send + Sync + 'static> MorphRpcServer
    for MorphRpcHandler<Provider>
{
    fn get_transaction_hashes_by_reference(
        &self,
        args: ReferenceQueryArgs,
    ) -> RpcResult<Vec<ReferenceTransactionResult>> {
        let query =
            ReferenceQuery::new(args.reference, args.offset, args.limit).map_err(to_rpc_error)?;

        let canonical_tip = self
            .ctx
            .provider
            .best_block_number()
            .map_err(|e| to_rpc_error(ReferenceIndexError::Other(eyre::eyre!(e))))?;

        self.ctx
            .reference_index
            .query(query, canonical_tip)
            .map_err(to_rpc_error)
    }
}

// ── error mapping ─────────────────────────────────────────────────────────────

fn to_rpc_error(error: ReferenceIndexError) -> ErrorObjectOwned {
    match error {
        ReferenceIndexError::Initializing => {
            ErrorObjectOwned::owned(-32000, "reference index initializing", None::<()>)
        }
        ReferenceIndexError::IndexBehind => {
            ErrorObjectOwned::owned(-32000, "reference index is behind", None::<()>)
        }
        ReferenceIndexError::LimitTooLarge { .. } | ReferenceIndexError::OffsetTooLarge { .. } => {
            ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                error.to_string(),
                None::<()>,
            )
        }
        // Log internal details for operators but return a generic message on
        // the wire so Database/Provider/Other error strings don't leak.
        other => {
            tracing::error!(
                target: "morph::reference_index_rpc",
                error = %other,
                "reference index internal error"
            );
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                "internal reference index error",
                None::<()>,
            )
        }
    }
}
