//! Morph engine validator.

use crate::MorphNode;
use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, keccak256};
use dashmap::DashMap;
use morph_chainspec::{
    L2_MESSAGE_QUEUE_ADDRESS, L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT, MorphChainSpec,
};
use morph_payload_types::{MorphExecutionData, MorphPayloadTypes};
use morph_primitives::MorphHeader;
use parking_lot::Mutex;
use reth_chainspec::EthChainSpec;
use reth_errors::ConsensusError;
use reth_node_api::{
    AddOnsContext, FullNodeComponents, InvalidPayloadAttributesError, NewPayloadError, NodeTypes,
    PayloadAttributes, PayloadTypes, PayloadValidator,
};
use reth_node_builder::{
    invalid_block_hook::InvalidBlockHookExt,
    rpc::{EngineValidatorBuilder, PayloadValidatorBuilder},
};
use reth_primitives_traits::{RecoveredBlock, SealedBlock};
use reth_provider::ChainSpecProvider;
use reth_tracing::tracing;
use std::{collections::VecDeque, sync::Arc};

/// Builder for Morph engine validator (payload validation).
///
/// Creates a validator for validating engine API payloads.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct MorphEngineValidatorBuilder;

impl<Node> PayloadValidatorBuilder<Node> for MorphEngineValidatorBuilder
where
    Node: FullNodeComponents<Types = MorphNode>,
    Node::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
{
    type Validator = MorphEngineValidator;

    async fn build(self, _ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(MorphEngineValidator::new())
    }
}

/// Builder for Morph tree engine validator.
///
/// This wires [`MorphEngineValidator`] into both payload validation and state-root
/// decision/validation hooks.
#[derive(Debug, Clone)]
pub struct MorphTreeEngineValidatorBuilder<PVB = MorphEngineValidatorBuilder> {
    payload_validator_builder: PVB,
}

impl<PVB> MorphTreeEngineValidatorBuilder<PVB> {
    /// Creates a new instance with the given payload validator builder.
    pub const fn new(payload_validator_builder: PVB) -> Self {
        Self {
            payload_validator_builder,
        }
    }
}

impl<PVB> Default for MorphTreeEngineValidatorBuilder<PVB>
where
    PVB: Default,
{
    fn default() -> Self {
        Self::new(PVB::default())
    }
}

impl<Node, PVB> EngineValidatorBuilder<Node> for MorphTreeEngineValidatorBuilder<PVB>
where
    Node: FullNodeComponents<
            Types = MorphNode,
            Evm: reth_node_api::ConfigureEngineEvm<
                <<Node::Types as NodeTypes>::Payload as PayloadTypes>::ExecutionData,
            >,
        >,
    Node::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    PVB: PayloadValidatorBuilder<Node>,
    PVB::Validator: reth_node_api::PayloadValidator<
            <Node::Types as NodeTypes>::Payload,
            Block = reth_node_api::BlockTy<Node::Types>,
        > + Clone,
{
    type EngineValidator =
        morph_engine_tree_ext::MorphBasicEngineValidator<Node::Provider, Node::Evm, PVB::Validator>;

    async fn build_tree_validator(
        self,
        ctx: &AddOnsContext<'_, Node>,
        tree_config: reth_node_api::TreeConfig,
        changeset_cache: reth_trie_db::ChangesetCache,
    ) -> eyre::Result<Self::EngineValidator> {
        let validator = self.payload_validator_builder.build(ctx).await?;
        let data_dir = ctx
            .config
            .datadir
            .clone()
            .resolve_datadir(ctx.config.chain.chain());
        let invalid_block_hook = ctx.create_invalid_block_hook(&data_dir).await?;
        let chain_spec = ctx.node.provider().chain_spec();

        Ok(morph_engine_tree_ext::MorphBasicEngineValidator::new(
            ctx.node.provider().clone(),
            Arc::new(ctx.node.consensus().clone()),
            ctx.node.evm_config().clone(),
            validator,
            tree_config,
            invalid_block_hook,
            changeset_cache,
            ctx.node.task_executor().clone(),
            chain_spec,
        ))
    }
}

/// Morph engine validator for payload validation.
///
/// This validator is used by the engine API to validate incoming payloads.
/// For Morph, most validation is deferred to the consensus layer.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct MorphEngineValidator {
    expected_withdraw_trie_roots: Arc<DashMap<B256, WithdrawTrieRootExpectation>>,
    expected_withdraw_trie_root_order: Arc<Mutex<VecDeque<B256>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WithdrawTrieRootExpectation {
    SkipValidation,
    Verify(B256),
}

impl MorphEngineValidator {
    const MAX_EXPECTED_WITHDRAW_TRIE_ROOTS: usize = 4096;

    /// Creates a new [`MorphEngineValidator`].
    pub fn new() -> Self {
        Self::default()
    }

    fn record_withdraw_trie_root_expectation(
        &self,
        block_hash: B256,
        expectation: WithdrawTrieRootExpectation,
    ) {
        let is_new_entry = self
            .expected_withdraw_trie_roots
            .insert(block_hash, expectation)
            .is_none();

        if is_new_entry {
            let mut order = self.expected_withdraw_trie_root_order.lock();
            order.push_back(block_hash);

            while self.expected_withdraw_trie_roots.len() > Self::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS {
                let Some(evicted_hash) = order.pop_front() else {
                    break;
                };
                self.expected_withdraw_trie_roots.remove(&evicted_hash);
            }
        }
    }

    fn take_withdraw_trie_root_expectation(
        &self,
        block_hash: B256,
    ) -> Option<WithdrawTrieRootExpectation> {
        let removed = self
            .expected_withdraw_trie_roots
            .remove(&block_hash)
            .map(|(_, expected)| expected);

        if removed.is_some() {
            self.expected_withdraw_trie_root_order
                .lock()
                .retain(|hash| *hash != block_hash);
        }

        removed
    }

    fn updated_withdraw_trie_root_from_hashed_state(
        state_updates: &reth_trie::HashedPostState,
    ) -> Option<B256> {
        let hashed_address = keccak256(L2_MESSAGE_QUEUE_ADDRESS);
        let hashed_slot = keccak256(B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT));

        state_updates
            .storages
            .get(&hashed_address)
            .and_then(|storage| storage.storage.get(&hashed_slot).copied())
            .map(B256::from)
    }
}

impl PayloadValidator<MorphPayloadTypes> for MorphEngineValidator {
    type Block = morph_primitives::Block;

    fn convert_payload_to_block(
        &self,
        payload: MorphExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        let expected_withdraw_trie_root = payload.expected_withdraw_trie_root;
        let sealed_block = Arc::unwrap_or_clone(payload.block);

        let expectation = expected_withdraw_trie_root
            .map(WithdrawTrieRootExpectation::Verify)
            .unwrap_or(WithdrawTrieRootExpectation::SkipValidation);
        self.record_withdraw_trie_root_expectation(sealed_block.hash(), expectation);

        Ok(sealed_block)
    }

    fn validate_block_post_execution_with_hashed_state(
        &self,
        state_updates: &reth_trie::HashedPostState,
        block: &RecoveredBlock<Self::Block>,
    ) -> Result<(), ConsensusError> {
        let Some(expectation) = self.take_withdraw_trie_root_expectation(block.hash()) else {
            // No CL-supplied expectation. Reachable on the Block-input path
            // (P2P-downloaded blocks, pipeline backfill, and buffered blocks
            // whose expectation was evicted from the bounded LRU before
            // reattach) — `convert_payload_to_block` was never invoked to
            // register one. Treat as SkipValidation so sync isn't stalled;
            // the strict post-Jade state-root check upstream still covers
            // withdraw-trie consistency through state-root equality.
            tracing::debug!(
                target: "morph::engine_validator",
                block_hash = %block.hash(),
                "no withdraw trie root expectation registered; skipping CL cross-check"
            );
            return Ok(());
        };
        let WithdrawTrieRootExpectation::Verify(expected_withdraw_trie_root) = expectation else {
            return Ok(());
        };

        // Only validate if the withdraw trie root slot was actually updated in this block.
        // If the slot is absent from hashed_state, the root is unchanged from the parent —
        // the consensus layer guarantees the expected value is correct in that case.
        // Doing a DB read for the parent state here would be expensive (history_by_block_hash
        // + storage lookup) and would occur while holding the execution cache write lock,
        // causing lock contention with the next block's cache lookup.
        let Some(actual_withdraw_trie_root) =
            Self::updated_withdraw_trie_root_from_hashed_state(state_updates)
        else {
            return Ok(());
        };

        if actual_withdraw_trie_root != expected_withdraw_trie_root {
            return Err(ConsensusError::msg(format!(
                "withdraw trie root mismatch: expected {expected_withdraw_trie_root}, got {actual_withdraw_trie_root}"
            )));
        }

        Ok(())
    }

    fn validate_payload_attributes_against_header(
        &self,
        attr: &<MorphPayloadTypes as reth_node_api::PayloadTypes>::PayloadAttributes,
        header: &MorphHeader,
    ) -> Result<(), InvalidPayloadAttributesError> {
        // Ensure that payload attributes timestamp is not in the past
        if attr.timestamp() < header.timestamp() {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use reth_trie::{HashedPostState, HashedStorage};

    #[test]
    fn test_extract_updated_withdraw_trie_root_from_hashed_state() {
        let expected = B256::from([0x11; 32]);
        let hashed_address = keccak256(L2_MESSAGE_QUEUE_ADDRESS);
        let hashed_slot = keccak256(B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT));

        let state = HashedPostState::from_hashed_storage(
            hashed_address,
            HashedStorage::from_iter(false, [(hashed_slot, U256::from_be_bytes(expected.0))]),
        );

        assert_eq!(
            MorphEngineValidator::updated_withdraw_trie_root_from_hashed_state(&state),
            Some(expected)
        );
    }

    #[test]
    fn test_extract_updated_withdraw_trie_root_from_hashed_state_missing_slot() {
        let state = HashedPostState::default();
        assert_eq!(
            MorphEngineValidator::updated_withdraw_trie_root_from_hashed_state(&state),
            None
        );
    }

    #[test]
    fn test_withdraw_trie_root_expectation_cache_evicts_incrementally_not_clear_all() {
        let validator = MorphEngineValidator::new();
        let key = |n: usize| {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&(n as u64).to_be_bytes());
            B256::from(bytes)
        };

        for i in 0..MorphEngineValidator::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS {
            validator.record_withdraw_trie_root_expectation(
                key(i),
                WithdrawTrieRootExpectation::Verify(B256::from([0xaa; 32])),
            );
        }
        assert_eq!(
            validator.expected_withdraw_trie_roots.len(),
            MorphEngineValidator::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS
        );

        let oldest = key(0);
        let newest = key(MorphEngineValidator::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS);
        validator.record_withdraw_trie_root_expectation(
            newest,
            WithdrawTrieRootExpectation::Verify(B256::from([0xbb; 32])),
        );

        assert_eq!(
            validator.expected_withdraw_trie_roots.len(),
            MorphEngineValidator::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS
        );
        assert!(!validator.expected_withdraw_trie_roots.is_empty());
        assert!(
            validator
                .expected_withdraw_trie_roots
                .get(&newest)
                .is_some()
        );
        assert!(
            validator
                .expected_withdraw_trie_roots
                .get(&oldest)
                .is_none()
        );
    }

    #[test]
    fn test_record_and_take_expectation_roundtrip() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x42; 32]);
        let expected_root = B256::from([0xee; 32]);

        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::Verify(expected_root),
        );

        // Take should return the expectation and remove it
        let result = validator.take_withdraw_trie_root_expectation(hash);
        assert_eq!(
            result,
            Some(WithdrawTrieRootExpectation::Verify(expected_root))
        );

        // Taking again should return None
        assert!(
            validator
                .take_withdraw_trie_root_expectation(hash)
                .is_none()
        );
    }

    #[test]
    fn test_record_skip_validation_expectation() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x99; 32]);

        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::SkipValidation,
        );

        let result = validator.take_withdraw_trie_root_expectation(hash);
        assert_eq!(result, Some(WithdrawTrieRootExpectation::SkipValidation));
    }

    #[test]
    fn test_duplicate_record_overwrites_value() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x11; 32]);
        let root1 = B256::from([0xaa; 32]);
        let root2 = B256::from([0xbb; 32]);

        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::Verify(root1),
        );
        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::Verify(root2),
        );

        let result = validator.take_withdraw_trie_root_expectation(hash);
        assert_eq!(result, Some(WithdrawTrieRootExpectation::Verify(root2)));
    }

    #[test]
    fn test_take_nonexistent_returns_none() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0xff; 32]);
        assert!(
            validator
                .take_withdraw_trie_root_expectation(hash)
                .is_none()
        );
    }

    #[test]
    fn test_updated_withdraw_trie_root_wrong_address() {
        // If storage update is for a different address, should return None
        let wrong_address = keccak256(alloy_primitives::Address::ZERO);
        let hashed_slot = keccak256(B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT));
        let state = HashedPostState::from_hashed_storage(
            wrong_address,
            HashedStorage::from_iter(false, [(hashed_slot, U256::from_be_bytes([0x11; 32]))]),
        );
        assert!(
            MorphEngineValidator::updated_withdraw_trie_root_from_hashed_state(&state).is_none()
        );
    }

    #[test]
    fn test_updated_withdraw_trie_root_wrong_slot() {
        // Correct address but wrong slot
        let hashed_address = keccak256(L2_MESSAGE_QUEUE_ADDRESS);
        let wrong_slot = keccak256(B256::from(alloy_primitives::U256::from(999)));
        let state = HashedPostState::from_hashed_storage(
            hashed_address,
            HashedStorage::from_iter(false, [(wrong_slot, U256::from_be_bytes([0x22; 32]))]),
        );
        assert!(
            MorphEngineValidator::updated_withdraw_trie_root_from_hashed_state(&state).is_none()
        );
    }

    fn empty_recovered_block_with_hash(
        hash: B256,
    ) -> reth_primitives_traits::RecoveredBlock<morph_primitives::Block> {
        let header = MorphHeader::default();
        let body = morph_primitives::BlockBody::default();
        let block = morph_primitives::Block::new(header, body);
        let sealed = reth_primitives_traits::SealedBlock::new_unchecked(block, hash);
        reth_primitives_traits::RecoveredBlock::new_sealed(sealed, Vec::new())
    }

    /// Block-input path (P2P sync, pipeline backfill) reaches
    /// `validate_block_post_execution_with_hashed_state` without calling
    /// `convert_payload_to_block`, so no expectation is registered. The
    /// validator must treat the missing entry as `SkipValidation` and
    /// return `Ok` — otherwise sync stalls. The upstream strict state-root
    /// check (post-Jade) remains the source of truth.
    #[test]
    fn validate_block_post_execution_skips_when_no_expectation_registered() {
        let validator = MorphEngineValidator::new();
        let block = empty_recovered_block_with_hash(B256::from([0xab; 32]));

        let result = validator
            .validate_block_post_execution_with_hashed_state(&HashedPostState::default(), &block);

        assert!(
            result.is_ok(),
            "missing expectation must be treated as SkipValidation, got {:?}",
            result.err()
        );
    }

    /// SkipValidation expectation (CL didn't supply a value) must be honored.
    #[test]
    fn validate_block_post_execution_honors_skip_validation_expectation() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0xcd; 32]);
        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::SkipValidation,
        );
        let block = empty_recovered_block_with_hash(hash);

        let result = validator
            .validate_block_post_execution_with_hashed_state(&HashedPostState::default(), &block);

        assert!(result.is_ok());
        // expectation must be consumed.
        assert!(
            validator
                .take_withdraw_trie_root_expectation(hash)
                .is_none()
        );
    }

    /// Verify expectation: when the slot wasn't touched we trust CL (no DB read)
    /// and pass through.
    #[test]
    fn validate_block_post_execution_passes_when_slot_unchanged() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x33; 32]);
        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::Verify(B256::from([0xee; 32])),
        );
        let block = empty_recovered_block_with_hash(hash);

        // empty hashed state → no withdraw-slot diff → skip per the doc-comment
        // explanation in the validator.
        let result = validator
            .validate_block_post_execution_with_hashed_state(&HashedPostState::default(), &block);

        assert!(result.is_ok());
    }

    /// Verify expectation that mismatches an actually-updated slot must fail.
    #[test]
    fn validate_block_post_execution_rejects_mismatched_root() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x55; 32]);
        let expected = B256::from([0xee; 32]);
        let actual = B256::from([0xff; 32]);
        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::Verify(expected),
        );

        let hashed_address = keccak256(L2_MESSAGE_QUEUE_ADDRESS);
        let hashed_slot = keccak256(B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT));
        let state = HashedPostState::from_hashed_storage(
            hashed_address,
            HashedStorage::from_iter(false, [(hashed_slot, U256::from_be_bytes(actual.0))]),
        );

        let block = empty_recovered_block_with_hash(hash);
        let err = validator
            .validate_block_post_execution_with_hashed_state(&state, &block)
            .expect_err("mismatched root must fail");
        assert!(
            err.to_string().contains("withdraw trie root mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_payload_attributes_timestamp_not_in_past() {
        use alloy_rpc_types_engine::PayloadAttributes;
        use morph_payload_types::MorphPayloadAttributes;
        use reth_node_api::PayloadValidator;

        let validator = MorphEngineValidator::new();

        // Create a header with timestamp 100
        let parent_header = MorphHeader {
            inner: alloy_consensus::Header {
                timestamp: 100,
                ..Default::default()
            },
            ..Default::default()
        };
        let parent = reth_primitives_traits::SealedHeader::seal_slow(parent_header);

        // Attributes with timestamp = 99 (before parent) should fail
        let attr = MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp: 99,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: alloy_primitives::Address::ZERO,
                withdrawals: None,
                parent_beacon_block_root: None,
                slot_number: None,
            },
            transactions: None,
            gas_limit: None,
            base_fee_per_gas: None,
        };
        assert!(
            validator
                .validate_payload_attributes_against_header(&attr, parent.header())
                .is_err()
        );

        // Attributes with timestamp = 100 (equal to parent) should pass
        let attr_same = MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp: 100,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: alloy_primitives::Address::ZERO,
                withdrawals: None,
                parent_beacon_block_root: None,
                slot_number: None,
            },
            transactions: None,
            gas_limit: None,
            base_fee_per_gas: None,
        };
        assert!(
            validator
                .validate_payload_attributes_against_header(&attr_same, parent.header())
                .is_ok()
        );

        // Attributes with timestamp = 101 (after parent) should pass
        let attr_future = MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp: 101,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: alloy_primitives::Address::ZERO,
                withdrawals: None,
                parent_beacon_block_root: None,
                slot_number: None,
            },
            transactions: None,
            gas_limit: None,
            base_fee_per_gas: None,
        };
        assert!(
            validator
                .validate_payload_attributes_against_header(&attr_future, parent.header())
                .is_ok()
        );
    }
}
