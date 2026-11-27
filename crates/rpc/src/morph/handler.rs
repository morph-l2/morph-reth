//! `MorphRpc` handler implementation.

use crate::morph::rpc::{MorphRpcServer, ReferenceQueryArgs};
use alloy_consensus::BlockHeader as _;
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObjectOwned},
};
use morph_reference_index::{
    CanonicalTip, ReferenceIndexError, ReferenceIndexHandle, ReferenceIndexPhase,
    ReferenceIndexProgress, ReferenceQuery, ReferenceTransactionResult,
};
use reth_storage_api::{BlockNumReader, HeaderProvider};
use std::time::Duration;
use tracing;

const TARGET: &str = "morph::reference_index_rpc";

/// Upper bound on how long a query waits for the asynchronous index runtime to
/// catch up to the canonical tip before returning `IndexBehind`. The steady-state
/// lag is a few milliseconds, so the vast majority of waits resolve well within
/// this budget and return real data instead of forcing the client to retry.
const INDEX_WAIT_TIMEOUT: Duration = Duration::from_millis(300);

/// Maximum block lag for which the RPC handler waits asynchronously for the
/// reference index to catch up. When the durable cursor lags further behind the
/// canonical tip (for example during large historical backfill), the handler
/// fails fast with `IndexBehind` instead of holding an async waiter open.
const MAX_STEADY_STATE_LAG: u64 = 2;

fn should_fail_fast(progress: ReferenceIndexProgress, canonical_tip_number: u64) -> bool {
    if matches!(
        progress.phase,
        ReferenceIndexPhase::Deferred
            | ReferenceIndexPhase::PreJade
            | ReferenceIndexPhase::Repairing
    ) {
        return true;
    }

    if let Some(indexed_to) = progress.indexed_to
        && canonical_tip_number.saturating_sub(indexed_to) > MAX_STEADY_STATE_LAG
    {
        return true;
    }

    false
}

enum QueryAttempt {
    Ready(Vec<ReferenceTransactionResult>),
    TipChanged,
    Behind { tip_number: u64 },
}

fn query_once<Provider>(
    ctx: &MorphRpc<Provider>,
    query: ReferenceQuery,
) -> Result<QueryAttempt, ReferenceIndexError>
where
    Provider: BlockNumReader + HeaderProvider,
{
    let before = ctx.provider.chain_info()?;
    let header = ctx
        .provider
        .header(before.best_hash)?
        .ok_or(ReferenceIndexError::IndexBehind)?;
    if header.number() != before.best_number {
        return Err(ReferenceIndexError::IndexBehind);
    }
    let canonical_tip = CanonicalTip {
        number: before.best_number,
        hash: before.best_hash,
        timestamp: header.timestamp(),
    };

    // Pre-Jade is a complete, terminal empty answer: no reference-carrying
    // transaction can exist before Jade, so return `[]` without touching
    // the index. This must not be `IndexBehind` (which instructs clients
    // to retry) — before Jade there is nothing to wait for.
    if ctx.reference_index.is_pre_jade(canonical_tip.timestamp) {
        let after = ctx.provider.chain_info()?;
        if after == before {
            return Ok(QueryAttempt::Ready(Vec::new()));
        }
        // The pre-Jade snapshot changed while it was bracketed. Do not
        // query the index with the stale tip: rebuild a fresh canonical
        // snapshot on the next iteration.
        return Ok(QueryAttempt::TipChanged);
    }

    match ctx.reference_index.query_at(query, canonical_tip) {
        Ok(result) => {
            let after = ctx.provider.chain_info()?;
            if after == before {
                Ok(QueryAttempt::Ready(result))
            } else {
                // The tip advanced while we read; this snapshot no longer
                // matches a stable head. Retry against the new tip while the
                // wait budget allows, otherwise report behind.
                Ok(QueryAttempt::TipChanged)
            }
        }
        Err(ReferenceIndexError::IndexBehind) => Ok(QueryAttempt::Behind {
            tip_number: canonical_tip.number,
        }),
        Err(other) => Err(other),
    }
}

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

#[cfg(test)]
#[derive(Clone)]
struct WaitObserver(std::sync::Arc<dyn Fn(bool) + Send + Sync>);

#[cfg(test)]
impl std::fmt::Debug for WaitObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WaitObserver")
    }
}

/// Handler that wraps [`MorphRpc`] and implements the jsonrpsee server trait.
#[derive(Debug, Clone)]
pub struct MorphRpcHandler<Provider> {
    ctx: MorphRpc<Provider>,
    #[cfg(test)]
    wait_observer: Option<WaitObserver>,
}

impl<Provider> MorphRpcHandler<Provider> {
    pub const fn new(ctx: MorphRpc<Provider>) -> Self {
        Self {
            ctx,
            #[cfg(test)]
            wait_observer: None,
        }
    }

    fn observe_rpc_wait(&self, timed_out: bool) {
        self.ctx.reference_index.observe_rpc_wait(timed_out);
        #[cfg(test)]
        if let Some(observer) = &self.wait_observer {
            (observer.0)(timed_out);
        }
    }

    #[cfg(test)]
    fn with_wait_observer(mut self, observer: impl Fn(bool) + Send + Sync + 'static) -> Self {
        self.wait_observer = Some(WaitObserver(std::sync::Arc::new(observer)));
        self
    }
}

struct RpcWaitGuard<'a, Provider> {
    handler: &'a MorphRpcHandler<Provider>,
    waited: bool,
    timed_out: bool,
}

impl<'a, Provider> RpcWaitGuard<'a, Provider> {
    fn new(handler: &'a MorphRpcHandler<Provider>) -> Self {
        Self {
            handler,
            waited: false,
            timed_out: false,
        }
    }

    const fn begin_wait(&mut self) {
        self.waited = true;
    }

    const fn mark_timed_out(&mut self) {
        self.timed_out = true;
    }
}

impl<Provider> Drop for RpcWaitGuard<'_, Provider> {
    fn drop(&mut self) {
        if self.waited {
            self.handler.observe_rpc_wait(self.timed_out);
        }
    }
}

#[async_trait::async_trait]
impl<Provider> MorphRpcServer for MorphRpcHandler<Provider>
where
    Provider: BlockNumReader + HeaderProvider + Clone + Send + Sync + 'static,
{
    async fn get_transaction_hashes_by_reference(
        &self,
        args: ReferenceQueryArgs,
    ) -> RpcResult<Vec<ReferenceTransactionResult>> {
        let query =
            ReferenceQuery::new(args.reference, args.offset, args.limit).map_err(to_rpc_error)?;

        // The index runtime is asynchronous (PR #141 decoupled it from block
        // execution), so for a few milliseconds after a new block becomes the
        // canonical tip the durable cursor still lags it. Run each synchronous
        // provider + MDBX read on Tokio's blocking pool, and park asynchronously
        // on the runtime's `watch` progress channel when waiting for steady-state
        // catch-up — holding zero OS blocking threads and opening zero MDBX read
        // transactions while waiting.
        let deadline = tokio::time::Instant::now() + INDEX_WAIT_TIMEOUT;
        let mut wait = RpcWaitGuard::new(self);
        let mut progress_rx = self.ctx.reference_index.subscribe_progress();
        let mut watch_closed = false;

        loop {
            // Mark the latest published progress as seen BEFORE running the
            // blocking read attempt so any commit/phase change occurring while
            // `query_once` executes immediately wakes `progress_rx.changed()`.
            let _ = *progress_rx.borrow_and_update();

            let ctx = self.ctx.clone();
            let attempt = tokio::task::spawn_blocking(move || query_once(&ctx, query))
                .await
                .map_err(|err| {
                    to_rpc_error(ReferenceIndexError::Other(eyre::eyre!(
                        "reference index query task failed: {err}"
                    )))
                })?
                .map_err(to_rpc_error)?;

            match attempt {
                QueryAttempt::Ready(results) => return Ok(results),
                QueryAttempt::TipChanged => {
                    if tokio::time::Instant::now() >= deadline {
                        wait.begin_wait();
                        wait.mark_timed_out();
                        return Err(to_rpc_error(ReferenceIndexError::IndexBehind));
                    }
                    wait.begin_wait();
                    tokio::task::yield_now().await;
                }
                QueryAttempt::Behind { tip_number } => {
                    let progress = self.ctx.reference_index.progress();
                    if progress.unavailable {
                        return Err(to_rpc_error(ReferenceIndexError::IndexUnavailable));
                    }
                    if should_fail_fast(progress, tip_number) || watch_closed {
                        return Err(to_rpc_error(ReferenceIndexError::IndexBehind));
                    }

                    wait.begin_wait();
                    loop {
                        match tokio::time::timeout_at(deadline, progress_rx.changed()).await {
                            Ok(Ok(())) => {
                                let updated = self.ctx.reference_index.progress();
                                if updated.unavailable {
                                    return Err(to_rpc_error(
                                        ReferenceIndexError::IndexUnavailable,
                                    ));
                                }
                                if matches!(
                                    updated.phase,
                                    ReferenceIndexPhase::Deferred | ReferenceIndexPhase::Repairing
                                ) || updated.indexed_to.is_some_and(|cur| {
                                    tip_number.saturating_sub(cur) > MAX_STEADY_STATE_LAG
                                }) {
                                    return Err(to_rpc_error(ReferenceIndexError::IndexBehind));
                                }
                                // Entering `Backfill` notifies before the batch commits; if
                                // the durable cursor is still strictly below `tip_number`,
                                // keep waiting for the actual commit instead of opening a
                                // doomed MDBX read transaction.
                                if updated.phase == ReferenceIndexPhase::Backfill
                                    && updated.indexed_to.is_some_and(|cur| cur < tip_number)
                                {
                                    continue;
                                }
                                break;
                            }
                            Ok(Err(_closed)) => {
                                // Runtime shut down (watch sender dropped): perform one
                                // final recheck in case a terminal commit or `Unavailable`
                                // transition happened immediately before shutdown, then exit.
                                watch_closed = true;
                                break;
                            }
                            Err(_elapsed) => {
                                wait.mark_timed_out();
                                return Err(to_rpc_error(ReferenceIndexError::IndexBehind));
                            }
                        }
                    }
                }
            }
        }
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
        sync::{
            Arc, Barrier, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
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

        fn finalized_block_number(&self) -> Result<Option<u64>, ReferenceIndexError> {
            Ok(None)
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
    struct GrowingTestChain {
        blocks: Arc<Mutex<Vec<CanonicalBlock>>>,
    }

    impl GrowingTestChain {
        fn new(block: CanonicalBlock) -> Self {
            Self {
                blocks: Arc::new(Mutex::new(vec![block])),
            }
        }

        fn push(&self, block: CanonicalBlock) {
            self.blocks.lock().unwrap().push(block);
        }
    }

    impl CanonicalChain for GrowingTestChain {
        fn head(&self) -> Result<CanonicalTip, ReferenceIndexError> {
            let blocks = self.blocks.lock().unwrap();
            let block = blocks.last().unwrap();
            Ok(CanonicalTip {
                number: block.number,
                hash: block.hash,
                timestamp: block.timestamp,
            })
        }

        fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ReferenceIndexError> {
            Ok(self
                .blocks
                .lock()
                .unwrap()
                .iter()
                .find_map(|block| (block.number == number).then_some(block.hash)))
        }

        fn block_timestamp(&self, number: u64) -> Result<Option<u64>, ReferenceIndexError> {
            Ok(self
                .blocks
                .lock()
                .unwrap()
                .iter()
                .find_map(|block| (block.number == number).then_some(block.timestamp)))
        }

        fn finalized_block_number(&self) -> Result<Option<u64>, ReferenceIndexError> {
            Ok(None)
        }

        fn canonical_blocks(
            &self,
            range: RangeInclusive<u64>,
        ) -> Result<Vec<CanonicalBlock>, ReferenceIndexError> {
            Ok(self
                .blocks
                .lock()
                .unwrap()
                .iter()
                .filter(|block| range.contains(&block.number))
                .cloned()
                .collect())
        }
    }

    #[derive(Clone, Debug)]
    struct TestProvider {
        chain_info: Arc<Mutex<VecDeque<ChainInfo>>>,
        headers: Arc<BTreeMap<B256, MorphHeader>>,
        chain_info_calls: Arc<AtomicUsize>,
    }

    impl TestProvider {
        fn new(
            chain_info: impl IntoIterator<Item = ChainInfo>,
            headers: BTreeMap<B256, MorphHeader>,
        ) -> Self {
            Self {
                chain_info: Arc::new(Mutex::new(chain_info.into_iter().collect())),
                headers: Arc::new(headers),
                chain_info_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn chain_info_calls(&self) -> usize {
            self.chain_info_calls.load(Ordering::Acquire)
        }

        fn info(&self) -> ChainInfo {
            self.chain_info_calls.fetch_add(1, Ordering::AcqRel);
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

    #[tokio::test]
    async fn rejects_results_if_canonical_head_changes_during_query() {
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
            .await
            .unwrap_err();

        assert_eq!(error.code(), -32000);
        assert_eq!(error.message(), "reference index is behind");
    }

    #[tokio::test]
    async fn passes_the_exact_head_timestamp_to_the_index() {
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
            .await
            .unwrap_err();

        assert_eq!(error.code(), -32000);
        assert_eq!(error.message(), "reference index is behind");
    }

    #[tokio::test]
    async fn pre_jade_empty_result_is_rejected_if_head_crosses_jade() {
        let pre_jade_hash = B256::repeat_byte(0x11);
        let jade_hash = B256::repeat_byte(0x22);
        let (_dir, handle) = synced_handle(200, 199, pre_jade_hash);
        let provider = TestProvider::new(
            [
                ChainInfo {
                    best_hash: pre_jade_hash,
                    best_number: 0,
                },
                ChainInfo {
                    best_hash: jade_hash,
                    best_number: 1,
                },
            ],
            BTreeMap::from([(pre_jade_hash, header(0, 199)), (jade_hash, header(1, 200))]),
        );
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider));

        let error = handler
            .get_transaction_hashes_by_reference(args())
            .await
            .expect_err("a head change across Jade must invalidate the empty snapshot");

        assert_eq!(error.code(), -32000);
        assert_eq!(error.message(), "reference index is behind");
    }

    #[tokio::test]
    async fn changed_pre_jade_snapshot_retries_without_querying_the_opening_index() {
        let first_hash = B256::repeat_byte(0x11);
        let second_hash = B256::repeat_byte(0x22);
        let handle = ReferenceIndexHandle::new(200);
        let provider = TestProvider::new(
            [
                ChainInfo {
                    best_hash: first_hash,
                    best_number: 0,
                },
                ChainInfo {
                    best_hash: second_hash,
                    best_number: 1,
                },
                ChainInfo {
                    best_hash: second_hash,
                    best_number: 1,
                },
            ],
            BTreeMap::from([(first_hash, header(0, 100)), (second_hash, header(1, 101))]),
        );
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider));

        let results = handler
            .get_transaction_hashes_by_reference(args())
            .await
            .unwrap();

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn rapid_sync_deferred_fails_fast_without_burning_the_wait_budget() {
        // Rapid-sync `Deferred`: a post-Jade query must return `IndexBehind`
        // promptly rather than waiting out the full `INDEX_WAIT_TIMEOUT`, since
        // the runtime will not catch up while indexing is intentionally paused.
        let dir = tempfile::tempdir().unwrap();
        let hash = B256::repeat_byte(0x11);
        let chain = TestChain {
            block: CanonicalBlock {
                number: 0,
                hash,
                timestamp: 100,
                transactions: Vec::new(),
            },
        };
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 0);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain);
        runtime.synchronize_once(true).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Deferred);

        let provider = TestProvider::new(
            [ChainInfo {
                best_hash: hash,
                best_number: 0,
            }],
            BTreeMap::from([(hash, header(0, 100))]),
        );
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let observed = outcomes.clone();
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider.clone()))
            .with_wait_observer(move |timed_out| {
                observed.lock().unwrap().push(timed_out);
            });

        let started = std::time::Instant::now();
        let error = handler
            .get_transaction_hashes_by_reference(args())
            .await
            .unwrap_err();
        drop(runtime);

        assert_eq!(error.message(), "reference index is behind");
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(outcomes.lock().unwrap().is_empty());
        assert_eq!(provider.chain_info_calls(), 1);
    }

    #[test]
    fn should_fail_fast_classifies_phases_and_steady_state_lag() {
        let progress = |phase, indexed_to| ReferenceIndexProgress {
            phase,
            indexed_to,
            unavailable: phase == ReferenceIndexPhase::Unavailable,
        };

        assert!(should_fail_fast(
            progress(ReferenceIndexPhase::Deferred, Some(10)),
            10
        ));
        assert!(should_fail_fast(
            progress(ReferenceIndexPhase::PreJade, None),
            10
        ));
        assert!(should_fail_fast(
            progress(ReferenceIndexPhase::Repairing, Some(10)),
            10
        ));

        // Steady-state lag <= MAX_STEADY_STATE_LAG (2) is allowed to wait.
        assert!(!should_fail_fast(
            progress(ReferenceIndexPhase::Live, Some(10)),
            11
        ));
        assert!(!should_fail_fast(
            progress(ReferenceIndexPhase::Backfill, Some(10)),
            12
        ));
        assert!(!should_fail_fast(
            progress(ReferenceIndexPhase::Backfill, None),
            1
        ));

        // Lag > MAX_STEADY_STATE_LAG (2) fails fast.
        assert!(should_fail_fast(
            progress(ReferenceIndexPhase::Backfill, Some(10)),
            13
        ));
        assert!(should_fail_fast(
            progress(ReferenceIndexPhase::Live, Some(10)),
            13
        ));
    }

    #[tokio::test]
    async fn live_lag_wait_eventually_returns_behind_without_polling() {
        let dir = tempfile::tempdir().unwrap();
        let indexed_hash = B256::repeat_byte(0x11);
        let head_hash = B256::repeat_byte(0x22);
        let chain = TestChain {
            block: CanonicalBlock {
                number: 0,
                hash: indexed_hash,
                timestamp: 100,
                transactions: Vec::new(),
            },
        };
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 0);
        // Keep `runtime` alive so the watch sender stays open and the wait hits the deadline.
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain);
        runtime.synchronize_once(false).unwrap();

        let provider = TestProvider::new(
            [ChainInfo {
                best_hash: head_hash,
                best_number: 1,
            }],
            BTreeMap::from([(head_hash, header(1, 101))]),
        );
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let observed = outcomes.clone();
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider.clone()))
            .with_wait_observer(move |timed_out| {
                observed.lock().unwrap().push(timed_out);
            });

        let error = handler
            .get_transaction_hashes_by_reference(args())
            .await
            .unwrap_err();
        drop(runtime);

        assert_eq!(error.message(), "reference index is behind");
        assert_eq!(*outcomes.lock().unwrap(), [true]);
        // Only the initial read occurred; no polling reads during the 300ms wait.
        assert_eq!(provider.chain_info_calls(), 1);
    }

    #[tokio::test]
    async fn live_lag_wait_returns_from_the_same_request_after_runtime_catches_up() {
        let dir = tempfile::tempdir().unwrap();
        let indexed_hash = B256::repeat_byte(0x11);
        let head_hash = B256::repeat_byte(0x22);
        let chain = GrowingTestChain::new(CanonicalBlock {
            number: 0,
            hash: indexed_hash,
            timestamp: 100,
            transactions: Vec::new(),
        });
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 0);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        runtime.synchronize_once(false).unwrap();
        chain.push(CanonicalBlock {
            number: 1,
            hash: head_hash,
            timestamp: 101,
            transactions: Vec::new(),
        });

        let provider = TestProvider::new(
            [ChainInfo {
                best_hash: head_hash,
                best_number: 1,
            }],
            BTreeMap::from([(head_hash, header(1, 101))]),
        );
        let catch_up = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            tokio::task::spawn_blocking(move || {
                runtime.synchronize_once(false).unwrap();
                runtime
            })
            .await
            .unwrap()
        });
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider.clone()));

        let results = handler
            .get_transaction_hashes_by_reference(args())
            .await
            .unwrap();
        let _runtime = catch_up.await.unwrap();

        assert!(results.is_empty());
        // Attempt 1 (`before`) + Attempt 2 (`before` + `after`) = 3 calls; zero polling.
        assert_eq!(provider.chain_info_calls(), 3);
    }

    #[tokio::test]
    async fn steady_state_lag_of_two_waits_and_succeeds_without_polling() {
        let dir = tempfile::tempdir().unwrap();
        let hash0 = B256::repeat_byte(0x10);
        let hash1 = B256::repeat_byte(0x11);
        let hash2 = B256::repeat_byte(0x12);
        let chain = GrowingTestChain::new(CanonicalBlock {
            number: 0,
            hash: hash0,
            timestamp: 100,
            transactions: Vec::new(),
        });
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 0);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        runtime.synchronize_once(false).unwrap();
        chain.push(CanonicalBlock {
            number: 1,
            hash: hash1,
            timestamp: 101,
            transactions: Vec::new(),
        });
        chain.push(CanonicalBlock {
            number: 2,
            hash: hash2,
            timestamp: 102,
            transactions: Vec::new(),
        });

        let provider = TestProvider::new(
            [ChainInfo {
                best_hash: hash2,
                best_number: 2,
            }],
            BTreeMap::from([(hash2, header(2, 102))]),
        );
        let catch_up = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            tokio::task::spawn_blocking(move || {
                runtime.synchronize_once(false).unwrap();
                runtime
            })
            .await
            .unwrap()
        });
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider.clone()));

        let results = handler
            .get_transaction_hashes_by_reference(args())
            .await
            .unwrap();
        let _runtime = catch_up.await.unwrap();

        assert!(results.is_empty());
        assert_eq!(provider.chain_info_calls(), 3);
    }

    #[tokio::test]
    async fn backfill_lag_above_two_fails_fast_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let hash0 = B256::repeat_byte(0x10);
        let hash3 = B256::repeat_byte(0x13);
        let chain = TestChain {
            block: CanonicalBlock {
                number: 0,
                hash: hash0,
                timestamp: 100,
                transactions: Vec::new(),
            },
        };
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 0);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain);
        runtime.synchronize_once(false).unwrap();

        let provider = TestProvider::new(
            [ChainInfo {
                best_hash: hash3,
                best_number: 3,
            }],
            BTreeMap::from([(hash3, header(3, 103))]),
        );
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let observed = outcomes.clone();
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider.clone()))
            .with_wait_observer(move |timed_out| {
                observed.lock().unwrap().push(timed_out);
            });

        let started = std::time::Instant::now();
        let error = handler
            .get_transaction_hashes_by_reference(args())
            .await
            .unwrap_err();
        drop(runtime);

        assert_eq!(error.message(), "reference index is behind");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "lag > 2 must fail fast without waiting"
        );
        assert!(outcomes.lock().unwrap().is_empty());
        assert_eq!(provider.chain_info_calls(), 1);
    }

    #[tokio::test]
    async fn repairing_phase_fails_fast_without_waiting() {
        #[derive(Clone, Debug)]
        struct PausingReorgChain {
            blocks: Arc<Mutex<Vec<CanonicalBlock>>>,
            pause_on_hash_of: Arc<Mutex<Option<u64>>>,
            entered_repair: Arc<Barrier>,
            release_repair: Arc<Barrier>,
        }

        impl CanonicalChain for PausingReorgChain {
            fn head(&self) -> Result<CanonicalTip, ReferenceIndexError> {
                let blocks = self.blocks.lock().unwrap();
                let block = blocks.last().unwrap();
                Ok(CanonicalTip {
                    number: block.number,
                    hash: block.hash,
                    timestamp: block.timestamp,
                })
            }

            fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ReferenceIndexError> {
                let should_pause = {
                    let mut target = self.pause_on_hash_of.lock().unwrap();
                    if *target == Some(number) {
                        *target = None;
                        true
                    } else {
                        false
                    }
                };
                if should_pause {
                    self.entered_repair.wait();
                    self.release_repair.wait();
                }
                Ok(self
                    .blocks
                    .lock()
                    .unwrap()
                    .iter()
                    .find_map(|block| (block.number == number).then_some(block.hash)))
            }

            fn block_timestamp(&self, number: u64) -> Result<Option<u64>, ReferenceIndexError> {
                Ok(self
                    .blocks
                    .lock()
                    .unwrap()
                    .iter()
                    .find_map(|block| (block.number == number).then_some(block.timestamp)))
            }

            fn finalized_block_number(&self) -> Result<Option<u64>, ReferenceIndexError> {
                Ok(None)
            }

            fn canonical_blocks(
                &self,
                range: RangeInclusive<u64>,
            ) -> Result<Vec<CanonicalBlock>, ReferenceIndexError> {
                Ok(self
                    .blocks
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|block| range.contains(&block.number))
                    .cloned()
                    .collect())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let entered_repair = Arc::new(Barrier::new(2));
        let release_repair = Arc::new(Barrier::new(2));
        let reorg_hash = B256::repeat_byte(0x99);
        let chain = PausingReorgChain {
            blocks: Arc::new(Mutex::new(vec![
                CanonicalBlock {
                    number: 0,
                    hash: B256::repeat_byte(0x10),
                    timestamp: 100,
                    transactions: Vec::new(),
                },
                CanonicalBlock {
                    number: 1,
                    hash: B256::repeat_byte(0x11),
                    timestamp: 101,
                    transactions: Vec::new(),
                },
            ])),
            pause_on_hash_of: Arc::new(Mutex::new(None)),
            entered_repair: entered_repair.clone(),
            release_repair: release_repair.clone(),
        };
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 0);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        runtime.synchronize_once(false).unwrap();

        // Trigger a reorg on block 1 and pause inside `reconcile_cursor` after
        // `ReferenceIndexPhase::Repairing` has been published.
        chain.blocks.lock().unwrap()[1].hash = reorg_hash;
        *chain.pause_on_hash_of.lock().unwrap() = Some(1);
        let repair_thread = std::thread::spawn(move || {
            runtime.synchronize_once(false).unwrap();
            runtime
        });
        entered_repair.wait();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Repairing);

        let provider = TestProvider::new(
            [ChainInfo {
                best_hash: reorg_hash,
                best_number: 1,
            }],
            BTreeMap::from([(reorg_hash, header(1, 101))]),
        );
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let observed = outcomes.clone();
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider.clone()))
            .with_wait_observer(move |timed_out| {
                observed.lock().unwrap().push(timed_out);
            });

        let started = std::time::Instant::now();
        let error = handler
            .get_transaction_hashes_by_reference(args())
            .await
            .unwrap_err();
        release_repair.wait();
        let _runtime = repair_thread.join().unwrap();

        assert_eq!(error.message(), "reference index is behind");
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(outcomes.lock().unwrap().is_empty());
        assert_eq!(provider.chain_info_calls(), 1);
    }

    #[tokio::test]
    async fn watch_sender_drop_wakes_waiter_and_exits_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let indexed_hash = B256::repeat_byte(0x11);
        let head_hash = B256::repeat_byte(0x22);
        let chain = TestChain {
            block: CanonicalBlock {
                number: 0,
                hash: indexed_hash,
                timestamp: 100,
                transactions: Vec::new(),
            },
        };
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 0);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain);
        runtime.synchronize_once(false).unwrap();

        let provider = TestProvider::new(
            [ChainInfo {
                best_hash: head_hash,
                best_number: 1,
            }],
            BTreeMap::from([(head_hash, header(1, 101))]),
        );
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let observed = outcomes.clone();
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider.clone()))
            .with_wait_observer(move |timed_out| {
                observed.lock().unwrap().push(timed_out);
            });

        // Drop the runtime while the RPC call is awaiting `progress_rx.changed()`.
        let shutdown = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            drop(runtime);
        });

        let started = std::time::Instant::now();
        let error = handler
            .get_transaction_hashes_by_reference(args())
            .await
            .unwrap_err();
        shutdown.await.unwrap();

        assert_eq!(error.message(), "reference index is behind");
        assert!(
            started.elapsed() < Duration::from_millis(180),
            "dropped watch sender must wake and exit before the 300ms timeout (elapsed {:?})",
            started.elapsed()
        );
        assert_eq!(*outcomes.lock().unwrap(), [false]);
        // Initial read + one final recheck after sender drop.
        assert_eq!(provider.chain_info_calls(), 2);
    }

    #[tokio::test]
    async fn live_lag_wait_covers_a_200ms_catch_up() {
        let dir = tempfile::tempdir().unwrap();
        let indexed_hash = B256::repeat_byte(0x11);
        let head_hash = B256::repeat_byte(0x22);
        let chain = GrowingTestChain::new(CanonicalBlock {
            number: 0,
            hash: indexed_hash,
            timestamp: 100,
            transactions: Vec::new(),
        });
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 0);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        runtime.synchronize_once(false).unwrap();
        chain.push(CanonicalBlock {
            number: 1,
            hash: head_hash,
            timestamp: 101,
            transactions: Vec::new(),
        });

        let provider = TestProvider::new(
            [ChainInfo {
                best_hash: head_hash,
                best_number: 1,
            }],
            BTreeMap::from([(head_hash, header(1, 101))]),
        );
        let catch_up = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tokio::task::spawn_blocking(move || {
                runtime.synchronize_once(false).unwrap();
                runtime
            })
            .await
            .unwrap()
        });
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider.clone()));

        let result = handler.get_transaction_hashes_by_reference(args()).await;
        let _runtime = catch_up.await.unwrap();

        assert!(
            result.is_ok(),
            "a live query should wait for a 200ms index catch-up: {result:?}"
        );
        assert_eq!(provider.chain_info_calls(), 3);
    }

    #[tokio::test]
    async fn deferred_transition_after_wait_is_not_counted_as_a_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let indexed_hash = B256::repeat_byte(0x11);
        let head_hash = B256::repeat_byte(0x22);
        let chain = GrowingTestChain::new(CanonicalBlock {
            number: 0,
            hash: indexed_hash,
            timestamp: 100,
            transactions: Vec::new(),
        });
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 0);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        runtime.synchronize_once(false).unwrap();
        chain.push(CanonicalBlock {
            number: 1,
            hash: head_hash,
            timestamp: 101,
            transactions: Vec::new(),
        });

        let provider = TestProvider::new(
            [ChainInfo {
                best_hash: head_hash,
                best_number: 1,
            }],
            BTreeMap::from([(head_hash, header(1, 101))]),
        );
        let transition = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            tokio::task::spawn_blocking(move || {
                runtime.synchronize_once(true).unwrap();
                runtime
            })
            .await
            .unwrap()
        });
        let phase = handle.clone();
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let observed_outcomes = outcomes.clone();
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider.clone()))
            .with_wait_observer(move |timed_out| {
                observed_outcomes.lock().unwrap().push(timed_out);
            });

        let error = handler
            .get_transaction_hashes_by_reference(args())
            .await
            .unwrap_err();
        let _runtime = transition.await.unwrap();

        assert_eq!(error.message(), "reference index is behind");
        assert_eq!(phase.phase(), ReferenceIndexPhase::Deferred);
        assert_eq!(*outcomes.lock().unwrap(), [false]);
        // Woke on watch transition to `Deferred` and exited without another blocking read.
        assert_eq!(provider.chain_info_calls(), 1);
    }

    #[tokio::test]
    async fn provider_failure_after_wait_is_observed_without_a_timeout() {
        let indexed_hash = B256::repeat_byte(0x11);
        let head_hash = B256::repeat_byte(0x22);
        let missing_header_hash = B256::repeat_byte(0x33);
        let (_dir, handle) = synced_handle(0, 100, indexed_hash);
        let provider = TestProvider::new(
            [
                ChainInfo {
                    best_hash: head_hash,
                    best_number: 1,
                },
                ChainInfo {
                    best_hash: missing_header_hash,
                    best_number: 2,
                },
            ],
            BTreeMap::from([(head_hash, header(1, 101))]),
        );
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let observed_outcomes = outcomes.clone();
        let handler = MorphRpcHandler::new(MorphRpc::new(handle, provider)).with_wait_observer(
            move |timed_out| {
                observed_outcomes.lock().unwrap().push(timed_out);
            },
        );

        let error = handler
            .get_transaction_hashes_by_reference(args())
            .await
            .unwrap_err();

        assert_eq!(error.message(), "reference index is behind");
        assert_eq!(*outcomes.lock().unwrap(), [false]);
    }

    #[test]
    fn maps_unavailable_to_an_explicit_server_error() {
        let error = to_rpc_error(ReferenceIndexError::IndexUnavailable);

        assert_eq!(error.code(), -32000);
        assert_eq!(error.message(), "reference index is unavailable");
    }
}
