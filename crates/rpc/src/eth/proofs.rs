//! Historical EIP-1186 proof RPC override.

use std::{sync::LazyLock, time::Instant};

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, keccak256};
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_serde::JsonStorageKey;
use async_trait::async_trait;
use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::{ErrorCode, ErrorObject},
};
use morph_proofs::{MorphProofsStorage, MorphProofsStore};
use reth_errors::RethError;
use reth_metrics::{
    Metrics,
    metrics::{Counter, Histogram, Label},
};
use reth_provider::StateProofProvider;
use reth_rpc_api::eth::helpers::FullEthApi;
use reth_rpc_eth_api::FromEthApiError;
use reth_rpc_eth_types::EthApiError;
use reth_trie_common::MultiProofTargets;

use crate::state::MorphProofStateProviderFactory;

/// Maximum storage keys accepted by one proof request.
///
/// Matches go-ethereum's `eth_getProof` cap, so it stays a constant rather than a CLI knob.
pub const MAX_PROOF_KEYS: usize = 1024;

/// Default maximum account targets accepted by one `eth_getMultiProof` request.
///
/// Chosen so neither request dimension dominates proof cost, and so realistic demand fits in one
/// round trip: Morph mainnet blocks touch at most ~36 accounts, while a Hoodi load test peaked
/// near ~200 accounts in a single block. Override with
/// `--proofs-history.max-multi-proof-targets`.
pub const DEFAULT_MAX_MULTI_PROOF_TARGETS: usize = 256;

/// Validates request sizes before any database work starts.
#[derive(Debug)]
pub struct ProofKeyLimit;

impl ProofKeyLimit {
    /// Rejects requests above [`MAX_PROOF_KEYS`].
    pub fn check(keys_len: usize) -> Result<(), ErrorObject<'static>> {
        if keys_len > MAX_PROOF_KEYS {
            return Err(ErrorObject::owned(
                ErrorCode::InvalidParams.code(),
                format!("too many storage keys: max {MAX_PROOF_KEYS}, got {keys_len}"),
                None::<()>,
            ));
        }
        Ok(())
    }

    /// Rejects a multiproof batch that exceeds `max_targets` accounts or [`MAX_PROOF_KEYS`] keys.
    ///
    /// The two dimensions are counted separately because an account target costs several times a
    /// storage slot: every account target retains its own account-trie path and opens a
    /// storage-trie cursor, while slots share one already-open trie. A single combined budget
    /// would therefore let an all-accounts batch cost several times an all-slots batch of the
    /// same nominal size.
    ///
    /// Duplicate addresses are charged once per occurrence even though proof generation
    /// consolidates them. That over-counts, which is the safe direction for a pre-flight check.
    pub fn check_multi(
        targets: &[(Address, Vec<B256>)],
        max_targets: usize,
    ) -> Result<(), ErrorObject<'static>> {
        if targets.len() > max_targets {
            return Err(ErrorObject::owned(
                ErrorCode::InvalidParams.code(),
                format!(
                    "too many proof targets: max {max_targets}, got {}",
                    targets.len()
                ),
                None::<()>,
            ));
        }

        let storage_keys = targets
            .iter()
            .try_fold(0usize, |total, (_, slots)| total.checked_add(slots.len()))
            .unwrap_or(usize::MAX);
        if storage_keys > MAX_PROOF_KEYS {
            return Err(ErrorObject::owned(
                ErrorCode::InvalidParams.code(),
                format!("too many storage keys: max {MAX_PROOF_KEYS}, got {storage_keys}"),
                None::<()>,
            ));
        }
        Ok(())
    }
}

/// Per-method counters, distinguished by a `method` label rather than by metric name.
///
/// One name per measurement lets a dashboard split by method or sum across both; separate
/// `get_proof_*` / `get_multi_proof_*` names would allow neither without string surgery.
#[derive(Metrics, Clone)]
#[metrics(scope = "morph.rpc.proofs")]
pub(crate) struct ProofRpcMetrics {
    /// Requests received, counted before the request-size limits are applied.
    requests_total: Counter,
    /// Requests refused before any proof computation started, by a request-size limit or by an
    /// unresolvable block.
    rejected_total: Counter,
    /// Requests answered with a proof.
    successful_responses_total: Counter,
    /// Requests that failed after passing the request-size limits.
    failures_total: Counter,
    /// Successful request latency in seconds.
    latency_seconds: Histogram,
}

// `new_with_labels` re-registers on every call, unlike the cached `default()`, so build each
// method's handles once and share them.
static GET_PROOF_METRICS: LazyLock<ProofRpcMetrics> =
    LazyLock::new(|| ProofRpcMetrics::new_with_labels(vec![Label::new("method", "eth_getProof")]));
static GET_MULTI_PROOF_METRICS: LazyLock<ProofRpcMetrics> = LazyLock::new(|| {
    ProofRpcMetrics::new_with_labels(vec![Label::new("method", "eth_getMultiProof")])
});

impl ProofRpcMetrics {
    /// Counts a request refused before the proof computation started.
    ///
    /// Kept separate from `failures_total` so that
    /// `requests_total == rejected_total + successful_responses_total + failures_total` holds;
    /// previously a rejected request was counted nowhere at all.
    pub(crate) fn record_rejection(&self) {
        self.rejected_total.increment(1);
    }

    /// Counts a received request, before any limit is applied.
    pub(crate) fn record_request(&self) {
        self.requests_total.increment(1);
    }

    /// Records the outcome of a request that passed the size limits.
    ///
    /// Latency is only sampled on success, so a flood of failures cannot skew the histogram.
    pub(crate) fn record_response<T>(&self, start: Instant, result: &RpcResult<T>) {
        match result {
            Ok(_) => {
                self.latency_seconds.record(start.elapsed().as_secs_f64());
                self.successful_responses_total.increment(1);
            }
            Err(_) => self.failures_total.increment(1),
        }
    }
}

/// Runs a proof computation on the blocking pool.
///
/// Proof generation walks MDBX and rebuilds trie nodes synchronously with no await points, so
/// leaving it on a runtime worker would let a handful of concurrent requests starve the Engine API.
pub(crate) async fn spawn_proof_task<Eth, T, F>(eth_api: Eth, task: F) -> RpcResult<T>
where
    Eth: FullEthApi + Send + Sync + 'static,
    F: FnOnce() -> Result<T, Eth::Error> + Send + 'static,
    T: Send + 'static,
{
    let permit = eth_api
        .acquire_owned_tracing()
        .await
        .map_err(RethError::other)
        .map_err(EthApiError::Internal)
        .map_err(Eth::Error::from)
        .map_err(Into::into)?;

    eth_api
        .spawn_blocking_io(move |_| {
            // Keep the shared proof permit until the blocking computation itself has stopped,
            // even if the requesting RPC future is cancelled while this task is running.
            let _permit = permit;
            task()
        })
        .await
        .map_err(Into::into)
}

fn multiproof_targets(targets: &[(Address, Vec<B256>)]) -> MultiProofTargets {
    let mut proof_targets = MultiProofTargets::with_capacity(targets.len());
    for (address, slots) in targets {
        proof_targets
            .entry(keccak256(address))
            .or_default()
            .extend(slots.iter().map(keccak256));
    }
    proof_targets
}

#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthProofApiOverride {
    /// Returns EIP-1186 account and storage proofs at a retained canonical block.
    #[method(name = "getProof")]
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse>;

    /// Returns EIP-1186 proofs for multiple targets at a retained canonical block.
    ///
    /// Storage keys are full 32-byte values, not the short form `eth_getProof` accepts.
    ///
    /// Duplicate addresses are consolidated for proof generation and expanded back into their
    /// original request order, each response carrying only the slots its own target requested.
    ///
    /// An empty batch still resolves and range-checks `block`, so it fails the same way a
    /// non-empty batch would outside the retained window; it returns an empty response only once
    /// the block is accepted.
    #[method(name = "getMultiProof")]
    async fn get_multi_proof(
        &self,
        targets: Vec<(Address, Vec<B256>)>,
        block: Option<BlockId>,
    ) -> RpcResult<Vec<EIP1186AccountProofResponse>>;
}

/// Historical proof RPC implementation backed exclusively by Morph proof history.
#[derive(Debug)]
pub struct EthProofApiExt<Eth, P> {
    state_provider_factory: MorphProofStateProviderFactory<Eth, P>,
    max_multi_proof_targets: usize,
}

impl<Eth, P> EthProofApiExt<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    P: MorphProofsStore + Clone + 'static,
{
    /// Creates the historical proof RPC override.
    pub const fn new(
        eth_api: Eth,
        storage: MorphProofsStorage<P>,
        max_multi_proof_targets: usize,
    ) -> Self {
        Self {
            state_provider_factory: MorphProofStateProviderFactory::new(eth_api, storage),
            max_multi_proof_targets,
        }
    }
}

#[async_trait]
impl<Eth, P> EthProofApiOverrideServer for EthProofApiExt<Eth, P>
where
    Eth: FullEthApi + Clone + Send + Sync + 'static,
    P: MorphProofsStore + Clone + 'static,
{
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse> {
        let metrics = &*GET_PROOF_METRICS;
        let start = Instant::now();
        metrics.record_request();
        if let Err(error) = ProofKeyLimit::check(keys.len()) {
            metrics.record_rejection();
            return Err(error);
        }

        let factory = self.state_provider_factory.clone();
        let eth_api = factory.eth_api().clone();
        let result = spawn_proof_task(eth_api, move || {
            let storage_keys = keys.iter().map(JsonStorageKey::as_b256).collect::<Vec<_>>();
            let proof = factory
                .state_provider(block)
                .map_err(Eth::Error::from_eth_err)?
                .proof(Default::default(), address, &storage_keys)
                .map_err(Eth::Error::from_eth_err)?;
            Ok(proof.into_eip1186_response(keys))
        })
        .await;

        metrics.record_response(start, &result);
        result
    }

    async fn get_multi_proof(
        &self,
        targets: Vec<(Address, Vec<B256>)>,
        block: Option<BlockId>,
    ) -> RpcResult<Vec<EIP1186AccountProofResponse>> {
        let metrics = &*GET_MULTI_PROOF_METRICS;
        let start = Instant::now();
        metrics.record_request();
        if let Err(error) = ProofKeyLimit::check_multi(&targets, self.max_multi_proof_targets) {
            metrics.record_rejection();
            return Err(error);
        }

        let factory = self.state_provider_factory.clone();
        let eth_api = factory.eth_api().clone();
        let result = spawn_proof_task(eth_api, move || {
            let state = factory
                .state_provider(block)
                .map_err(Eth::Error::from_eth_err)?;
            let multiproof = state
                .multiproof(Default::default(), multiproof_targets(&targets))
                .map_err(Eth::Error::from_eth_err)?;

            targets
                .into_iter()
                .map(|(address, slots)| {
                    let proof = multiproof
                        .account_proof(address, &slots)
                        .map_err(RethError::other)
                        .map_err(EthApiError::Internal)
                        .map_err(Eth::Error::from)?;
                    let storage_keys = slots.into_iter().map(JsonStorageKey::from).collect();
                    Ok(proof.into_eip1186_response(storage_keys))
                })
                .collect()
        })
        .await;

        metrics.record_response(start, &result);
        result
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use alloy_primitives::{Address, B256, U256, keccak256};
    use jsonrpsee::types::ErrorCode;

    use super::{
        DEFAULT_MAX_MULTI_PROOF_TARGETS, GET_MULTI_PROOF_METRICS, GET_PROOF_METRICS,
        MAX_PROOF_KEYS, ProofKeyLimit, multiproof_targets,
    };

    #[test]
    fn builds_per_method_metric_handles() {
        // Unlike the cached `default()`, `new_with_labels` runs registration lazily on first use,
        // so a bad label or scope would otherwise only panic while serving a real request.
        LazyLock::force(&GET_PROOF_METRICS);
        LazyLock::force(&GET_MULTI_PROOF_METRICS);
    }

    fn empty_targets(count: usize) -> Vec<(Address, Vec<B256>)> {
        (0..count)
            .map(|index| {
                (
                    Address::from_word(B256::from(U256::from(index))),
                    Vec::<B256>::new(),
                )
            })
            .collect()
    }

    #[test]
    fn enforces_get_proof_key_limit() {
        assert!(ProofKeyLimit::check(MAX_PROOF_KEYS).is_ok());
        let error = ProofKeyLimit::check(MAX_PROOF_KEYS + 1).expect_err("must reject");
        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
    }

    #[test]
    fn enforces_get_multi_proof_storage_key_limit() {
        let at_limit = [
            (Address::ZERO, vec![B256::ZERO; 512]),
            (Address::repeat_byte(1), vec![B256::ZERO; 512]),
        ];
        assert!(ProofKeyLimit::check_multi(&at_limit, DEFAULT_MAX_MULTI_PROOF_TARGETS).is_ok());

        let over_limit = [
            (Address::ZERO, vec![B256::ZERO; 512]),
            (Address::repeat_byte(1), vec![B256::ZERO; 513]),
        ];
        let error = ProofKeyLimit::check_multi(&over_limit, DEFAULT_MAX_MULTI_PROOF_TARGETS)
            .expect_err("must reject");
        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert!(error.message().contains("too many storage keys"));
        assert!(error.message().contains("got 1025"));
    }

    #[test]
    fn enforces_get_multi_proof_target_limit() {
        let at_limit = empty_targets(DEFAULT_MAX_MULTI_PROOF_TARGETS);
        assert!(ProofKeyLimit::check_multi(&at_limit, DEFAULT_MAX_MULTI_PROOF_TARGETS).is_ok());

        let over_limit = empty_targets(DEFAULT_MAX_MULTI_PROOF_TARGETS + 1);
        let error = ProofKeyLimit::check_multi(&over_limit, DEFAULT_MAX_MULTI_PROOF_TARGETS)
            .expect_err("must reject");
        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert!(error.message().contains("too many proof targets"));
        assert!(
            error
                .message()
                .contains(&format!("got {}", DEFAULT_MAX_MULTI_PROOF_TARGETS + 1))
        );
    }

    #[test]
    fn honors_a_configured_target_limit_below_the_default() {
        // The configured value, not the default, decides: 8 targets pass at 8 and fail at 7.
        let targets = empty_targets(8);
        assert!(ProofKeyLimit::check_multi(&targets, 8).is_ok());
        let error = ProofKeyLimit::check_multi(&targets, 7).expect_err("must reject");
        assert!(error.message().contains("max 7, got 8"));
    }

    #[test]
    fn charges_duplicate_addresses_once_per_occurrence() {
        // Consolidation happens during proof generation, but the pre-flight check counts entries,
        // so a duplicated address still consumes two target slots.
        let address = Address::repeat_byte(0x11);
        let targets = [(address, Vec::<B256>::new()), (address, Vec::<B256>::new())];
        assert!(ProofKeyLimit::check_multi(&targets, 2).is_ok());
        assert!(ProofKeyLimit::check_multi(&targets, 1).is_err());
    }

    #[test]
    fn consolidates_duplicate_targets_for_proof_generation() {
        let address = Address::repeat_byte(0x11);
        let slot_a = B256::repeat_byte(0x22);
        let slot_b = B256::repeat_byte(0x33);
        let targets =
            multiproof_targets(&[(address, vec![slot_a]), (address, vec![slot_b, slot_a])]);

        assert_eq!(targets.len(), 1);
        let slots = targets
            .get(&keccak256(address))
            .expect("consolidated account target");
        assert_eq!(slots.len(), 2);
        assert!(slots.contains(&keccak256(slot_a)));
        assert!(slots.contains(&keccak256(slot_b)));
    }

    #[test]
    fn accepts_an_empty_multi_proof_batch() {
        assert!(multiproof_targets(&[]).is_empty());
        assert!(ProofKeyLimit::check_multi(&[], DEFAULT_MAX_MULTI_PROOF_TARGETS).is_ok());
        // Even a zero limit admits an empty batch, so the block check still runs.
        assert!(ProofKeyLimit::check_multi(&[], 0).is_ok());
    }
}
