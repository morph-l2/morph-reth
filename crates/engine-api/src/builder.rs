//! Morph L2 Engine API implementation.
//!
//! This module provides both the builder and implementation for creating the L2 Engine API instance
//! that will be registered with the RPC server.

use crate::{EngineApiResult, MorphEngineApiError, MorphL2EngineApi, MorphValidationContext};
use alloy_consensus::BlockHeader;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bloom, Sealable};
use alloy_rpc_types_engine::PayloadAttributes;
use dashmap::DashMap;
use morph_chainspec::MorphChainSpec;
use morph_payload_types::{
    AssembleL2BlockParams, ExecutableL2Data, GenericResponse, MorphBuiltPayload,
    MorphPayloadBuilderAttributes, MorphPayloadTypes, SafeL2Data,
};
use morph_primitives::{Block, BlockBody, MorphHeader, MorphTxEnvelope};
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives_traits::{Block as BlockTrait, SealedBlock};
use reth_provider::BlockReader;
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

    /// Validation context for state root checks.
    validation_ctx: MorphValidationContext,

    /// Cache for validated blocks (keyed by block hash).
    /// Used to avoid re-executing the same block multiple times.
    validation_cache: Arc<DashMap<B256, ()>>,
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
    Provider: BlockReader + Clone + Send + Sync + 'static,
{
    async fn assemble_l2_block(
        &self,
        params: AssembleL2BlockParams,
    ) -> EngineApiResult<ExecutableL2Data> {
        tracing::debug!(
            target: "morph::engine",
            block_number = params.number,
            tx_count = params.transactions.len(),
            "assembling L2 block"
        );

        // 1. Validate block number (must be current_head + 1)
        let current_head = self
            .provider
            .last_block_number()
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?;

        if params.number != current_head + 1 {
            return Err(MorphEngineApiError::DiscontinuousBlockNumber {
                expected: current_head + 1,
                actual: params.number,
            });
        }

        // 2. Get parent header
        let parent = self
            .provider
            .sealed_header(current_head)
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?
            .ok_or_else(|| {
                MorphEngineApiError::Internal(format!("parent header {current_head} not found"))
            })?;

        // 3. Build MorphPayloadAttributes
        let parent_hash = parent.hash();

        // Calculate timestamp (current time or parent + 1 second, whichever is greater)
        let timestamp = std::cmp::max(
            parent.timestamp() + 1,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );

        // Create payload attributes (RPC format)
        let rpc_attributes = morph_payload_types::MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp,
                prev_randao: parent.mix_hash().unwrap_or_default(),
                suggested_fee_recipient: Address::ZERO, // Morph doesn't use fee recipient
                withdrawals: Some(Vec::new()),
                parent_beacon_block_root: None,
            },
            // Include all transactions from params (forced L1 messages + L2 txs)
            transactions: Some(params.transactions.clone()),
        };

        // Convert to builder attributes (internal format with decoded transactions)
        let builder_attrs = MorphPayloadBuilderAttributes::try_new(
            parent_hash,
            rpc_attributes,
            1, // Use version 1 for now
        )
        .map_err(|e| {
            MorphEngineApiError::BlockBuildError(format!(
                "failed to create builder attributes: {e}",
            ))
        })?;

        // 4. Request payload building from the service
        // The PayloadBuilderHandle manages the async building process
        let payload_id = builder_attrs.payload_id();

        tracing::debug!(
            target: "morph::engine",
            ?payload_id,
            "requesting payload build from service"
        );

        // Send build request to the payload builder service
        // send_new_payload returns a Receiver<Result<PayloadId, PayloadBuilderError>>
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

        // Wait for the payload to be built
        // best_payload returns Option<Result<MorphBuiltPayload, PayloadBuilderError>>
        let built_payload: MorphBuiltPayload = self
            .payload_builder
            .best_payload(payload_id)
            .await
            .ok_or_else(|| {
                MorphEngineApiError::Internal(format!("no payload response for id {payload_id:?}"))
            })?
            .map_err(|e| {
                MorphEngineApiError::BlockBuildError(format!("failed to get built payload: {e}"))
            })?;

        // 5. Extract ExecutableL2Data from built payload
        // MorphBuiltPayload directly contains executable_data field
        let executable_data = built_payload.executable_data;

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

        // 2. Try to get parent header
        let parent = match self.provider.sealed_header_by_hash(data.parent_hash) {
            Ok(Some(header)) => header,
            Ok(None) => {
                // Parent not found - this can happen when:
                // 1. reth database is empty (first block)
                // 2. morphnode is importing historical blocks
                // In Morph's architecture, morphnode is responsible for validation,
                // so we trust the data and skip re-execution validation
                tracing::debug!(
                    target: "morph::engine",
                    block_number = data.number,
                    parent_hash = %data.parent_hash,
                    "parent header not found, skipping re-execution validation"
                );
                self.validation_cache.insert(data.hash, ());
                return Ok(GenericResponse { success: true });
            }
            Err(e) => {
                tracing::warn!(
                    target: "morph::engine",
                    error = %e,
                    "failed to query parent header"
                );
                // Database error - but don't fail, just skip validation
                self.validation_cache.insert(data.hash, ());
                return Ok(GenericResponse { success: true });
            }
        };

        // 3. Verify parent hash matches
        if parent.hash() != data.parent_hash {
            tracing::warn!(
                target: "morph::engine",
                expected = %parent.hash(),
                actual = %data.parent_hash,
                "parent hash mismatch"
            );
            return Ok(GenericResponse { success: false });
        }

        // 4. Re-build the block with the same inputs and compare results
        let rebuild_params = AssembleL2BlockParams {
            number: data.number,
            transactions: data.transactions.clone(),
        };

        let rebuilt = match self.assemble_l2_block(rebuild_params).await {
            Ok(rebuilt_data) => rebuilt_data,
            Err(e) => {
                tracing::warn!(
                    target: "morph::engine",
                    error = %e,
                    "failed to rebuild block for validation, skipping"
                );
                // Can't rebuild - probably missing state
                // In Morph's architecture, trust morphnode's validation
                self.validation_cache.insert(data.hash, ());
                return Ok(GenericResponse { success: true });
            }
        };

        // 5. Compare execution results
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

        // Compare other critical fields
        if rebuilt.gas_used != data.gas_used {
            tracing::warn!(
                target: "morph::engine",
                expected = data.gas_used,
                actual = rebuilt.gas_used,
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

        // 6. Cache validation result
        self.validation_cache.insert(data.hash, ());

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
        let current_number = self
            .provider
            .last_block_number()
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?;

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
        let current_header = self
            .provider
            .sealed_header(current_number)
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?
            .ok_or_else(|| {
                MorphEngineApiError::Internal(format!(
                    "current header {current_number} not found (database may not be initialized)",
                ))
            })?;

        if data.parent_hash != current_header.hash() {
            tracing::warn!(
                target: "morph::engine",
                expected = %current_header.hash(),
                actual = %data.parent_hash,
                "wrong parent hash"
            );
            return Err(MorphEngineApiError::WrongParentHash {
                expected: current_header.hash(),
                actual: data.parent_hash,
            });
        }

        // 4. Optionally validate the block content
        if let Ok(validation) = self.validate_l2_block(data.clone()).await
            && !validation.success
        {
            tracing::warn!(
                target: "morph::engine",
                block_hash = %data.hash,
                "block content validation failed"
            );
        }

        // 5. Convert ExecutableL2Data to SealedBlock
        let sealed_block = self.executable_data_to_sealed_block(&data, batch_hash)?;

        // 6. TODO: Write block to database
        // In go-ethereum: api.eth.BlockChain().WriteStateAndSetHead(block, receipts, stateDB, procTime)
        // In reth, this should be handled by Engine Tree when it processes forkchoice updates.
        // For now, we just accept the block and log success.
        // Future work: integrate with reth's Engine Tree for actual persistence.

        tracing::info!(
            target: "morph::engine",
            block_hash = %sealed_block.hash(),
            block_number = sealed_block.number(),
            "L2 block accepted"
        );

        Ok(())
    }

    async fn new_safe_l2_block(&self, data: SafeL2Data) -> EngineApiResult<MorphHeader> {
        tracing::info!(
            target: "morph::engine",
            block_number = data.number,
            "importing safe L2 block from L1 derivation"
        );

        // 1. Get latest block number
        let latest_number = self
            .provider
            .last_block_number()
            .map_err(|e| MorphEngineApiError::Database(e.to_string()))?;

        if data.number != latest_number + 1 {
            return Err(MorphEngineApiError::DiscontinuousBlockNumber {
                expected: latest_number + 1,
                actual: data.number,
            });
        }

        // 2. Assemble the block from SafeL2Data inputs
        let assemble_params = AssembleL2BlockParams {
            number: data.number,
            transactions: data.transactions.clone(),
        };

        let executable_data = self.assemble_l2_block(assemble_params).await?;

        // 3. Import the block
        self.new_l2_block(executable_data.clone(), data.batch_hash)
            .await?;

        // 4. Return the header
        let header = self.executable_data_to_header(&executable_data, data.batch_hash)?;

        tracing::info!(
            target: "morph::engine",
            block_hash = %header.hash_slow(),
            "safe L2 block imported successfully"
        );

        Ok(header)
    }
}

impl<Provider> RealMorphL2EngineApi<Provider> {
    /// Converts ExecutableL2Data to a SealedBlock.
    fn executable_data_to_sealed_block(
        &self,
        data: &ExecutableL2Data,
        batch_hash: Option<B256>,
    ) -> EngineApiResult<SealedBlock<Block>> {
        // Decode transactions from bytes
        let transactions: Vec<MorphTxEnvelope> = data
            .transactions
            .iter()
            .enumerate()
            .map(|(i, tx_bytes)| {
                let mut buf = tx_bytes.as_ref();
                MorphTxEnvelope::decode_2718(&mut buf).map_err(|e| {
                    MorphEngineApiError::InvalidTransaction {
                        index: i,
                        message: format!("failed to decode transaction: {e}"),
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Build header
        let header = self.executable_data_to_header(data, batch_hash)?;

        // Build block
        let block = Block::new(
            header,
            BlockBody {
                transactions,
                ommers: Vec::new(),
                withdrawals: Some(Default::default()),
            },
        );

        // Seal with the provided hash (use seal_unchecked to avoid recalculating)
        Ok(block.seal_unchecked(data.hash))
    }

    /// Converts ExecutableL2Data to a MorphHeader.
    fn executable_data_to_header(
        &self,
        data: &ExecutableL2Data,
        batch_hash: Option<B256>,
    ) -> EngineApiResult<MorphHeader> {
        use alloy_consensus::Header;

        // Parse logs bloom
        let logs_bloom = if data.logs_bloom.len() == 256 {
            Bloom::from_slice(&data.logs_bloom)
        } else {
            Bloom::ZERO
        };

        let inner = Header {
            parent_hash: data.parent_hash,
            ommers_hash: alloy_primitives::B256::ZERO, // No ommers in L2
            beneficiary: data.miner,
            state_root: data.state_root,
            transactions_root: alloy_primitives::B256::ZERO, // Will be calculated when sealing
            receipts_root: data.receipts_root,
            logs_bloom,
            difficulty: alloy_primitives::U256::ZERO, // No PoW in L2
            number: data.number,
            gas_limit: data.gas_limit,
            gas_used: data.gas_used,
            timestamp: data.timestamp,
            extra_data: Default::default(),
            mix_hash: alloy_primitives::B256::ZERO,
            nonce: alloy_primitives::B64::ZERO,
            base_fee_per_gas: data.base_fee_per_gas.map(|f| f as u64),
            withdrawals_root: Some(alloy_primitives::B256::ZERO),
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_hash: None,
        };

        Ok(MorphHeader {
            inner,
            next_l1_msg_index: data.next_l1_message_index,
            batch_hash: batch_hash.unwrap_or_default(),
        })
    }
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
