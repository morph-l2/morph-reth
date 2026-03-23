//! Morph L2 Engine API implementation.
//!
//! This module provides the concrete Morph L2 Engine API implementation and supporting helpers.

use crate::{EngineApiResult, MorphEngineApiError, MorphL2EngineApi};
use alloy_consensus::{
    BlockHeader, EMPTY_OMMER_ROOT_HASH, Header, constants::EMPTY_WITHDRAWALS,
    proofs::calculate_transaction_root,
};
use alloy_eips::eip2718::Decodable2718;
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{Address, B64, B256, Sealable};
use alloy_rpc_types_engine::PayloadAttributes;
use morph_chainspec::MorphChainSpec;
use morph_payload_types::{
    AssembleL2BlockParams, ExecutableL2Data, GenericResponse, MorphBuiltPayload,
    MorphExecutionData, MorphPayloadBuilderAttributes, MorphPayloadTypes, SafeL2Data,
};
use morph_primitives::{Block, BlockBody, MorphHeader, MorphPrimitives, MorphTxEnvelope};
use parking_lot::RwLock;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{EngineApiMessageVersion, PayloadBuilderAttributes};
#[cfg(test)]
use reth_primitives_traits::RecoveredBlock;
use reth_primitives_traits::{SealedBlock, SealedHeader};
use reth_provider::{BlockIdReader, BlockNumReader, CanonChainTracker, HeaderProvider};
use std::{sync::Arc, time::Instant};

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

    /// Engine-state tracker updated from consensus engine events (authoritative) and local FCU
    /// success hints (fast path).
    engine_state_tracker: Arc<EngineStateTracker>,
}

#[derive(Debug, Clone, Copy)]
struct InMemoryHead {
    number: u64,
    hash: B256,
    timestamp: u64,
}

/// Allow FCU tag fallback to head only while the imported block is clearly historical.
///
/// Once imported blocks are close to wall-clock time, we stop synthesizing safe/finalized and
/// wait for Morph node's real `set_block_tags` updates instead.
const FCU_TAG_FALLBACK_MAX_AGE_SECS: u64 = 60;

/// Tracks engine-visible canonical head for the custom Morph engine API.
///
/// Updated from `CanonicalChainCommitted` consensus engine events and optimistically
/// on successful local FCU calls to reduce latency before event delivery.
///
/// Also caches L1-based safe/finalized block hashes from `set_block_tags` so that
/// the FCU can pass them to the engine tree, keeping both memory cleanup and
/// RPC-visible tags consistent.
#[derive(Debug, Default)]
pub struct EngineStateTracker {
    head: RwLock<Option<InMemoryHead>>,
    /// Last L1-based safe/finalized hashes from `set_block_tags`.
    /// `None` means `set_block_tags` has not yet provided a value (e.g. during
    /// historical sync when all batches are already finalized on L1).
    block_tags: RwLock<BlockTagCache>,
}

/// Cached L1-based block tag hashes from `set_block_tags`.
#[derive(Debug, Default, Clone, Copy)]
struct BlockTagCache {
    safe_hash: Option<B256>,
    finalized_hash: Option<B256>,
}

impl EngineStateTracker {
    /// Records a canonical head hint from a locally successful FCU call.
    pub fn record_local_head(&self, number: u64, hash: B256, timestamp: u64) {
        *self.head.write() = Some(InMemoryHead {
            number,
            hash,
            timestamp,
        });
    }

    /// Updates the tracker from a consensus engine event stream item.
    pub fn on_consensus_engine_event(
        &self,
        event: &reth_node_api::ConsensusEngineEvent<MorphPrimitives>,
    ) {
        use reth_node_api::ConsensusEngineEvent;

        if let ConsensusEngineEvent::CanonicalChainCommitted(header, _) = event {
            self.record_local_head(header.number(), header.hash(), header.timestamp());
        }
    }

    fn current_head(&self) -> Option<InMemoryHead> {
        *self.head.read()
    }

    /// Caches L1-based block tag hashes from a successful `set_block_tags` call.
    pub fn record_block_tags(&self, safe_hash: Option<B256>, finalized_hash: Option<B256>) {
        let mut tags = self.block_tags.write();
        if let Some(h) = safe_hash {
            tags.safe_hash = Some(h);
        }
        if let Some(h) = finalized_hash {
            tags.finalized_hash = Some(h);
        }
    }

    /// Returns the last L1-based finalized hash, or `None` if not yet set.
    fn l1_finalized_hash(&self) -> Option<B256> {
        self.block_tags.read().finalized_hash
    }

    /// Returns the last L1-based safe hash, or `None` if not yet set.
    fn l1_safe_hash(&self) -> Option<B256> {
        self.block_tags.read().safe_hash
    }
}

impl<Provider> RealMorphL2EngineApi<Provider> {
    /// Creates a new [`RealMorphL2EngineApi`].
    pub fn new(
        provider: Provider,
        payload_builder: PayloadBuilderHandle<MorphPayloadTypes>,
        chain_spec: Arc<MorphChainSpec>,
        engine_handle: reth_node_api::ConsensusEngineHandle<MorphPayloadTypes>,
        engine_state_tracker: Arc<EngineStateTracker>,
    ) -> Self {
        Self {
            provider,
            payload_builder,
            chain_spec,
            engine_handle,
            engine_state_tracker,
        }
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
        + BlockIdReader
        + BlockNumReader
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
        let built_payload = self.build_l2_payload(params, None, None).await?;
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
            return Err(MorphEngineApiError::WrongParentHash {
                expected: current_head.hash,
                actual: data.parent_hash,
            });
        }

        // 2. Convert and forward to reth engine tree (`newPayload` path).
        let convert_started = Instant::now();
        let (payload, _) = match self.execution_payload_from_executable_data(&data) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    target: "morph::engine",
                    block_hash = %data.hash,
                    error = %err,
                    "failed to convert executable data for validation"
                );
                return Ok(GenericResponse { success: false });
            }
        };
        let convert_elapsed = convert_started.elapsed();

        let new_payload_started = Instant::now();
        let status = match self.engine_handle.new_payload(payload).await {
            Ok(status) => status,
            Err(err) => {
                tracing::warn!(
                    target: "morph::engine",
                    block_hash = %data.hash,
                    error = %err,
                    "engine new_payload failed during validate_l2_block"
                );
                return Ok(GenericResponse { success: false });
            }
        };
        let new_payload_elapsed = new_payload_started.elapsed();

        tracing::debug!(
            target: "morph::engine",
            block_hash = %data.hash,
            status = ?status.status,
            "validate_l2_block returned engine payload status"
        );

        let success = matches!(
            status.status,
            alloy_rpc_types_engine::PayloadStatusEnum::Valid
                | alloy_rpc_types_engine::PayloadStatusEnum::Accepted
        );
        tracing::info!(
            target: "morph::engine",
            block_number = data.number,
            block_hash = %data.hash,
            convert_elapsed = ?convert_elapsed,
            new_payload_elapsed = ?new_payload_elapsed,
            total_elapsed = ?validate_started.elapsed(),
            status = ?status.status,
            success,
            "validate_l2_block timing"
        );

        Ok(GenericResponse { success })
    }

    async fn new_l2_block(&self, data: ExecutableL2Data) -> EngineApiResult<()> {
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
                return Ok(());
            }
            // Discontinuous block number
            tracing::warn!(
                target: "morph::engine",
                expected_number = expected_number,
                actual_number = data.number,
                "cannot new block with discontinuous block number"
            );
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
            return Err(MorphEngineApiError::WrongParentHash {
                expected: current_head.hash,
                actual: data.parent_hash,
            });
        }

        let block_hash = data.hash;
        let block_number = data.number;
        self.import_l2_block_via_engine(data).await?;

        tracing::debug!(
            target: "morph::engine",
            block_hash = %block_hash,
            block_number,
            "L2 block accepted via engine tree"
        );

        Ok(())
    }

    async fn new_safe_l2_block(&self, mut data: SafeL2Data) -> EngineApiResult<MorphHeader> {
        tracing::debug!(
            target: "morph::engine",
            block_number = data.number,
            "importing safe L2 block from L1 derivation"
        );

        // 1. Get latest block number
        let latest_number = self.current_head()?.number;

        if data.number != latest_number + 1 {
            return Err(MorphEngineApiError::DiscontinuousBlockNumber {
                expected: latest_number + 1,
                actual: data.number,
            });
        }

        // 2. Assemble the block from SafeL2Data inputs.
        let assemble_params = AssembleL2BlockParams {
            number: data.number,
            // Move transactions out of data to avoid cloning the full Vec<Bytes>.
            transactions: std::mem::take(&mut data.transactions),
            timestamp: Some(data.timestamp),
        };

        let built_payload = self
            .build_l2_payload(assemble_params, Some(data.gas_limit), data.base_fee_per_gas)
            .await?;
        let executable_data = built_payload.executable_data;
        // Save hash before moving executable_data into the import call.
        let block_hash = executable_data.hash;

        // 3. Import the block through reth engine tree and return the in-path header
        // (do not rely on immediate DB visibility after FCU).
        let header = self.import_l2_block_via_engine(executable_data).await?;

        // Update safe block tag and seed finalized for memory cleanup.
        //
        // Validator / derivation mode does not run BlockTagService, so
        // set_block_tags is never called externally.  Without a cached
        // finalized hash the FCU falls back to B256::ZERO once blocks are
        // near wall-clock time, disabling changeset-cache eviction.
        //
        // Passing block_hash as finalized here seeds the tracker so the
        // engine tree can keep evicting.  Once validators adopt
        // BlockTagService the L1-derived finalized value will naturally
        // supersede this hint.
        //
        // Best-effort: block import already succeeded, so don't fail the
        // whole call if only the tag update encounters an issue.
        if let Err(e) = self.set_block_tags(block_hash, block_hash).await {
            tracing::warn!(
                target: "morph::engine",
                block_hash = %block_hash,
                error = %e,
                "failed to update safe tag after block import; tag can be set later via setBlockTags"
            );
        }

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
        if finalized_block_hash != B256::ZERO {
            self.update_block_tag(finalized_block_hash, "finalized", |sealed| {
                self.provider.set_finalized(sealed);
            })?;
        }

        if safe_block_hash != B256::ZERO {
            self.update_block_tag(safe_block_hash, "safe", |sealed| {
                self.provider.set_safe(sealed);
            })?;
        }

        // Cache the L1-based hashes so subsequent FCU calls use them instead of
        // falling back to head.  This keeps engine-tree finalization and
        // RPC-visible tags aligned with the actual L1 finalization status.
        self.engine_state_tracker.record_block_tags(
            if safe_block_hash != B256::ZERO {
                Some(safe_block_hash)
            } else {
                None
            },
            if finalized_block_hash != B256::ZERO {
                Some(finalized_block_hash)
            } else {
                None
            },
        );

        Ok(())
    }
}

impl<Provider> RealMorphL2EngineApi<Provider> {
    /// Looks up a sealed header by hash, calls `setter` on it, and logs the tag update.
    ///
    /// Used by `set_block_tags` to deduplicate the finalized/safe update paths.
    fn update_block_tag(
        &self,
        hash: B256,
        tag_name: &str,
        setter: impl FnOnce(SealedHeader<MorphHeader>),
    ) -> EngineApiResult<()>
    where
        Provider: HeaderProvider<Header = MorphHeader>,
    {
        let sealed = self
            .provider
            .sealed_header_by_hash(hash)
            .map_err(|e| MorphEngineApiError::Internal(e.to_string()))?
            .ok_or_else(|| {
                MorphEngineApiError::Internal(format!("{tag_name} block {hash} not found"))
            })?;
        setter(sealed);
        tracing::info!(
            target: "morph::engine",
            %hash,
            "{tag_name} block tag updated"
        );
        Ok(())
    }

    async fn build_l2_payload(
        &self,
        params: AssembleL2BlockParams,
        gas_limit_override: Option<u64>,
        base_fee_override: Option<u128>,
    ) -> EngineApiResult<MorphBuiltPayload>
    where
        Provider:
            HeaderProvider<Header = MorphHeader> + BlockNumReader + Clone + Send + Sync + 'static,
    {
        tracing::debug!(
            target: "morph::engine",
            block_number = params.number,
            tx_count = params.transactions.len(),
            "assembling L2 block"
        );

        // 1. Validate block number (must be current_head + 1).
        let current_head = self.current_head()?;
        if params.number != current_head.number + 1 {
            return Err(MorphEngineApiError::DiscontinuousBlockNumber {
                expected: current_head.number + 1,
                actual: params.number,
            });
        }

        // 2. Build payload attributes.
        let parent_hash = current_head.hash;
        let timestamp = params.timestamp.unwrap_or_else(|| {
            std::cmp::max(
                current_head.timestamp + 1,
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
            },
            transactions: Some(params.transactions),
            gas_limit: gas_limit_override,
            base_fee_per_gas: base_fee_override,
        };

        let builder_attrs = MorphPayloadBuilderAttributes::try_new(parent_hash, rpc_attributes, 1)
            .map_err(|e| {
                MorphEngineApiError::BlockBuildError(format!(
                    "failed to create builder attributes: {e}",
                ))
            })?;
        let payload_id = builder_attrs.payload_id();

        let _ = self
            .payload_builder
            .send_new_payload(builder_attrs)
            .await
            .map_err(|_| {
                MorphEngineApiError::BlockBuildError("failed to send build request".to_string())
            })?
            .map_err(|e| {
                MorphEngineApiError::BlockBuildError(format!(
                    "failed to receive build response: {e}"
                ))
            })?;

        self.payload_builder
            .best_payload(payload_id)
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
    ) -> EngineApiResult<MorphHeader>
    where
        Provider: HeaderProvider<Header = MorphHeader>
            + BlockIdReader
            + BlockNumReader
            + CanonChainTracker<Header = MorphHeader>,
    {
        let import_started = Instant::now();
        let convert_started = Instant::now();
        let (payload, header) = self.execution_payload_from_executable_data(&data)?;
        let convert_elapsed = convert_started.elapsed();

        let new_payload_started = Instant::now();
        let payload_status = self
            .engine_handle
            .new_payload(payload)
            .await
            .map_err(|e| MorphEngineApiError::ExecutionFailed(e.to_string()))?;
        let new_payload_elapsed = new_payload_started.elapsed();
        self.ensure_payload_status_acceptable(&payload_status, "newPayload")?;

        // Morph uses Tendermint consensus with instant finality — every committed
        // block is final and no reorgs are possible.
        //
        // The safe/finalized hashes passed here serve two purposes in reth's engine
        // tree: (1) driving changeset-cache eviction and sidechain pruning (memory
        // management), and (2) setting the RPC-visible "safe"/"finalized" block tags.
        //
        // When BlockTagService has provided L1-based tags via set_block_tags, we
        // forward those so the engine tree and RPC layer stay consistent with the
        // actual L1 finalization status.
        //
        // During deep historical sync, BlockTagService may be unable to provide
        // tags for already-finalized batches. In that case we temporarily fall back
        // to head so the engine tree can continue evicting old changesets.
        //
        // Once imported blocks are close to wall-clock time, we stop synthesizing
        // safe/finalized and wait for real L1-derived tags to avoid falsely
        // advertising live blocks as finalized in the catch-up window.
        let now_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let finalized_hash = resolve_fcu_block_tag_hash(
            self.engine_state_tracker.l1_finalized_hash(),
            data.hash,
            data.timestamp,
            now_timestamp,
        );
        let safe_hash = resolve_fcu_block_tag_hash(
            self.engine_state_tracker.l1_safe_hash(),
            data.hash,
            data.timestamp,
            now_timestamp,
        );
        let forkchoice = alloy_rpc_types_engine::ForkchoiceState {
            head_block_hash: data.hash,
            safe_block_hash: safe_hash,
            finalized_block_hash: finalized_hash,
        };

        self.provider.on_forkchoice_update_received(&forkchoice);

        let fcu_started = Instant::now();
        let fcu_result = self
            .engine_handle
            .fork_choice_updated(forkchoice, None, Self::engine_api_version())
            .await
            .map_err(|e| MorphEngineApiError::ExecutionFailed(e.to_string()))?;
        let fcu_elapsed = fcu_started.elapsed();
        self.ensure_payload_status_acceptable(&fcu_result.payload_status, "forkchoiceUpdated")?;

        // Synchronously update the canonical head so that eth_blockNumber immediately
        // reflects the new block. The background write pipeline updates
        // canonical_in_memory_state asynchronously; without this call, morph-node
        // would see eth_blockNumber return the old block number and reject the next
        // block as ErrWrongBlockNumber.
        self.engine_state_tracker
            .record_local_head(data.number, data.hash, data.timestamp);
        self.provider
            .set_canonical_head(SealedHeader::new(header.clone(), data.hash));

        tracing::info!(
            target: "morph::engine",
            block_number = data.number,
            block_hash = %data.hash,
            convert_elapsed = ?convert_elapsed,
            new_payload_elapsed = ?new_payload_elapsed,
            fcu_elapsed = ?fcu_elapsed,
            total_elapsed = ?import_started.elapsed(),
            new_payload_status = ?payload_status.status,
            fcu_status = ?fcu_result.payload_status.status,
            "new_l2_block engine timing"
        );

        Ok(header)
    }

    fn execution_payload_from_executable_data(
        &self,
        data: &ExecutableL2Data,
    ) -> EngineApiResult<(MorphExecutionData, MorphHeader)> {
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
        let shanghai_active = self
            .chain_spec
            .is_shanghai_active_at_timestamp(data.timestamp);
        let cancun_active = self
            .chain_spec
            .is_cancun_active_at_timestamp(data.timestamp);
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
                withdrawals_root: shanghai_active.then_some(EMPTY_WITHDRAWALS),
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
                blob_gas_used: cancun_active.then_some(0),
                excess_blob_gas: cancun_active.then_some(0),
                requests_hash: None,
            },
        };
        let body = BlockBody {
            transactions: txs,
            ommers: Default::default(),
            withdrawals: None,
        };

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

    fn ensure_payload_status_acceptable(
        &self,
        status: &alloy_rpc_types_engine::PayloadStatus,
        context: &'static str,
    ) -> EngineApiResult<()> {
        match &status.status {
            alloy_rpc_types_engine::PayloadStatusEnum::Valid
            | alloy_rpc_types_engine::PayloadStatusEnum::Accepted => Ok(()),
            alloy_rpc_types_engine::PayloadStatusEnum::Syncing => {
                Err(MorphEngineApiError::ExecutionFailed(format!(
                    "{context} returned SYNCING for payload"
                )))
            }
            alloy_rpc_types_engine::PayloadStatusEnum::Invalid { validation_error } => {
                Err(MorphEngineApiError::ValidationFailed(format!(
                    "{context} returned INVALID: {validation_error}"
                )))
            }
        }
    }

    const fn engine_api_version() -> EngineApiMessageVersion {
        EngineApiMessageVersion::V1
    }

    fn current_head(&self) -> EngineApiResult<InMemoryHead>
    where
        Provider: HeaderProvider + BlockNumReader,
    {
        if let Some(head) = self.engine_state_tracker.current_head() {
            return Ok(head);
        }

        let number = self
            .provider
            .last_block_number()
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?;
        let header = self
            .provider
            .sealed_header(number)
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?
            .ok_or_else(|| MorphEngineApiError::Internal(format!("header {number} not found")))?;

        let head = InMemoryHead {
            number,
            hash: header.hash(),
            timestamp: header.timestamp(),
        };
        self.engine_state_tracker
            .record_local_head(head.number, head.hash, head.timestamp);
        Ok(head)
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

fn resolve_fcu_block_tag_hash(
    l1_tag_hash: Option<B256>,
    head_hash: B256,
    block_timestamp: u64,
    now_timestamp: u64,
) -> B256 {
    match l1_tag_hash {
        Some(hash) => hash,
        None if now_timestamp.saturating_sub(block_timestamp) > FCU_TAG_FALLBACK_MAX_AGE_SECS => {
            head_hash
        }
        None => B256::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::{Address, Bloom, Bytes};
    use morph_primitives::BlockBody;
    use reth_node_api::ConsensusEngineEvent;
    use reth_primitives_traits::SealedHeader;
    use std::time::Duration;

    fn recovered_with_header(header: MorphHeader) -> RecoveredBlock<Block> {
        let block = Block::new(header, BlockBody::default());
        RecoveredBlock::new_unhashed(block, Vec::new())
    }

    #[test]
    fn test_engine_state_tracker_updates_head_on_canonical_chain_commit() {
        let tracker = EngineStateTracker::default();
        assert!(tracker.current_head().is_none());

        let header = MorphHeader {
            inner: Header {
                number: 42,
                timestamp: 1_700_000_042,
                ..Default::default()
            },
            ..Default::default()
        };
        let sealed_header = SealedHeader::seal_slow(header);
        tracker.on_consensus_engine_event(&ConsensusEngineEvent::CanonicalChainCommitted(
            Box::new(sealed_header.clone()),
            Duration::ZERO,
        ));

        let current_head = tracker.current_head().expect("head should be updated");
        assert_eq!(current_head.number, sealed_header.number());
        assert_eq!(current_head.hash, sealed_header.hash());
        assert_eq!(current_head.timestamp, sealed_header.timestamp());
    }

    #[test]
    fn test_resolve_fcu_block_tag_hash_uses_l1_tag_when_available() {
        let l1_tag = B256::from([0x11; 32]);
        let head = B256::from([0x22; 32]);

        let resolved = resolve_fcu_block_tag_hash(Some(l1_tag), head, 1_700_000_000, 1_700_000_030);

        assert_eq!(resolved, l1_tag);
    }

    #[test]
    fn test_resolve_fcu_block_tag_hash_falls_back_to_head_for_historical_blocks() {
        let head = B256::from([0x33; 32]);

        let resolved = resolve_fcu_block_tag_hash(None, head, 1_700_000_000, 1_700_000_000 + 300);

        assert_eq!(resolved, head);
    }

    #[test]
    fn test_resolve_fcu_block_tag_hash_returns_zero_near_live_without_l1_tag() {
        let head = B256::from([0x44; 32]);

        let resolved = resolve_fcu_block_tag_hash(None, head, 1_700_000_000, 1_700_000_000 + 5);

        assert_eq!(resolved, B256::ZERO);
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
}
