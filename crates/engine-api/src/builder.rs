//! Morph L2 Engine API implementation.
//!
//! This module provides the concrete Morph L2 Engine API implementation and supporting helpers.

use crate::{
    EngineApiResult, MorphEngineApiError, MorphL2EngineApi, metrics::MorphEngineApiMetrics,
};
use alloy_consensus::{
    BlockHeader, EMPTY_OMMER_ROOT_HASH, Header, proofs::calculate_transaction_root,
};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B64, B256, Sealable};
use alloy_rpc_types_engine::{PayloadAttributes, PayloadStatus, PayloadStatusEnum};
use morph_chainspec::MorphChainSpec;
use morph_payload_types::{
    AssembleL2BlockParams, AssembleL2BlockV2Params, ExecutableL2Data, GenericResponse,
    MorphBuiltPayload, MorphExecutionData, MorphPayloadTypes, SafeL2Data,
};
use morph_primitives::{Block, BlockBody, MorphHeader, MorphTxEnvelope};
use parking_lot::RwLock;
use reth_payload_builder::{BuildNewPayload, PayloadBuilderHandle};
#[cfg(test)]
use reth_primitives_traits::RecoveredBlock;
use reth_primitives_traits::{FastInstant as Instant, SealedBlock, SealedHeader};
use reth_provider::{BlockNumReader, BlockReaderIdExt, CanonChainTracker, HeaderProvider};
use std::sync::Arc;

// =============================================================================
// Real Implementation
// =============================================================================

/// Real implementation of the Morph L2 Engine API.
///
/// This implementation integrates with reth's provider and payload builder service
/// to provide full L2 Engine API functionality for block building, validation, and import.
#[derive(Debug)]
pub struct RealMorphL2EngineApi<Provider> {
    /// Blockchain data provider for state and header access.
    provider: Provider,

    /// Payload builder service handle for constructing new blocks.
    payload_builder: PayloadBuilderHandle<MorphPayloadTypes>,

    /// Chain specification for hardfork rules.
    chain_spec: Arc<MorphChainSpec>,

    /// Handle to the running reth engine tree pipeline.
    engine_handle: reth_node_api::ConsensusEngineHandle<MorphPayloadTypes>,

    /// Tracks L1-derived finalized block tags for FCU updates.
    block_tag_tracker: Arc<BlockTagTracker>,

    /// Prometheus metrics for custom Morph L2 Engine API endpoints and chain head health.
    metrics: MorphEngineApiMetrics,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CanonicalHead {
    number: u64,
    hash: B256,
    timestamp: u64,
}

/// Tracks the L1-derived finalized block hash from `set_block_tags` so that FCU
/// calls can forward it to the engine tree.
///
/// Only finalized is cached. FCU safe is passed by the import caller, not cached or
/// derived from the head, so reorg-capable imports never reuse a stale safe tag.
#[derive(Debug, Default)]
pub struct BlockTagTracker {
    /// Last L1-derived finalized hash from `set_block_tags`. `None` means
    /// `set_block_tags` has not yet provided a value (e.g. a validator not running
    /// BlockTagService, or before the first L1-finalized batch).
    finalized_hash: RwLock<Option<B256>>,
}

impl BlockTagTracker {
    /// Caches the L1-derived finalized hash from a successful `set_block_tags` call.
    /// `None` is ignored, so a previously-supplied finalized is preserved.
    pub fn record_finalized_hash(&self, finalized_hash: Option<B256>) {
        if let Some(h) = finalized_hash {
            *self.finalized_hash.write() = Some(h);
        }
    }

    /// Returns the last L1-derived finalized hash, or `None` if not yet set.
    fn l1_finalized_hash(&self) -> Option<B256> {
        *self.finalized_hash.read()
    }
}

impl<Provider> RealMorphL2EngineApi<Provider> {
    /// Creates a new [`RealMorphL2EngineApi`].
    pub fn new(
        provider: Provider,
        payload_builder: PayloadBuilderHandle<MorphPayloadTypes>,
        chain_spec: Arc<MorphChainSpec>,
        engine_handle: reth_node_api::ConsensusEngineHandle<MorphPayloadTypes>,
        block_tag_tracker: Arc<BlockTagTracker>,
    ) -> Self {
        Self {
            provider,
            payload_builder,
            chain_spec,
            engine_handle,
            block_tag_tracker,
            metrics: MorphEngineApiMetrics::default(),
        }
    }

    /// Updates `head_block_timegap_seconds` gauge after a successful block import.
    fn record_head_metrics(&self, block_timestamp: u64) {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.metrics
            .head_block_timegap_seconds
            .set(now_secs.saturating_sub(block_timestamp) as f64);
    }

    /// Returns a reference to the provider.
    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    /// Returns a reference to the payload builder.
    pub fn payload_builder(&self) -> &PayloadBuilderHandle<MorphPayloadTypes> {
        &self.payload_builder
    }

    /// Returns a reference to the chain spec.
    pub fn chain_spec(&self) -> &MorphChainSpec {
        &self.chain_spec
    }
}

#[async_trait::async_trait]
impl<Provider> MorphL2EngineApi for RealMorphL2EngineApi<Provider>
where
    Provider: HeaderProvider<Header = MorphHeader>
        + BlockNumReader
        + BlockReaderIdExt<Header = MorphHeader>
        + CanonChainTracker<Header = MorphHeader>
        + Clone
        + Send
        + Sync
        + 'static,
{
    async fn assemble_l2_block(
        &self,
        params: AssembleL2BlockParams,
    ) -> EngineApiResult<ExecutableL2Data> {
        let started = Instant::now();
        let result = self.build_l2_payload(params, None, None, None).await;
        self.metrics
            .assemble_l2_block_duration_seconds
            .record(started.elapsed());

        let built_payload = result.inspect_err(|_| {
            self.metrics.assemble_l2_block_failures_total.increment(1);
        })?;
        let executable_data = built_payload.executable_data;

        tracing::debug!(
            target: "morph::engine",
            block_hash = %executable_data.hash,
            gas_used = executable_data.gas_used,
            tx_count = executable_data.transactions.len(),
            "L2 block assembled successfully"
        );

        Ok(executable_data)
    }

    async fn assemble_l2_block_v2(
        &self,
        params: AssembleL2BlockV2Params,
    ) -> EngineApiResult<ExecutableL2Data> {
        let started = Instant::now();
        let parent_hash = params.parent_hash;

        // Derive the block number from the pinned parent (parent + 1). The parent is
        // looked up by hash and need not be the canonical head — that is the point of V2
        // (the sequencer can build on a parent that diverges from its current head).
        let parent = self
            .provider
            .sealed_header_by_hash(parent_hash)
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?
            .ok_or_else(|| {
                MorphEngineApiError::Internal(format!("parent block not found: {parent_hash}"))
            })?;

        let assemble_params = AssembleL2BlockParams {
            number: parent.number() + 1,
            transactions: params.transactions,
            timestamp: params.timestamp,
        };

        let result = self
            .build_l2_payload(assemble_params, None, None, Some(parent_hash))
            .await;
        self.metrics
            .assemble_l2_block_duration_seconds
            .record(started.elapsed());

        let built_payload = result.inspect_err(|_| {
            self.metrics.assemble_l2_block_failures_total.increment(1);
        })?;
        let executable_data = built_payload.executable_data;

        tracing::debug!(
            target: "morph::engine",
            block_hash = %executable_data.hash,
            parent_hash = %parent_hash,
            gas_used = executable_data.gas_used,
            tx_count = executable_data.transactions.len(),
            "L2 block assembled successfully (v2)"
        );

        Ok(executable_data)
    }

    async fn validate_l2_block(&self, data: ExecutableL2Data) -> EngineApiResult<GenericResponse> {
        let validate_started = Instant::now();
        tracing::debug!(
            target: "morph::engine",
            block_number = data.number,
            block_hash = %data.hash,
            "validating L2 block"
        );

        // 1. Enforce canonical continuity against the current head.
        //    Matching go-ethereum: returns error (not GenericResponse{false}) for
        //    discontinuous block number or parent hash mismatch.
        let current_head = self.current_head()?;
        if data.number != current_head.number + 1 {
            tracing::warn!(
                target: "morph::engine",
                expected = current_head.number + 1,
                actual = data.number,
                "cannot validate block with discontinuous block number"
            );
            self.metrics.validate_l2_block_failures_total.increment(1);
            self.metrics
                .validate_l2_block_duration_seconds
                .record(validate_started.elapsed());
            return Err(MorphEngineApiError::DiscontinuousBlockNumber {
                expected: current_head.number + 1,
                actual: data.number,
            });
        }

        if data.parent_hash != current_head.hash {
            tracing::warn!(
                target: "morph::engine",
                expected = %current_head.hash,
                actual = %data.parent_hash,
                "parent hash mismatch"
            );
            self.metrics.validate_l2_block_failures_total.increment(1);
            self.metrics
                .validate_l2_block_duration_seconds
                .record(validate_started.elapsed());
            return Err(MorphEngineApiError::WrongParentHash {
                expected: current_head.hash,
                actual: data.parent_hash,
            });
        }

        // 2. Convert and forward to reth engine tree (`newPayload` path).
        let (payload, _) = match self.execution_payload_from_executable_data(&data) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    target: "morph::engine",
                    block_hash = %data.hash,
                    error = %err,
                    "failed to convert executable data for validation"
                );
                self.metrics.validate_l2_block_failures_total.increment(1);
                self.metrics
                    .validate_l2_block_duration_seconds
                    .record(validate_started.elapsed());
                return Ok(GenericResponse { success: false });
            }
        };

        let status = match self.engine_handle.new_payload(payload).await {
            Ok(status) => status,
            Err(err) => {
                tracing::warn!(
                    target: "morph::engine",
                    block_hash = %data.hash,
                    error = %err,
                    "engine new_payload failed during validate_l2_block"
                );
                self.metrics.validate_l2_block_failures_total.increment(1);
                self.metrics
                    .validate_l2_block_duration_seconds
                    .record(validate_started.elapsed());
                return Ok(GenericResponse { success: false });
            }
        };

        tracing::debug!(
            target: "morph::engine",
            block_hash = %data.hash,
            status = ?status.status,
            "validate_l2_block returned engine payload status"
        );

        let success = payload_status_is_validated(&status);
        self.metrics
            .validate_l2_block_duration_seconds
            .record(validate_started.elapsed());
        if !success {
            self.metrics.validate_l2_block_failures_total.increment(1);
        }

        Ok(GenericResponse { success })
    }

    async fn new_l2_block(&self, data: ExecutableL2Data) -> EngineApiResult<()> {
        let started = Instant::now();
        tracing::debug!(
            target: "morph::engine",
            block_number = data.number,
            block_hash = %data.hash,
            "importing new L2 block"
        );

        // 1. Get current head from blockchain (same as go-ethereum's parent := api.eth.BlockChain().CurrentBlock())
        let current_head = self.current_head()?;
        let current_number = current_head.number;

        let expected_number = current_number + 1;

        // 2. Validate block number (same as go-ethereum's logic)
        if data.number != expected_number {
            if data.number < expected_number {
                // Ignore past blocks (same as go-ethereum)
                tracing::warn!(
                    target: "morph::engine",
                    block_number = data.number,
                    current_number = current_number,
                    "ignoring past block number"
                );
                self.metrics
                    .new_l2_block_duration_seconds
                    .record(started.elapsed());
                return Ok(());
            }
            // Discontinuous block number
            tracing::warn!(
                target: "morph::engine",
                expected_number = expected_number,
                actual_number = data.number,
                "cannot new block with discontinuous block number"
            );
            self.metrics.new_l2_block_failures_total.increment(1);
            self.metrics
                .new_l2_block_duration_seconds
                .record(started.elapsed());
            return Err(MorphEngineApiError::DiscontinuousBlockNumber {
                expected: expected_number,
                actual: data.number,
            });
        }

        // 3. Get current head header and validate parent hash
        if data.parent_hash != current_head.hash {
            tracing::warn!(
                target: "morph::engine",
                expected = %current_head.hash,
                actual = %data.parent_hash,
                "wrong parent hash"
            );
            self.metrics.new_l2_block_failures_total.increment(1);
            self.metrics
                .new_l2_block_duration_seconds
                .record(started.elapsed());
            return Err(MorphEngineApiError::WrongParentHash {
                expected: current_head.hash,
                actual: data.parent_hash,
            });
        }

        let block_hash = data.hash;
        let block_number = data.number;
        let block_timestamp = data.timestamp;
        self.import_l2_block_via_engine(data, B256::ZERO)
            .await
            .inspect_err(|_| {
                self.metrics.new_l2_block_failures_total.increment(1);
                self.metrics
                    .new_l2_block_duration_seconds
                    .record(started.elapsed());
            })?;

        self.metrics
            .new_l2_block_duration_seconds
            .record(started.elapsed());
        self.record_head_metrics(block_timestamp);

        tracing::debug!(
            target: "morph::engine",
            block_hash = %block_hash,
            block_number,
            "L2 block accepted via engine tree"
        );

        Ok(())
    }

    async fn new_l2_block_v2(&self, data: ExecutableL2Data) -> EngineApiResult<MorphHeader> {
        let started = Instant::now();
        tracing::debug!(
            target: "morph::engine",
            block_number = data.number,
            block_hash = %data.hash,
            parent_hash = %data.parent_hash,
            "importing new L2 block (v2, reorg-capable)"
        );

        // 1. Parent selection by hash. Relaxed from V1's "parent must be the current
        //    head" to "parent must exist": when the parent is not the canonical head,
        //    the forkchoice update inside import_l2_block_via_engine reorganizes the
        //    chain onto this block. This is the centralized-sequencer import path,
        //    where the sequencer may rebuild and replace recent blocks.
        let parent = self
            .provider
            .sealed_header_by_hash(data.parent_hash)
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?
            .ok_or_else(|| {
                MorphEngineApiError::Internal(format!(
                    "parent block not found: {}",
                    data.parent_hash
                ))
            })?;

        // 2. Block number must be exactly parent + 1.
        let expected_number = parent.number() + 1;
        if data.number != expected_number {
            self.metrics.new_l2_block_failures_total.increment(1);
            self.metrics
                .new_l2_block_duration_seconds
                .record(started.elapsed());
            return Err(MorphEngineApiError::DiscontinuousBlockNumber {
                expected: expected_number,
                actual: data.number,
            });
        }

        // 3. Import via the engine tree (newPayload + forkchoiceUpdated). The hash check
        //    against data.hash happens inside execution_payload_from_executable_data; the
        //    FCU advances or reorgs the canonical head onto data.hash.
        let block_timestamp = data.timestamp;
        let header = self
            .import_l2_block_via_engine(data, B256::ZERO)
            .await
            .inspect_err(|_| {
                self.metrics.new_l2_block_failures_total.increment(1);
                self.metrics
                    .new_l2_block_duration_seconds
                    .record(started.elapsed());
            })?;

        self.metrics
            .new_l2_block_duration_seconds
            .record(started.elapsed());
        self.record_head_metrics(block_timestamp);

        Ok(header)
    }

    async fn new_safe_l2_block(&self, mut data: SafeL2Data) -> EngineApiResult<MorphHeader> {
        let started = Instant::now();
        tracing::debug!(
            target: "morph::engine",
            block_number = data.number,
            parent_hash = ?data.parent_hash,
            "importing safe L2 block from L1 derivation"
        );

        let block_timestamp = data.timestamp;

        // Parent selection: caller-pinned (derivation reorg path, deriveForce) or the
        // current head (legacy sequential path). The block-number invariant
        // (`number == parent + 1`) is validated inside build_l2_payload against the
        // resolved parent, so callers that pin a non-head parent reorg correctly.
        let parent_override = data.parent_hash;

        // Assemble the block from SafeL2Data inputs.
        let assemble_params = AssembleL2BlockParams {
            number: data.number,
            // Move transactions out of data to avoid cloning the full Vec<Bytes>.
            transactions: std::mem::take(&mut data.transactions),
            timestamp: Some(data.timestamp),
        };

        let built_payload = self
            .build_l2_payload(
                assemble_params,
                Some(data.gas_limit),
                data.base_fee_per_gas,
                parent_override,
            )
            .await
            .inspect_err(|_| {
                self.metrics.new_safe_l2_block_failures_total.increment(1);
                self.metrics
                    .new_safe_l2_block_duration_seconds
                    .record(started.elapsed());
            })?;
        let executable_data = built_payload.executable_data;
        // Save hash before moving executable_data into the import call.
        let block_hash = executable_data.hash;

        // 3. Import the block through reth engine tree, marking the imported block safe
        // in the same FCU. Do not mark it finalized: L1 derivation can still reorg this
        // block, and finalized is only authoritative when supplied by BlockTagService via
        // set_block_tags. Return the in-path header (do not rely on immediate DB
        // visibility after FCU).
        let header = self
            .import_l2_block_via_engine(executable_data, block_hash)
            .await
            .inspect_err(|_| {
                self.metrics.new_safe_l2_block_failures_total.increment(1);
                self.metrics
                    .new_safe_l2_block_duration_seconds
                    .record(started.elapsed());
            })?;

        self.metrics
            .new_safe_l2_block_duration_seconds
            .record(started.elapsed());
        self.record_head_metrics(block_timestamp);

        tracing::debug!(
            target: "morph::engine",
            block_hash = %block_hash,
            "safe L2 block imported successfully"
        );

        Ok(header)
    }

    async fn set_block_tags(
        &self,
        safe_block_hash: B256,
        finalized_block_hash: B256,
    ) -> EngineApiResult<()> {
        // Match geth's SetBlockTags: look up the header by hash and call set_finalized /
        // set_safe on the provider directly, skipping zero hashes. This avoids a full
        // FCU round-trip through the async engine pipeline for what is purely a tag
        // update, and correctly skips the update when the caller passes B256::ZERO.
        //
        // Order matters: set safe FIRST, then finalized. The Ethereum invariant
        // `finalized.number <= safe.number` must hold at every observable point
        // for an RPC reader. Updating finalized first and then safe leaves a
        // window between the two writes where `eth_getBlockByNumber("finalized")`
        // returns the new value but `eth_getBlockByNumber("safe")` returns the
        // stale older value — a transient `finalized > safe` violation. Updating
        // safe first keeps the invariant satisfied throughout (finalized stays
        // at its older, smaller value while safe advances).
        let safe = if safe_block_hash != B256::ZERO {
            Some(self.resolve_block_tag(safe_block_hash, "safe")?)
        } else {
            None
        };
        let finalized = if finalized_block_hash != B256::ZERO {
            Some(self.resolve_block_tag(finalized_block_hash, "finalized")?)
        } else {
            None
        };
        if safe.is_none() && finalized.is_none() {
            return Ok(());
        }

        let canonical_head = self.current_head()?;
        validate_resolved_block_tags(
            safe.as_ref().map(|header| header.number()),
            finalized.as_ref().map(|header| header.number()),
            canonical_head.number,
        )?;

        if let Some(sealed) = safe {
            self.provider.set_safe(sealed);
            tracing::info!(
                target: "morph::engine",
                hash = %safe_block_hash,
                "safe block tag updated"
            );
        }

        if let Some(sealed) = finalized {
            self.provider.set_finalized(sealed);
            tracing::info!(
                target: "morph::engine",
                hash = %finalized_block_hash,
                "finalized block tag updated"
            );
        }

        // Cache the L1-derived finalized hash so subsequent FCU calls can forward it.
        // Safe is not cached: FCU safe is supplied by each import caller, and the
        // RPC-visible safe tag can also be advanced by set_block_tags.
        self.block_tag_tracker
            .record_finalized_hash(if finalized_block_hash != B256::ZERO {
                Some(finalized_block_hash)
            } else {
                None
            });

        Ok(())
    }
}

impl<Provider> RealMorphL2EngineApi<Provider> {
    /// Looks up a sealed header by hash.
    ///
    /// Used by `set_block_tags` to validate both tag updates before mutating provider state.
    fn resolve_block_tag(
        &self,
        hash: B256,
        tag_name: &str,
    ) -> EngineApiResult<SealedHeader<MorphHeader>>
    where
        Provider: HeaderProvider<Header = MorphHeader>,
    {
        self.provider
            .sealed_header_by_hash(hash)
            .map_err(|e| MorphEngineApiError::Internal(e.to_string()))?
            .ok_or_else(|| {
                MorphEngineApiError::Internal(format!("{tag_name} block {hash} not found"))
            })
    }

    async fn build_l2_payload(
        &self,
        params: AssembleL2BlockParams,
        gas_limit_override: Option<u64>,
        base_fee_override: Option<u128>,
        parent_override: Option<B256>,
    ) -> EngineApiResult<MorphBuiltPayload>
    where
        Provider: HeaderProvider<Header = MorphHeader>
            + BlockNumReader
            + BlockReaderIdExt<Header = MorphHeader>
            + Clone
            + Send
            + Sync
            + 'static,
    {
        tracing::debug!(
            target: "morph::engine",
            block_number = params.number,
            tx_count = params.transactions.len(),
            "assembling L2 block"
        );

        // 1. Resolve the parent: caller-pinned (reorg path, e.g. derivation deriveForce
        //    or assembleL2BlockV2) or the current canonical head (sequential path). When
        //    pinned, the parent need not be the head — building on it lets the subsequent
        //    forkchoice update reorganize the chain onto the new block.
        let (parent_number, parent_hash, parent_timestamp) = match parent_override {
            Some(parent_hash) => {
                let parent = self
                    .provider
                    .sealed_header_by_hash(parent_hash)
                    .map_err(|e| MorphEngineApiError::Database(e.to_string()))?
                    .ok_or_else(|| {
                        MorphEngineApiError::Internal(format!(
                            "parent block not found: {parent_hash}"
                        ))
                    })?;
                (parent.number(), parent_hash, parent.timestamp())
            }
            None => {
                let current_head = self.current_head()?;
                (
                    current_head.number,
                    current_head.hash,
                    current_head.timestamp,
                )
            }
        };

        // 2. Validate block number (must be parent + 1).
        if params.number != parent_number + 1 {
            return Err(MorphEngineApiError::DiscontinuousBlockNumber {
                expected: parent_number + 1,
                actual: params.number,
            });
        }

        // 3. Build payload attributes.
        let timestamp = params.timestamp.unwrap_or_else(|| {
            std::cmp::max(
                parent_timestamp + 1,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            )
        });
        let base_fee_override = base_fee_override
            .map(|fee| {
                u64::try_from(fee).map_err(|_| {
                    MorphEngineApiError::BlockBuildError(format!(
                        "base fee override exceeds u64: {fee}"
                    ))
                })
            })
            .transpose()?;

        let rpc_attributes = morph_payload_types::MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp,
                // Deterministic placeholder: Morph does not use fee recipient/prev_randao here.
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Address::ZERO,
                withdrawals: Some(Vec::new()),
                parent_beacon_block_root: None,
                // Morph L2 has no PoS slot semantics; introduced in alloy 2.0.
                slot_number: None,
            },
            transactions: Some(params.transactions),
            gas_limit: gas_limit_override,
            base_fee_per_gas: base_fee_override,
        };

        let payload_id = rpc_attributes.morph_payload_id(&parent_hash);

        let build_input = BuildNewPayload {
            attributes: rpc_attributes,
            parent_hash,
            cache: None,
            trie_handle: None,
        };
        let _ = self
            .payload_builder
            .send_new_payload(build_input)
            .await
            .map_err(|_| {
                MorphEngineApiError::BlockBuildError("failed to send build request".to_string())
            })?
            .map_err(|e| {
                MorphEngineApiError::BlockBuildError(format!(
                    "failed to receive build response: {e}"
                ))
            })?;

        // Morph builds blocks synchronously (no external CL timeout), so we
        // use WaitForPending to wait for the payload builder to finish rather
        // than racing an empty payload via Earliest.
        self.payload_builder
            .resolve_kind(payload_id, reth_node_api::PayloadKind::WaitForPending)
            .await
            .ok_or_else(|| {
                MorphEngineApiError::Internal(format!("no payload response for id {payload_id:?}"))
            })?
            .map_err(|e| {
                MorphEngineApiError::BlockBuildError(format!("failed to get built payload: {e}"))
            })
    }

    async fn import_l2_block_via_engine(
        &self,
        data: ExecutableL2Data,
        safe_block_hash: B256,
    ) -> EngineApiResult<MorphHeader>
    where
        Provider: HeaderProvider<Header = MorphHeader>
            + BlockNumReader
            + CanonChainTracker<Header = MorphHeader>,
    {
        let (payload, header) = self.execution_payload_from_executable_data(&data)?;

        let payload_status = self
            .engine_handle
            .new_payload(payload)
            .await
            .map_err(|e| MorphEngineApiError::ExecutionFailed(e.to_string()))?;
        ensure_payload_status_valid(&payload_status, "newPayload")?;

        // FCU safe/finalized must be canonical ancestors. Unsafe imports pass safe zero;
        // new_safe_l2_block passes the imported block itself, never a cached old safe.
        // Forward only the L1-derived finalized tag; zero is a no-op when it is absent,
        // and pinned reth v2.2.0 still cleans changesets/canonical memory without
        // finalized.
        let forkchoice = alloy_rpc_types_engine::ForkchoiceState {
            head_block_hash: data.hash,
            safe_block_hash,
            finalized_block_hash: self
                .block_tag_tracker
                .l1_finalized_hash()
                .unwrap_or_default(),
        };

        self.provider.on_forkchoice_update_received(&forkchoice);

        let fcu_result = self
            .engine_handle
            .fork_choice_updated(forkchoice, None)
            .await
            .map_err(|e| MorphEngineApiError::ExecutionFailed(e.to_string()))?;
        ensure_payload_status_valid(&fcu_result.payload_status, "forkchoiceUpdated")?;

        Ok(header)
    }

    fn header_and_body_from_executable_data(
        &self,
        data: &ExecutableL2Data,
    ) -> EngineApiResult<(MorphHeader, BlockBody)> {
        let base_fee_per_gas = data
            .base_fee_per_gas
            .map(|fee| {
                u64::try_from(fee).map_err(|_| {
                    MorphEngineApiError::ValidationFailed(format!(
                        "base_fee_per_gas exceeds u64 in block {}",
                        data.hash
                    ))
                })
            })
            .transpose()?;
        if data.logs_bloom.len() != 256 {
            return Err(MorphEngineApiError::ValidationFailed(format!(
                "logs_bloom must be 256 bytes, got {} bytes in block {}",
                data.logs_bloom.len(),
                data.hash
            )));
        }

        let mut txs = Vec::with_capacity(data.transactions.len());
        for (index, tx_bytes) in data.transactions.iter().enumerate() {
            let mut buf = tx_bytes.as_ref();
            let tx = MorphTxEnvelope::decode_2718(&mut buf).map_err(|e| {
                MorphEngineApiError::InvalidTransaction {
                    index,
                    message: e.to_string(),
                }
            })?;
            if !buf.is_empty() {
                return Err(MorphEngineApiError::InvalidTransaction {
                    index,
                    message: "trailing bytes after tx RLP decoding".to_string(),
                });
            }
            txs.push(tx);
        }

        let logs_bloom = alloy_primitives::Bloom::from_slice(data.logs_bloom.as_ref());
        // Override coinbase to empty address when FeeVault is enabled,
        // matching go-ethereum's executableDataToBlock (l2_api.go:292-293).
        let beneficiary = if self.chain_spec.is_fee_vault_enabled() {
            Address::ZERO
        } else {
            data.miner
        };
        let header = MorphHeader {
            next_l1_msg_index: data.next_l1_message_index,
            inner: Header {
                parent_hash: data.parent_hash,
                ommers_hash: EMPTY_OMMER_ROOT_HASH,
                beneficiary,
                state_root: data.state_root,
                transactions_root: calculate_transaction_root(&txs),
                receipts_root: data.receipts_root,
                // Morph L2 has no withdrawals — always None, matching assemble path.
                withdrawals_root: None,
                logs_bloom,
                difficulty: Default::default(),
                number: data.number,
                gas_limit: data.gas_limit,
                gas_used: data.gas_used,
                timestamp: data.timestamp,
                mix_hash: B256::ZERO,
                nonce: B64::ZERO,
                base_fee_per_gas,
                extra_data: Default::default(),
                parent_beacon_block_root: None,
                // Morph L2 has no blob transactions — always None, matching assemble path.
                blob_gas_used: None,
                excess_blob_gas: None,
                requests_hash: None,
                // Pre-Amsterdam Morph blocks do not carry a block-access-list hash,
                // and there is no PoS slot number.
                block_access_list_hash: None,
                slot_number: None,
            },
        };
        let body = BlockBody {
            transactions: txs,
            ommers: Default::default(),
            withdrawals: None,
        };

        Ok((header, body))
    }

    fn execution_payload_from_executable_data(
        &self,
        data: &ExecutableL2Data,
    ) -> EngineApiResult<(MorphExecutionData, MorphHeader)> {
        let (header, body) = self.header_and_body_from_executable_data(data)?;

        // Compute header hash once and verify against expected hash before
        // constructing the sealed block. This avoids the clone + re-hash that
        // seal_slow would perform, saving one keccak256 + one MorphHeader clone
        // per block import.
        let computed_hash = header.hash_slow();
        if computed_hash != data.hash {
            return Err(MorphEngineApiError::ValidationFailed(format!(
                "block hash mismatch: expected {}, computed {}",
                data.hash, computed_hash
            )));
        }
        let sealed_block =
            SealedBlock::new_unchecked(Block::new(header.clone(), body), computed_hash);

        Ok((
            MorphExecutionData::with_expected_withdraw_trie_root(
                Arc::new(sealed_block),
                data.withdraw_trie_root,
            ),
            header,
        ))
    }

    fn current_head(&self) -> EngineApiResult<CanonicalHead>
    where
        Provider: BlockReaderIdExt<Header = MorphHeader>,
    {
        let header = self
            .provider
            .latest_header()
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?
            .ok_or_else(|| {
                MorphEngineApiError::Internal("canonical head header not found".to_string())
            })?;

        Ok(CanonicalHead {
            number: header.number(),
            hash: header.hash(),
            timestamp: header.timestamp(),
        })
    }
}

fn validate_resolved_block_tags(
    safe_number: Option<u64>,
    finalized_number: Option<u64>,
    canonical_head_number: u64,
) -> EngineApiResult<()> {
    if let Some(safe_number) = safe_number
        && safe_number > canonical_head_number
    {
        return Err(MorphEngineApiError::ValidationFailed(format!(
            "safe block number {safe_number} exceeds canonical head number {canonical_head_number}"
        )));
    }

    if let Some(finalized_number) = finalized_number
        && finalized_number > canonical_head_number
    {
        return Err(MorphEngineApiError::ValidationFailed(format!(
            "finalized block number {finalized_number} exceeds canonical head number {canonical_head_number}"
        )));
    }

    if let (Some(safe_number), Some(finalized_number)) = (safe_number, finalized_number)
        && finalized_number > safe_number
    {
        return Err(MorphEngineApiError::ValidationFailed(format!(
            "finalized block number {finalized_number} exceeds safe block number {safe_number}"
        )));
    }

    Ok(())
}

fn payload_status_is_validated(status: &PayloadStatus) -> bool {
    matches!(status.status, PayloadStatusEnum::Valid)
}

fn ensure_payload_status_valid(
    status: &PayloadStatus,
    context: &'static str,
) -> EngineApiResult<()> {
    match &status.status {
        PayloadStatusEnum::Valid => Ok(()),
        PayloadStatusEnum::Accepted => Err(MorphEngineApiError::ExecutionFailed(format!(
            "{context} returned ACCEPTED before payload was validated"
        ))),
        PayloadStatusEnum::Syncing => Err(MorphEngineApiError::ExecutionFailed(format!(
            "{context} returned SYNCING for payload"
        ))),
        PayloadStatusEnum::Invalid { validation_error } => {
            Err(MorphEngineApiError::ValidationFailed(format!(
                "{context} returned INVALID: {validation_error}"
            )))
        }
    }
}

#[cfg(test)]
fn apply_executable_data_overrides(
    recovered_block: RecoveredBlock<Block>,
    data: &ExecutableL2Data,
) -> EngineApiResult<RecoveredBlock<Block>> {
    let base_fee_per_gas = data
        .base_fee_per_gas
        .map(|fee| {
            u64::try_from(fee).map_err(|_| {
                MorphEngineApiError::ValidationFailed(format!(
                    "base_fee_per_gas exceeds u64 in block {}",
                    data.hash
                ))
            })
        })
        .transpose()?;
    if data.logs_bloom.len() != 256 {
        return Err(MorphEngineApiError::ValidationFailed(format!(
            "logs_bloom must be 256 bytes, got {} bytes in block {}",
            data.logs_bloom.len(),
            data.hash
        )));
    }
    let logs_bloom = alloy_primitives::Bloom::from_slice(data.logs_bloom.as_ref());

    let (block, senders) = recovered_block.split();
    let block = block.map_header(|mut header: MorphHeader| {
        // Normalize header fields from sequencer input so hash calculation is deterministic.
        header.inner.parent_hash = data.parent_hash;
        header.inner.beneficiary = data.miner;
        header.inner.number = data.number;
        header.inner.gas_limit = data.gas_limit;
        header.inner.gas_used = data.gas_used;
        header.inner.timestamp = data.timestamp;
        header.inner.state_root = data.state_root;
        header.inner.receipts_root = data.receipts_root;
        header.inner.base_fee_per_gas = base_fee_per_gas;
        header.inner.logs_bloom = logs_bloom;
        header.next_l1_msg_index = data.next_l1_message_index;
        header
    });
    Ok(RecoveredBlock::new_unhashed(block, senders))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::{Address, Bloom, Bytes};
    use alloy_rpc_types_engine::{PayloadStatus, PayloadStatusEnum};
    use morph_primitives::BlockBody;

    fn recovered_with_header(header: MorphHeader) -> RecoveredBlock<Block> {
        let block = Block::new(header, BlockBody::default());
        RecoveredBlock::new_unhashed(block, Vec::new())
    }

    fn payload_status(status: PayloadStatusEnum) -> PayloadStatus {
        PayloadStatus::from_status(status)
    }

    #[test]
    fn test_validation_success_requires_valid_payload_status() {
        assert!(payload_status_is_validated(&payload_status(
            PayloadStatusEnum::Valid
        )));
        assert!(!payload_status_is_validated(&payload_status(
            PayloadStatusEnum::Accepted
        )));
        assert!(!payload_status_is_validated(&payload_status(
            PayloadStatusEnum::Syncing
        )));
        assert!(!payload_status_is_validated(&payload_status(
            PayloadStatusEnum::Invalid {
                validation_error: "bad payload".to_string(),
            }
        )));
    }

    #[test]
    fn test_ensure_payload_status_valid_rejects_accepted() {
        let err =
            ensure_payload_status_valid(&payload_status(PayloadStatusEnum::Accepted), "newPayload")
                .unwrap_err();

        match err {
            MorphEngineApiError::ExecutionFailed(msg) => {
                assert!(msg.contains("newPayload returned ACCEPTED"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn test_apply_executable_data_overrides_aligns_hash_with_engine_data() {
        let source_header: MorphHeader = Header::default().into();
        let source_recovered = recovered_with_header(source_header);

        let target_header = MorphHeader {
            next_l1_msg_index: 42,
            inner: Header {
                parent_hash: B256::from([0x11; 32]),
                beneficiary: Address::from([0x22; 20]),
                number: 7,
                gas_limit: 30_000_000,
                gas_used: 21_000,
                timestamp: 1_700_000_001,
                state_root: B256::from([0x33; 32]),
                receipts_root: B256::from([0x44; 32]),
                base_fee_per_gas: Some(1_000_000_000),
                logs_bloom: Bloom::from([0x55; 256]),
                ..Default::default()
            },
        };
        let expected_hash = recovered_with_header(target_header.clone()).hash();

        let data = ExecutableL2Data {
            parent_hash: target_header.inner.parent_hash,
            miner: target_header.inner.beneficiary,
            number: target_header.inner.number,
            gas_limit: target_header.inner.gas_limit,
            base_fee_per_gas: target_header.inner.base_fee_per_gas.map(u128::from),
            timestamp: target_header.inner.timestamp,
            transactions: Vec::new(),
            state_root: target_header.inner.state_root,
            gas_used: target_header.inner.gas_used,
            receipts_root: target_header.inner.receipts_root,
            logs_bloom: Bytes::copy_from_slice(target_header.inner.logs_bloom.as_slice()),
            withdraw_trie_root: B256::ZERO,
            next_l1_message_index: target_header.next_l1_msg_index,
            hash: expected_hash,
        };

        let overridden = apply_executable_data_overrides(source_recovered, &data).unwrap();
        assert_eq!(overridden.hash(), expected_hash);
    }

    #[test]
    fn test_apply_executable_data_overrides_rejects_overflow_base_fee() {
        let recovered = recovered_with_header(Header::default().into());
        let data = ExecutableL2Data {
            base_fee_per_gas: Some((u64::MAX as u128) + 1),
            hash: B256::from([0x99; 32]),
            ..Default::default()
        };

        let err = apply_executable_data_overrides(recovered, &data).unwrap_err();
        match err {
            MorphEngineApiError::ValidationFailed(msg) => {
                assert!(msg.contains("base_fee_per_gas exceeds u64"))
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn test_apply_executable_data_overrides_rejects_invalid_logs_bloom_len() {
        let recovered = recovered_with_header(Header::default().into());
        let data = ExecutableL2Data {
            logs_bloom: Bytes::from(vec![0u8; 32]),
            hash: B256::from([0x77; 32]),
            ..Default::default()
        };

        let err = apply_executable_data_overrides(recovered, &data).unwrap_err();
        match err {
            MorphEngineApiError::ValidationFailed(msg) => {
                assert!(msg.contains("logs_bloom must be 256 bytes"))
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn test_apply_executable_data_overrides_sets_header_fields_exactly() {
        let source_header = MorphHeader {
            next_l1_msg_index: 1,
            inner: Header {
                parent_hash: B256::from([0x01; 32]),
                beneficiary: Address::from([0x02; 20]),
                number: 1,
                gas_limit: 1_000_000,
                gas_used: 500_000,
                timestamp: 10,
                state_root: B256::from([0x03; 32]),
                receipts_root: B256::from([0x04; 32]),
                base_fee_per_gas: Some(123),
                logs_bloom: Bloom::from([0x05; 256]),
                ..Default::default()
            },
        };
        let recovered = recovered_with_header(source_header);
        let data = ExecutableL2Data {
            parent_hash: B256::from([0x11; 32]),
            miner: Address::from([0x22; 20]),
            number: 9,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            timestamp: 1_700_000_009,
            transactions: Vec::new(),
            state_root: B256::from([0x33; 32]),
            gas_used: 21_009,
            receipts_root: B256::from([0x44; 32]),
            logs_bloom: Bytes::from(vec![0x55; 256]),
            withdraw_trie_root: B256::ZERO,
            next_l1_message_index: 99,
            hash: B256::from([0x66; 32]),
        };

        let overridden = apply_executable_data_overrides(recovered, &data).unwrap();
        let sealed = overridden.sealed_block();
        let header = sealed.header();

        assert_eq!(header.inner.parent_hash, data.parent_hash);
        assert_eq!(header.inner.beneficiary, data.miner);
        assert_eq!(header.inner.number, data.number);
        assert_eq!(header.inner.gas_limit, data.gas_limit);
        assert_eq!(header.inner.gas_used, data.gas_used);
        assert_eq!(header.inner.timestamp, data.timestamp);
        assert_eq!(header.inner.state_root, data.state_root);
        assert_eq!(header.inner.receipts_root, data.receipts_root);
        assert_eq!(
            header.inner.base_fee_per_gas,
            data.base_fee_per_gas.map(|v| v as u64)
        );
        assert_eq!(header.inner.logs_bloom.as_slice(), data.logs_bloom.as_ref());
        assert_eq!(header.next_l1_msg_index, data.next_l1_message_index);
    }

    #[test]
    fn test_apply_executable_data_overrides_supports_none_base_fee() {
        let recovered = recovered_with_header(MorphHeader {
            inner: Header {
                base_fee_per_gas: Some(10),
                ..Default::default()
            },
            ..Default::default()
        });
        let data = ExecutableL2Data {
            base_fee_per_gas: None,
            logs_bloom: Bytes::from(vec![0u8; 256]),
            hash: B256::from([0x44; 32]),
            ..Default::default()
        };

        let overridden = apply_executable_data_overrides(recovered, &data).unwrap();
        assert_eq!(
            overridden.sealed_block().header().inner.base_fee_per_gas,
            None
        );
    }

    #[test]
    fn test_block_tag_tracker_records_finalized_hash() {
        let tracker = BlockTagTracker::default();
        let finalized_hash = B256::from([0x22; 32]);

        tracker.record_finalized_hash(Some(finalized_hash));
        assert_eq!(tracker.l1_finalized_hash(), Some(finalized_hash));

        // `None` is ignored: a previously-supplied finalized is preserved.
        tracker.record_finalized_hash(None);
        assert_eq!(tracker.l1_finalized_hash(), Some(finalized_hash));
    }

    #[test]
    fn test_validate_resolved_block_tags_rejects_finalized_after_safe() {
        let err = validate_resolved_block_tags(Some(10), Some(11), 11).unwrap_err();
        match err {
            MorphEngineApiError::ValidationFailed(msg) => {
                assert!(msg.contains("finalized block number 11 exceeds safe block number 10"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn test_validate_resolved_block_tags_rejects_safe_after_canonical_head() {
        let err = validate_resolved_block_tags(Some(12), Some(11), 11).unwrap_err();
        match err {
            MorphEngineApiError::ValidationFailed(msg) => {
                assert!(msg.contains("safe block number 12 exceeds canonical head number 11"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    // =========================================================================
    // apply_executable_data_overrides edge cases
    // =========================================================================

    #[test]
    fn test_apply_executable_data_overrides_exact_u64_max_base_fee() {
        let recovered = recovered_with_header(Header::default().into());
        let data = ExecutableL2Data {
            base_fee_per_gas: Some(u64::MAX as u128),
            logs_bloom: Bytes::from(vec![0u8; 256]),
            hash: B256::from([0x55; 32]),
            ..Default::default()
        };

        // u64::MAX should be accepted (it fits in u64)
        let result = apply_executable_data_overrides(recovered, &data);
        assert!(result.is_ok());
        let header = result.unwrap().sealed_block().header().clone();
        assert_eq!(header.inner.base_fee_per_gas, Some(u64::MAX));
    }

    #[test]
    fn test_apply_executable_data_overrides_empty_logs_bloom() {
        let recovered = recovered_with_header(Header::default().into());
        let data = ExecutableL2Data {
            logs_bloom: Bytes::new(),
            hash: B256::from([0x66; 32]),
            ..Default::default()
        };

        let err = apply_executable_data_overrides(recovered, &data).unwrap_err();
        match err {
            MorphEngineApiError::ValidationFailed(msg) => {
                assert!(msg.contains("logs_bloom must be 256 bytes"));
                assert!(msg.contains("0 bytes"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn test_apply_executable_data_overrides_oversized_logs_bloom() {
        let recovered = recovered_with_header(Header::default().into());
        let data = ExecutableL2Data {
            logs_bloom: Bytes::from(vec![0u8; 512]),
            hash: B256::from([0x77; 32]),
            ..Default::default()
        };

        let err = apply_executable_data_overrides(recovered, &data).unwrap_err();
        match err {
            MorphEngineApiError::ValidationFailed(msg) => {
                assert!(msg.contains("512 bytes"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
