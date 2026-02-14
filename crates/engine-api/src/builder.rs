//! Morph L2 Engine API implementation.
//!
//! This module provides both the builder and implementation for creating the L2 Engine API instance
//! that will be registered with the RPC server.

use crate::{EngineApiResult, MorphEngineApiError, MorphL2EngineApi, MorphValidationContext};
use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256, Sealable};
use alloy_rpc_types_engine::PayloadAttributes;
use dashmap::DashMap;
use morph_chainspec::MorphChainSpec;
use morph_payload_types::{
    AssembleL2BlockParams, ExecutableL2Data, GenericResponse, MorphBuiltPayload,
    MorphPayloadBuilderAttributes, MorphPayloadTypes, SafeL2Data,
};
use morph_primitives::{Block, MorphHeader, MorphPrimitives, MorphReceipt};
use reth_execution_types::ExecutionOutcome;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{
    BlockReader, BlockWriter, CanonChainTracker, DBProvider, DatabaseProviderFactory,
    StateProviderFactory,
};
use std::{sync::Arc, sync::RwLock};

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

    /// Validation context for state root checks.
    validation_ctx: MorphValidationContext,

    /// Cache for validated/executed blocks (keyed by block hash).
    /// Used to avoid re-executing before import and to persist execution artifacts.
    validation_cache: Arc<DashMap<B256, CachedExecutionArtifacts>>,

    /// In-memory head tracking for local sequential imports.
    ///
    /// This bridges the gap before DB persistence is wired, so block `n+1`
    /// can still be validated against an accepted in-process head `n`.
    in_memory_head: Arc<RwLock<Option<InMemoryHead>>>,
}

#[derive(Debug, Clone, Copy)]
struct InMemoryHead {
    number: u64,
    hash: B256,
    timestamp: u64,
}

#[derive(Debug, Clone)]
struct CachedExecutionArtifacts {
    executable_data: ExecutableL2Data,
    executed: reth_payload_primitives::BuiltPayloadExecutedBlock<MorphPrimitives>,
}

impl<Provider> RealMorphL2EngineApi<Provider> {
    /// Creates a new [`RealMorphL2EngineApi`].
    pub fn new(
        provider: Provider,
        payload_builder: PayloadBuilderHandle<MorphPayloadTypes>,
        chain_spec: Arc<MorphChainSpec>,
    ) -> Self {
        let validation_ctx = MorphValidationContext::new(chain_spec.clone());
        Self {
            provider,
            payload_builder,
            chain_spec,
            validation_ctx,
            validation_cache: Arc::new(DashMap::new()),
            in_memory_head: Arc::new(RwLock::new(None)),
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

    fn set_in_memory_head(&self, head: InMemoryHead) {
        if let Ok(mut guard) = self.in_memory_head.write() {
            *guard = Some(head);
        }
    }
}

#[async_trait::async_trait]
impl<Provider> MorphL2EngineApi for RealMorphL2EngineApi<Provider>
where
    Provider: BlockReader<Header = MorphHeader>
        + CanonChainTracker<Header = MorphHeader>
        + DatabaseProviderFactory
        + StateProviderFactory
        + Clone
        + Send
        + Sync
        + 'static,
    <Provider as DatabaseProviderFactory>::ProviderRW:
        BlockWriter<Block = Block, Receipt = MorphReceipt> + DBProvider,
{
    async fn assemble_l2_block(
        &self,
        params: AssembleL2BlockParams,
    ) -> EngineApiResult<ExecutableL2Data> {
        let built_payload = self.build_l2_payload(params, None, None).await?;
        let executable_data = built_payload.executable_data.clone();
        self.cache_built_payload(&built_payload);

        tracing::info!(
            target: "morph::engine",
            block_hash = %executable_data.hash,
            gas_used = executable_data.gas_used,
            tx_count = executable_data.transactions.len(),
            "L2 block assembled successfully"
        );

        Ok(executable_data)
    }

    async fn validate_l2_block(&self, data: ExecutableL2Data) -> EngineApiResult<GenericResponse> {
        tracing::debug!(
            target: "morph::engine",
            block_number = data.number,
            block_hash = %data.hash,
            "validating L2 block"
        );

        // 1. Check if already validated (cached)
        if self.validation_cache.contains_key(&data.hash) {
            tracing::debug!(
                target: "morph::engine",
                block_hash = %data.hash,
                "block already validated (cached)"
            );
            return Ok(GenericResponse { success: true });
        }

        // 2. Enforce canonical continuity against the current head.
        let current_head = self.current_head()?;
        if data.number != current_head.number + 1 {
            tracing::warn!(
                target: "morph::engine",
                expected = current_head.number + 1,
                actual = data.number,
                "cannot validate block with discontinuous block number"
            );
            return Ok(GenericResponse { success: false });
        }

        if data.parent_hash != current_head.hash {
            tracing::warn!(
                target: "morph::engine",
                expected = %current_head.hash,
                actual = %data.parent_hash,
                "parent hash mismatch"
            );
            return Ok(GenericResponse { success: false });
        }

        // 3. Re-build the block with the same inputs and compare results.
        let rebuild_params = AssembleL2BlockParams {
            number: data.number,
            transactions: data.transactions.clone(),
            timestamp: Some(data.timestamp),
        };

        let rebuilt_payload = match self
            .build_l2_payload(rebuild_params, Some(data.gas_limit), data.base_fee_per_gas)
            .await
        {
            Ok(payload) => payload,
            Err(e) => {
                tracing::warn!(
                    target: "morph::engine",
                    error = %e,
                    "failed to rebuild block for validation"
                );
                return Ok(GenericResponse { success: false });
            }
        };
        let rebuilt = rebuilt_payload.executable_data.clone();

        // 4. Compare execution results
        // Check state root if MPTFork is active
        if self
            .validation_ctx
            .should_validate_state_root(data.timestamp)
            && rebuilt.state_root != data.state_root
        {
            tracing::warn!(
                target: "morph::engine",
                expected = %data.state_root,
                actual = %rebuilt.state_root,
                "state root mismatch"
            );
            return Ok(GenericResponse { success: false });
        }

        // Compare other critical fields.
        if rebuilt.gas_used != data.gas_used {
            tracing::warn!(
                target: "morph::engine",
                expected = data.gas_used,
                actual = rebuilt.gas_used,
                tx_count = data.transactions.len(),
                "gas used mismatch"
            );
            return Ok(GenericResponse { success: false });
        }

        if rebuilt.receipts_root != data.receipts_root {
            tracing::warn!(
                target: "morph::engine",
                expected = %data.receipts_root,
                actual = %rebuilt.receipts_root,
                "receipts root mismatch"
            );
            return Ok(GenericResponse { success: false });
        }

        if rebuilt.logs_bloom != data.logs_bloom {
            tracing::warn!(
                target: "morph::engine",
                "logs bloom mismatch"
            );
            return Ok(GenericResponse { success: false });
        }

        if rebuilt.withdraw_trie_root != data.withdraw_trie_root {
            tracing::warn!(
                target: "morph::engine",
                expected = %data.withdraw_trie_root,
                actual = %rebuilt.withdraw_trie_root,
                "withdraw trie root mismatch"
            );
            return Ok(GenericResponse { success: false });
        }

        // 5. Cache validation result with execution artifacts for new_l2_block.
        if let Some(executed) = rebuilt_payload.executed().cloned() {
            self.validation_cache.insert(
                data.hash,
                CachedExecutionArtifacts {
                    executable_data: data.clone(),
                    executed,
                },
            );
        } else {
            tracing::warn!(
                target: "morph::engine",
                block_hash = %data.hash,
                "validated payload missing execution artifacts"
            );
            return Ok(GenericResponse { success: false });
        }

        tracing::debug!(
            target: "morph::engine",
            block_hash = %data.hash,
            "block validation successful"
        );

        Ok(GenericResponse { success: true })
    }

    async fn new_l2_block(
        &self,
        data: ExecutableL2Data,
        batch_hash: Option<B256>,
    ) -> EngineApiResult<()> {
        tracing::info!(
            target: "morph::engine",
            block_number = data.number,
            block_hash = %data.hash,
            ?batch_hash,
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

        // 4. Validate block content before import.
        let validation = self.validate_l2_block(data.clone()).await?;
        if !validation.success {
            return Err(MorphEngineApiError::ValidationFailed(format!(
                "block content validation failed for {}",
                data.hash
            )));
        }

        // 5. Read and consume cached execution artifacts produced during assemble/validate.
        let (_, cached) = self.validation_cache.remove(&data.hash).ok_or_else(|| {
            MorphEngineApiError::Internal(format!(
                "missing cached execution artifacts for block {}",
                data.hash
            ))
        })?;
        if cached.executable_data != data {
            return Err(MorphEngineApiError::ValidationFailed(format!(
                "cached execution data mismatch for block {}",
                data.hash
            )));
        }

        let executed = cached.executed.into_executed_payload();
        let recovered_with_batch =
            self.apply_batch_hash(executed.recovered_block().clone(), batch_hash);
        let recovered_with_data = apply_executable_data_overrides(recovered_with_batch, &data)?;
        let computed_hash = recovered_with_data.hash();
        if computed_hash != data.hash {
            return Err(MorphEngineApiError::ValidationFailed(format!(
                "block hash mismatch: expected {}, computed {}",
                data.hash, computed_hash
            )));
        }

        let (block, senders) = recovered_with_data.split();
        // Keep external block hash as canonical identity for engine API compatibility.
        let recovered_for_import = RecoveredBlock::new(block, senders, data.hash);

        let execution_outcome = ExecutionOutcome::from((
            executed.execution_outcome().clone(),
            executed.block_number(),
        ));
        let hashed_state = Arc::unwrap_or_clone(executed.hashed_state());

        let provider_rw = self
            .provider
            .database_provider_rw()
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?;
        provider_rw
            .append_blocks_with_state(
                vec![recovered_for_import.clone()],
                &execution_outcome,
                hashed_state,
            )
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?;
        provider_rw
            .commit()
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?;

        let sealed_block = recovered_for_import.sealed_block();

        tracing::info!(
            target: "morph::engine",
            block_hash = %sealed_block.hash(),
            block_number = sealed_block.number(),
            "L2 block accepted"
        );

        // Keep reth's canonical in-memory view in sync so eth_blockNumber advances.
        self.provider
            .set_canonical_head(sealed_block.clone_sealed_header());

        self.set_in_memory_head(InMemoryHead {
            number: sealed_block.number(),
            hash: sealed_block.hash(),
            timestamp: sealed_block.timestamp(),
        });

        // Clear cache after successful import. Next block validation will repopulate it.
        self.validation_cache.clear();

        Ok(())
    }

    async fn new_safe_l2_block(&self, data: SafeL2Data) -> EngineApiResult<MorphHeader> {
        tracing::info!(
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
            transactions: data.transactions.clone(),
            timestamp: Some(data.timestamp),
        };

        let built_payload = self
            .build_l2_payload(assemble_params, Some(data.gas_limit), data.base_fee_per_gas)
            .await?;
        let executable_data = built_payload.executable_data.clone();
        self.cache_built_payload(&built_payload);

        // 3. Import the block
        self.new_l2_block(executable_data.clone(), data.batch_hash)
            .await?;

        // 4. Return the persisted header.
        let header = self
            .provider
            .sealed_header(data.number)
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?
            .ok_or_else(|| {
                MorphEngineApiError::Internal(format!(
                    "imported safe block header {} not found",
                    data.number
                ))
            })?
            .into_header();

        tracing::info!(
            target: "morph::engine",
            block_hash = %header.hash_slow(),
            "safe L2 block imported successfully"
        );

        Ok(header)
    }
}

impl<Provider> RealMorphL2EngineApi<Provider> {
    async fn build_l2_payload(
        &self,
        params: AssembleL2BlockParams,
        gas_limit_override: Option<u64>,
        base_fee_override: Option<u128>,
    ) -> EngineApiResult<MorphBuiltPayload>
    where
        Provider: BlockReader<Header = MorphHeader> + Clone + Send + Sync + 'static,
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
                    .unwrap()
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
            transactions: Some(params.transactions.clone()),
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
                MorphEngineApiError::BlockBuildError("failed to receive build response".to_string())
            })?
            .map_err(|e| {
                MorphEngineApiError::BlockBuildError(format!("failed to send build request: {e}"))
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

    fn cache_built_payload(&self, payload: &MorphBuiltPayload) {
        let Some(executed) = payload.executed().cloned() else {
            tracing::warn!(
                target: "morph::engine",
                block_hash = %payload.executable_data.hash,
                "built payload missing execution artifacts"
            );
            return;
        };
        self.validation_cache.insert(
            payload.executable_data.hash,
            CachedExecutionArtifacts {
                executable_data: payload.executable_data.clone(),
                executed,
            },
        );
    }

    fn apply_batch_hash(
        &self,
        recovered_block: RecoveredBlock<Block>,
        batch_hash: Option<B256>,
    ) -> RecoveredBlock<Block> {
        let Some(batch_hash) = batch_hash else {
            return recovered_block;
        };
        let (block, senders) = recovered_block.split();
        let block = block.map_header(|mut header: MorphHeader| {
            header.batch_hash = batch_hash;
            header
        });
        RecoveredBlock::new_unhashed(block, senders)
    }

    fn current_head(&self) -> EngineApiResult<InMemoryHead>
    where
        Provider: BlockReader,
    {
        if let Ok(guard) = self.in_memory_head.read()
            && let Some(head) = *guard
        {
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

        Ok(InMemoryHead {
            number,
            hash: header.hash(),
            timestamp: header.timestamp(),
        })
    }
}

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

// =============================================================================
// Stub Implementation (for testing/fallback)
// =============================================================================

/// Stub implementation of MorphL2EngineApi.
///
/// This is a temporary placeholder that returns errors for all methods.
/// It allows the node to start and register the RPC methods, but the methods
/// will return errors when called.
#[derive(Debug, Clone)]
pub struct StubMorphL2EngineApi;

#[async_trait::async_trait]
impl MorphL2EngineApi for StubMorphL2EngineApi {
    async fn assemble_l2_block(
        &self,
        _params: morph_payload_types::AssembleL2BlockParams,
    ) -> crate::EngineApiResult<morph_payload_types::ExecutableL2Data> {
        tracing::warn!(target: "morph::engine", "assemble_l2_block called on stub implementation");
        Err(crate::MorphEngineApiError::Internal(
            "L2 Engine API not yet implemented".to_string(),
        ))
    }

    async fn validate_l2_block(
        &self,
        _data: morph_payload_types::ExecutableL2Data,
    ) -> crate::EngineApiResult<morph_payload_types::GenericResponse> {
        tracing::warn!(target: "morph::engine", "validate_l2_block called on stub implementation");
        Err(crate::MorphEngineApiError::Internal(
            "L2 Engine API not yet implemented".to_string(),
        ))
    }

    async fn new_l2_block(
        &self,
        _data: morph_payload_types::ExecutableL2Data,
        _batch_hash: Option<alloy_primitives::B256>,
    ) -> crate::EngineApiResult<()> {
        tracing::warn!(target: "morph::engine", "new_l2_block called on stub implementation");
        Err(crate::MorphEngineApiError::Internal(
            "L2 Engine API not yet implemented".to_string(),
        ))
    }

    async fn new_safe_l2_block(
        &self,
        _data: morph_payload_types::SafeL2Data,
    ) -> crate::EngineApiResult<morph_primitives::MorphHeader> {
        tracing::warn!(target: "morph::engine", "new_safe_l2_block called on stub implementation");
        Err(crate::MorphEngineApiError::Internal(
            "L2 Engine API not yet implemented".to_string(),
        ))
    }
}

// =============================================================================
// Legacy Builder (kept for reference, can be removed)
// =============================================================================

use crate::MorphL2EngineRpcHandler;
use reth_node_api::{AddOnsContext, FullNodeComponents};
use reth_node_builder::rpc::EngineApiBuilder;

/// Builder for the Morph L2 Engine API.
///
/// Note: This builder is now superseded by the direct registration in MorphAddOns.
/// It's kept for compatibility but not used in practice.
#[derive(Debug, Default, Clone)]
pub struct MorphL2EngineApiBuilder;

impl<N: FullNodeComponents> EngineApiBuilder<N> for MorphL2EngineApiBuilder {
    type EngineApi = MorphL2EngineRpcHandler<StubMorphL2EngineApi>;

    async fn build_engine_api(self, _ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::EngineApi> {
        // Use stub implementation - real implementation is registered in MorphAddOns
        Ok(MorphL2EngineRpcHandler::new(StubMorphL2EngineApi))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::{Address, Bloom, Bytes};
    use morph_primitives::BlockBody;

    fn recovered_with_header(header: MorphHeader) -> RecoveredBlock<Block> {
        let block = Block::new(header, BlockBody::default());
        RecoveredBlock::new_unhashed(block, Vec::new())
    }

    #[test]
    fn test_apply_executable_data_overrides_aligns_hash_with_engine_data() {
        let source_header: MorphHeader = Header::default().into();
        let source_recovered = recovered_with_header(source_header);

        let target_header = MorphHeader {
            next_l1_msg_index: 42,
            batch_hash: B256::ZERO,
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
            batch_hash: B256::from([0xab; 32]),
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
        let source_batch_hash = source_header.batch_hash;
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
        assert_eq!(header.batch_hash, source_batch_hash);
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
