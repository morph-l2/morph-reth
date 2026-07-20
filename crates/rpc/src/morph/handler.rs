//! `MorphRpc` handler implementation.

use crate::morph::rpc::{MorphRpcServer, ReferenceQueryArgs};
use alloy_consensus::BlockHeader as _;
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObjectOwned},
};
use morph_reference_index::{
    CanonicalTip, ReferenceIndexError, ReferenceIndexHandle, ReferenceQuery,
    ReferenceTransactionResult,
};
use reth_storage_api::{BlockNumReader, HeaderProvider};
use tracing;

const TARGET: &str = "morph::reference_index_rpc";

// ── Context ──────────────────────────────────────────────────────────────────

/// `morph_` namespace context.  All dependencies are required; no `Option<>`.
///
/// `Provider` supplies an exact canonical tip snapshot (number, hash, and
/// timestamp) for strict index-read consistency.
#[derive(Debug, Clone)]
pub struct MorphRpc<Provider> {
    pub reference_index: ReferenceIndexHandle,
    pub provider: Provider,
}

impl<Provider> MorphRpc<Provider> {
    pub const fn new(reference_index: ReferenceIndexHandle, provider: Provider) -> Self {
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

impl<Provider> MorphRpcHandler<Provider> {
    pub const fn new(ctx: MorphRpc<Provider>) -> Self {
        Self { ctx }
    }
}

impl<Provider> MorphRpcServer for MorphRpcHandler<Provider>
where
    Provider: BlockNumReader + HeaderProvider + Clone + Send + Sync + 'static,
{
    fn get_transaction_hashes_by_reference(
        &self,
        args: ReferenceQueryArgs,
    ) -> RpcResult<Vec<ReferenceTransactionResult>> {
        let query =
            ReferenceQuery::new(args.reference, args.offset, args.limit).map_err(to_rpc_error)?;

        let before = self
            .ctx
            .provider
            .chain_info()
            .map_err(ReferenceIndexError::from)
            .map_err(to_rpc_error)?;
        let header = self
            .ctx
            .provider
            .header(before.best_hash)
            .map_err(ReferenceIndexError::from)
            .map_err(to_rpc_error)?
            .ok_or(ReferenceIndexError::IndexBehind)
            .map_err(to_rpc_error)?;
        if header.number() != before.best_number {
            return Err(to_rpc_error(ReferenceIndexError::IndexBehind));
        }
        let canonical_tip = CanonicalTip {
            number: before.best_number,
            hash: before.best_hash,
            timestamp: header.timestamp(),
        };

        // Pre-Jade is a complete, terminal empty answer: no reference-carrying
        // transaction can exist before Jade, so return `[]` without touching the
        // index. This must not be `IndexBehind` (which instructs clients to
        // retry) — before Jade there is nothing to wait for.
        if self.ctx.reference_index.is_pre_jade(canonical_tip.timestamp) {
            return Ok(Vec::new());
        }

        let result = self
            .ctx
            .reference_index
            .query_at(query, canonical_tip)
            .map_err(to_rpc_error)?;

        let after = self
            .ctx
            .provider
            .chain_info()
            .map_err(ReferenceIndexError::from)
            .map_err(to_rpc_error)?;
        if after != before {
            return Err(to_rpc_error(ReferenceIndexError::IndexBehind));
        }

        Ok(result)
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
        ReferenceIndexError::IndexUnavailable => {
            ErrorObjectOwned::owned(-32000, "reference index is unavailable", None::<()>)
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
                target: TARGET,
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use morph_primitives::MorphHeader;
    use morph_reference_index::{
        CanonicalBlock, CanonicalChain, CanonicalTip, ReferenceIndexConfig, ReferenceIndexHandle,
        ReferenceIndexRuntime,
    };
    use reth_chainspec::ChainInfo;
    use reth_errors::ProviderResult;
    use reth_primitives_traits::SealedHeader;
    use reth_storage_api::{BlockHashReader, HeaderProvider};
    use std::{
        collections::{BTreeMap, VecDeque},
        ops::{RangeBounds, RangeInclusive},
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Debug)]
    struct TestChain {
        block: CanonicalBlock,
    }

    impl CanonicalChain for TestChain {
        fn head(&self) -> Result<CanonicalTip, ReferenceIndexError> {
            Ok(CanonicalTip {
                number: self.block.number,
                hash: self.block.hash,
                timestamp: self.block.timestamp,
            })
        }

        fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ReferenceIndexError> {
            Ok((number == self.block.number).then_some(self.block.hash))
        }

        fn block_timestamp(&self, number: u64) -> Result<Option<u64>, ReferenceIndexError> {
            Ok((number == self.block.number).then_some(self.block.timestamp))
        }

        fn canonical_blocks(
            &self,
            range: RangeInclusive<u64>,
        ) -> Result<Vec<CanonicalBlock>, ReferenceIndexError> {
            Ok(range
                .contains(&self.block.number)
                .then(|| self.block.clone())
                .into_iter()
                .collect())
        }
    }

    #[derive(Clone, Debug)]
    struct TestProvider {
        chain_info: Arc<Mutex<VecDeque<ChainInfo>>>,
        headers: Arc<BTreeMap<B256, MorphHeader>>,
    }

    impl TestProvider {
        fn new(
            chain_info: impl IntoIterator<Item = ChainInfo>,
            headers: BTreeMap<B256, MorphHeader>,
        ) -> Self {
            Self {
                chain_info: Arc::new(Mutex::new(chain_info.into_iter().collect())),
                headers: Arc::new(headers),
            }
        }

        fn info(&self) -> ChainInfo {
            let mut infos = self.chain_info.lock().unwrap();
            if infos.len() > 1 {
                infos.pop_front().unwrap()
            } else {
                *infos.front().unwrap()
            }
        }
    }

    impl BlockHashReader for TestProvider {
        fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
            Ok(self
                .headers
                .iter()
                .find_map(|(hash, header)| (header.number() == number).then_some(*hash)))
        }

        fn canonical_hashes_range(&self, start: u64, end: u64) -> ProviderResult<Vec<B256>> {
            Ok(self
                .headers
                .iter()
                .filter_map(|(hash, header)| {
                    (start..end).contains(&header.number()).then_some(*hash)
                })
                .collect())
        }
    }

    impl BlockNumReader for TestProvider {
        fn chain_info(&self) -> ProviderResult<ChainInfo> {
            Ok(self.info())
        }

        fn best_block_number(&self) -> ProviderResult<u64> {
            Ok(self.info().best_number)
        }

        fn last_block_number(&self) -> ProviderResult<u64> {
            self.best_block_number()
        }

        fn block_number(&self, hash: B256) -> ProviderResult<Option<u64>> {
            Ok(self.headers.get(&hash).map(|header| header.number()))
        }
    }

    impl HeaderProvider for TestProvider {
        type Header = MorphHeader;

        fn header(&self, block_hash: B256) -> ProviderResult<Option<Self::Header>> {
            Ok(self.headers.get(&block_hash).cloned())
        }

        fn header_by_number(&self, number: u64) -> ProviderResult<Option<Self::Header>> {
            Ok(self
                .headers
                .values()
                .find(|header| header.number() == number)
                .cloned())
        }

        fn headers_range(
            &self,
            _range: impl RangeBounds<u64>,
        ) -> ProviderResult<Vec<Self::Header>> {
            Ok(self.headers.values().cloned().collect())
        }

        fn sealed_header(&self, number: u64) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
            Ok(self.headers.iter().find_map(|(hash, header)| {
                (header.number() == number).then(|| SealedHeader::new(header.clone(), *hash))
            }))
        }

        fn sealed_headers_while(
            &self,
            _range: impl RangeBounds<u64>,
            mut predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
        ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
            Ok(self
                .headers
                .iter()
                .map(|(hash, header)| SealedHeader::new(header.clone(), *hash))
                .take_while(|header| predicate(header))
                .collect())
        }
    }

    fn header(number: u64, timestamp: u64) -> MorphHeader {
        let mut header = MorphHeader::default();
        header.inner.number = number;
        header.inner.timestamp = timestamp;
        header
    }

    fn synced_handle(
        jade_timestamp: u64,
        block_timestamp: u64,
        hash: B256,
    ) -> (tempfile::TempDir, ReferenceIndexHandle) {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain {
            block: CanonicalBlock {
                number: 0,
                hash,
                timestamp: block_timestamp,
                transactions: Vec::new(),
            },
        };
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, jade_timestamp);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain);
        runtime.synchronize_once(false).unwrap();
        (dir, handle)
    }

    fn args() -> ReferenceQueryArgs {
        ReferenceQueryArgs {
            reference: B256::repeat_byte(0x42),
            offset: None,
            limit: None,
        }
    }

    #[test]
    fn rejects_results_if_canonical_head_changes_during_query() {
        let indexed_hash = B256::repeat_byte(0x11);
        let changed_hash = B256::repeat_byte(0x22);
        let (_dir, handle) = synced_handle(0, 100, indexed_hash);
        let provider = TestProvider::new(
            [
                ChainInfo {
                    best_hash: indexed_hash,
                    best_number: 0,
                },
                ChainInfo {
                    best_hash: changed_hash,
                    best_number: 0,
                },
            ],
            BTreeMap::from([(indexed_hash, header(0, 100))]),
        );
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider));

        let error = handler
            .get_transaction_hashes_by_reference(args())
            .unwrap_err();

        assert_eq!(error.code(), -32000);
        assert_eq!(error.message(), "reference index is behind");
    }

    #[test]
    fn passes_the_exact_head_timestamp_to_the_index() {
        let hash = B256::repeat_byte(0x11);
        let (_dir, handle) = synced_handle(200, 199, hash);
        let provider = TestProvider::new(
            [ChainInfo {
                best_hash: hash,
                best_number: 0,
            }],
            BTreeMap::from([(hash, header(0, 200))]),
        );
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider));

        let error = handler
            .get_transaction_hashes_by_reference(args())
            .unwrap_err();

        assert_eq!(error.code(), -32000);
        assert_eq!(error.message(), "reference index is behind");
    }

    #[test]
    fn maps_unavailable_to_an_explicit_server_error() {
        let error = to_rpc_error(ReferenceIndexError::IndexUnavailable);

        assert_eq!(error.code(), -32000);
        assert_eq!(error.message(), "reference index is unavailable");
    }
}
