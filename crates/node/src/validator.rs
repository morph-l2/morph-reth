//! Morph engine validator.

use crate::MorphNode;
use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, keccak256};
use dashmap::DashMap;
use morph_chainspec::{
    L2_MESSAGE_QUEUE_ADDRESS, L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT, MorphChainSpec,
    MorphHardforks,
};
use morph_payload_types::{MorphExecutionData, MorphPayloadTypes};
use morph_primitives::MorphHeader;
use reth_errors::ConsensusError;
use reth_node_api::{
    AddOnsContext, FullNodeComponents, InvalidPayloadAttributesError, NewPayloadError,
    PayloadAttributes, PayloadValidator, StateRootValidationOutcome,
};
use reth_node_builder::rpc::PayloadValidatorBuilder;
use reth_primitives_traits::{GotExpected, RecoveredBlock, SealedBlock};
use reth_provider::{ChainSpecProvider, StateProvider, StateProviderFactory};
use std::{collections::VecDeque, sync::Arc, sync::Mutex};

/// Builder for Morph engine validator (payload validation).
///
/// Creates a validator for validating engine API payloads.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct MorphEngineValidatorBuilder;

impl<Node> PayloadValidatorBuilder<Node> for MorphEngineValidatorBuilder
where
    Node: FullNodeComponents<Types = MorphNode>,
    Node::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec> + StateProviderFactory + Clone,
{
    type Validator = MorphEngineValidator<Node::Provider>;

    async fn build(self, ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(MorphEngineValidator::new(
            ctx.node.provider().chain_spec(),
            ctx.node.provider().clone(),
        ))
    }
}

/// Morph engine validator for payload validation.
///
/// This validator is used by the engine API to validate incoming payloads.
/// For Morph, most validation is deferred to the consensus layer.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MorphEngineValidator<P> {
    chain_spec: Arc<MorphChainSpec>,
    provider: P,
    expected_withdraw_trie_roots: Arc<DashMap<B256, WithdrawTrieRootExpectation>>,
    expected_withdraw_trie_root_order: Arc<Mutex<VecDeque<B256>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WithdrawTrieRootExpectation {
    SkipValidation,
    Verify(B256),
}

impl<P> MorphEngineValidator<P> {
    const MAX_EXPECTED_WITHDRAW_TRIE_ROOTS: usize = 4096;

    /// Creates a new [`MorphEngineValidator`].
    pub fn new(chain_spec: Arc<MorphChainSpec>, provider: P) -> Self {
        Self {
            chain_spec,
            provider,
            expected_withdraw_trie_roots: Arc::new(DashMap::new()),
            expected_withdraw_trie_root_order: Arc::new(Mutex::new(VecDeque::new())),
        }
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
            let mut order = self
                .expected_withdraw_trie_root_order
                .lock()
                .expect("withdraw trie root expectation order mutex poisoned");
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

        if removed.is_some()
            && let Ok(mut order) = self.expected_withdraw_trie_root_order.lock()
        {
            order.retain(|hash| *hash != block_hash);
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

impl<P> MorphEngineValidator<P>
where
    P: StateProviderFactory,
{
    fn parent_withdraw_trie_root(&self, parent_hash: B256) -> Result<B256, ConsensusError> {
        let parent_state = self
            .provider
            .history_by_block_hash(parent_hash)
            .map_err(|err| {
                ConsensusError::Other(format!(
                    "failed to open parent state for withdraw trie root check: {err}"
                ))
            })?;

        let value = parent_state
            .storage(
                L2_MESSAGE_QUEUE_ADDRESS,
                B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT),
            )
            .map_err(|err| {
                ConsensusError::Other(format!(
                    "failed to read withdraw trie root from parent state: {err}"
                ))
            })?
            .unwrap_or_default();

        Ok(B256::from(value))
    }
}

impl<P> PayloadValidator<MorphPayloadTypes> for MorphEngineValidator<P>
where
    P: StateProviderFactory + Send + Sync + Unpin + 'static,
{
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
            return Err(ConsensusError::Other(format!(
                "missing withdraw trie root expectation cache entry for block {}",
                block.hash()
            )));
        };
        let WithdrawTrieRootExpectation::Verify(expected_withdraw_trie_root) = expectation else {
            return Ok(());
        };

        let actual_withdraw_trie_root = if let Some(updated_withdraw_trie_root) =
            Self::updated_withdraw_trie_root_from_hashed_state(state_updates)
        {
            updated_withdraw_trie_root
        } else {
            self.parent_withdraw_trie_root(block.parent_hash())?
        };

        if actual_withdraw_trie_root != expected_withdraw_trie_root {
            return Err(ConsensusError::Other(format!(
                "withdraw trie root mismatch: expected {expected_withdraw_trie_root}, got {actual_withdraw_trie_root}"
            )));
        }

        Ok(())
    }

    fn validate_computed_state_root(
        &self,
        block: &RecoveredBlock<Self::Block>,
        computed_state_root: B256,
    ) -> Result<StateRootValidationOutcome, ConsensusError> {
        if !self
            .chain_spec
            .is_mpt_fork_active_at_timestamp(block.header().timestamp())
        {
            return Ok(StateRootValidationOutcome::Skipped);
        }

        let header_state_root = block.header().state_root();
        if computed_state_root == header_state_root {
            Ok(StateRootValidationOutcome::Valid)
        } else {
            Err(ConsensusError::BodyStateRootDiff(
                GotExpected {
                    got: computed_state_root,
                    expected: header_state_root,
                }
                .into(),
            ))
        }
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
    use morph_chainspec::MORPH_HOODI;
    use reth_trie::{HashedPostState, HashedStorage};
    use std::sync::Arc;

    fn test_chain_spec() -> Arc<MorphChainSpec> {
        MORPH_HOODI.clone()
    }

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
            MorphEngineValidator::<()>::updated_withdraw_trie_root_from_hashed_state(&state),
            Some(expected)
        );
    }

    #[test]
    fn test_extract_updated_withdraw_trie_root_from_hashed_state_missing_slot() {
        let state = HashedPostState::default();
        assert_eq!(
            MorphEngineValidator::<()>::updated_withdraw_trie_root_from_hashed_state(&state),
            None
        );
    }

    #[test]
    fn test_withdraw_trie_root_expectation_cache_evicts_incrementally_not_clear_all() {
        let validator = MorphEngineValidator::new(test_chain_spec(), ());
        let key = |n: usize| {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&(n as u64).to_be_bytes());
            B256::from(bytes)
        };

        for i in 0..MorphEngineValidator::<()>::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS {
            validator.record_withdraw_trie_root_expectation(
                key(i),
                WithdrawTrieRootExpectation::Verify(B256::from([0xaa; 32])),
            );
        }
        assert_eq!(
            validator.expected_withdraw_trie_roots.len(),
            MorphEngineValidator::<()>::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS
        );

        let oldest = key(0);
        let newest = key(MorphEngineValidator::<()>::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS);
        validator.record_withdraw_trie_root_expectation(
            newest,
            WithdrawTrieRootExpectation::Verify(B256::from([0xbb; 32])),
        );

        assert_eq!(
            validator.expected_withdraw_trie_roots.len(),
            MorphEngineValidator::<()>::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS
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
}
