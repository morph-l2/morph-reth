//! Morph L2 Engine API trait definition.
//!
//! This module defines the L2 Engine API trait that provides methods for
//! building, validating, and importing L2 blocks. These methods are called
//! by the sequencer to produce new blocks.

use crate::EngineApiResult;
use alloy_primitives::B256;
use morph_payload_types::{AssembleL2BlockParams, ExecutableL2Data, GenericResponse, SafeL2Data};
use morph_primitives::MorphHeader;

/// Morph L2 Engine API trait.
///
/// This trait defines the interface for the L2 Engine API, which is used by
/// the sequencer to interact with the execution layer for block production.
///
/// The API is designed to be compatible with the go-ethereum implementation
/// and provides the following methods:
///
/// - `assemble_l2_block`: Build a new L2 block with the given transactions
/// - `validate_l2_block`: Validate an L2 block without importing it
/// - `new_l2_block`: Import and finalize a new L2 block
/// - `new_safe_l2_block`: Import a safe L2 block from derivation
/// - `set_block_tags`: Update safe/finalized block tags without importing a block
#[async_trait::async_trait]
#[auto_impl::auto_impl(Arc, &, Box)]
pub trait MorphL2EngineApi: Send + Sync {
    /// Build a new L2 block with the given transactions.
    ///
    /// This method is called by the sequencer to assemble a new block containing
    /// the provided transactions. The transactions should include L1 messages
    /// at the beginning, followed by L2 transactions.
    ///
    /// # Arguments
    ///
    /// * `params` - The parameters for assembling the block, including:
    ///   - `number`: The expected block number (must be `current_head + 1`)
    ///   - `transactions`: RLP-encoded transactions to include in the block
    ///
    /// # Returns
    ///
    /// Returns the execution result including state root, receipts root, etc.
    async fn assemble_l2_block(
        &self,
        params: AssembleL2BlockParams,
    ) -> EngineApiResult<ExecutableL2Data>;

    /// Validate an L2 block without importing it.
    ///
    /// This method validates a block by forwarding it to the reth engine tree
    /// (`newPayload`) and mapping the returned payload status to a boolean.
    ///
    /// It does not issue a forkchoice update, so it does not advance canonical head.
    ///
    /// # Arguments
    ///
    /// * `data` - The block data to validate, including execution results
    ///
    /// # Returns
    ///
    /// Returns a `GenericResponse` indicating whether validation succeeded.
    async fn validate_l2_block(&self, data: ExecutableL2Data) -> EngineApiResult<GenericResponse>;

    /// Import and finalize a new L2 block.
    ///
    /// This method imports a block through the reth engine pipeline using
    /// `newPayload + forkchoiceUpdated`, which advances the canonical head on success.
    ///
    /// # Arguments
    ///
    /// * `data` - The block data to import
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success.
    async fn new_l2_block(&self, data: ExecutableL2Data) -> EngineApiResult<()>;

    /// Import a safe L2 block from derivation.
    ///
    /// This method is used by the derivation pipeline to import blocks that
    /// have been confirmed on L1. Unlike `new_l2_block`, this method accepts
    /// only the inputs needed to re-execute the block and computes the
    /// execution results.
    ///
    /// # Arguments
    ///
    /// * `data` - The safe block data containing only input fields
    ///
    /// # Returns
    ///
    /// Returns the header of the imported block.
    async fn new_safe_l2_block(&self, data: SafeL2Data) -> EngineApiResult<MorphHeader>;

    /// Set the safe and finalized block tags.
    ///
    /// This method updates the safe and finalized block pointers without
    /// importing any new block. It aligns with go-ethereum's `engine_setBlockTags`.
    ///
    /// Either hash can be `B256::ZERO` to skip updating that tag.
    ///
    /// # Arguments
    ///
    /// * `safe_block_hash` - Hash of the block to mark as safe
    /// * `finalized_block_hash` - Hash of the block to mark as finalized
    async fn set_block_tags(
        &self,
        safe_block_hash: B256,
        finalized_block_hash: B256,
    ) -> EngineApiResult<()>;
}
