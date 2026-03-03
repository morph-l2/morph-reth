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
use parking_lot::Mutex;
use reth_chainspec::EthChainSpec;
use reth_errors::ConsensusError;
use reth_node_api::{
    AddOnsContext, BlockTy, FullNodeComponents, InvalidPayloadAttributesError, NewPayloadError,
    NodeTypes, PayloadAttributes, PayloadTypes, PayloadValidator, StateRootDecisionInput,
    StateRootValidator,
};
use reth_node_builder::{
    invalid_block_hook::InvalidBlockHookExt,
    rpc::{BasicEngineValidator, ChangesetCache, EngineValidatorBuilder, PayloadValidatorBuilder},
};
use reth_primitives_traits::{GotExpected, RecoveredBlock, SealedBlock};
use reth_provider::ChainSpecProvider;
use reth_tracing::tracing;
use std::{collections::VecDeque, sync::Arc};

/// Builder for Morph engine validator (payload validation).
///
/// Creates a validator for validating engine API payloads.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct MorphEngineValidatorBuilder {
    /// Optional geth RPC URL for cross-validating MPT state root via `morph_diskRoot`.
    pub geth_rpc_url: Option<String>,
}

impl MorphEngineValidatorBuilder {
    /// Sets the geth RPC URL for state root cross-validation.
    pub fn with_geth_rpc_url(mut self, url: Option<String>) -> Self {
        self.geth_rpc_url = url;
        self
    }
}

impl<Node> PayloadValidatorBuilder<Node> for MorphEngineValidatorBuilder
where
    Node: FullNodeComponents<Types = MorphNode>,
    Node::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
{
    type Validator = MorphEngineValidator;

    async fn build(self, ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        let mut validator = MorphEngineValidator::new(ctx.node.provider().chain_spec());
        if let Some(url) = self.geth_rpc_url {
            validator = validator.with_geth_rpc_url(url);
        }
        Ok(validator)
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
        Evm: reth_node_api::ConfigureEngineEvm<
            <<Node::Types as NodeTypes>::Payload as PayloadTypes>::ExecutionData,
        >,
    >,
    PVB: PayloadValidatorBuilder<Node>,
    PVB::Validator: reth_node_api::PayloadValidator<
            <Node::Types as NodeTypes>::Payload,
            Block = BlockTy<Node::Types>,
        > + StateRootValidator<<Node::Types as NodeTypes>::Primitives>
        + Clone,
{
    type EngineValidator =
        BasicEngineValidator<Node::Provider, Node::Evm, PVB::Validator, PVB::Validator>;

    async fn build_tree_validator(
        self,
        ctx: &AddOnsContext<'_, Node>,
        tree_config: reth_node_api::TreeConfig,
        changeset_cache: ChangesetCache,
    ) -> eyre::Result<Self::EngineValidator> {
        let validator = self.payload_validator_builder.build(ctx).await?;
        let data_dir = ctx
            .config
            .datadir
            .clone()
            .resolve_datadir(ctx.config.chain.chain());
        let invalid_block_hook = ctx.create_invalid_block_hook(&data_dir).await?;

        Ok(BasicEngineValidator::new(
            ctx.node.provider().clone(),
            Arc::new(ctx.node.consensus().clone()),
            ctx.node.evm_config().clone(),
            validator.clone(),
            tree_config,
            invalid_block_hook,
            changeset_cache,
        )
        .with_state_root_validator(validator))
    }
}

/// Morph engine validator for payload validation.
///
/// This validator is used by the engine API to validate incoming payloads.
/// For Morph, most validation is deferred to the consensus layer.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MorphEngineValidator {
    chain_spec: Arc<MorphChainSpec>,
    expected_withdraw_trie_roots: Arc<DashMap<B256, WithdrawTrieRootExpectation>>,
    expected_withdraw_trie_root_order: Arc<Mutex<VecDeque<B256>>>,
    /// Optional geth RPC URL for cross-validating MPT state root via `morph_diskRoot`.
    geth_rpc_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WithdrawTrieRootExpectation {
    SkipValidation,
    Verify(B256),
}

impl MorphEngineValidator {
    const MAX_EXPECTED_WITHDRAW_TRIE_ROOTS: usize = 4096;

    /// Creates a new [`MorphEngineValidator`].
    pub fn new(chain_spec: Arc<MorphChainSpec>) -> Self {
        Self {
            chain_spec,
            expected_withdraw_trie_roots: Arc::new(DashMap::new()),
            expected_withdraw_trie_root_order: Arc::new(Mutex::new(VecDeque::new())),
            geth_rpc_url: None,
        }
    }

    /// Sets the geth RPC URL for cross-validating MPT state root.
    pub fn with_geth_rpc_url(mut self, url: String) -> Self {
        tracing::info!(target: "morph::validator", %url, "Enabled state root cross-validation via geth diskRoot RPC");
        self.geth_rpc_url = Some(url);
        self
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
            return Err(ConsensusError::Other(format!(
                "missing withdraw trie root expectation cache entry for block {}",
                block.hash()
            )));
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
            return Err(ConsensusError::Other(format!(
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

impl StateRootValidator<morph_primitives::MorphPrimitives> for MorphEngineValidator {
    fn should_compute_state_root(&self, input: &StateRootDecisionInput) -> bool {
        // Long-term behavior: always compute after Jade.
        // Temporary behavior: if geth RPC is configured, also compute before Jade
        // so we can cross-check against geth's `morph_diskRoot`.
        self.chain_spec.is_jade_active_at_timestamp(input.timestamp) || self.geth_rpc_url.is_some()
    }

    fn validate_state_root(
        &self,
        block: &RecoveredBlock<morph_primitives::Block>,
        computed_state_root: B256,
    ) -> Result<(), ConsensusError> {
        let block_number = block.header().number();
        let jade_active = self
            .chain_spec
            .is_jade_active_at_timestamp(block.header().timestamp());

        // Always enforce canonical state-root equality in MPT mode.
        if jade_active {
            let expected_state_root = block.header().state_root();
            if computed_state_root != expected_state_root {
                return Err(ConsensusError::BodyStateRootDiff(
                    GotExpected {
                        got: computed_state_root,
                        expected: expected_state_root,
                    }
                    .into(),
                ));
            }
        }

        // Temporary cross-validation path: compare with geth's diskRoot when configured.
        let Some(geth_url) = self.geth_rpc_url.as_deref() else {
            return Ok(());
        };

        match fetch_geth_disk_root(geth_url, block_number) {
            Ok(disk_root) => {
                if computed_state_root == disk_root {
                    tracing::debug!(
                        target: "morph::validator",
                        block_number,
                        ?computed_state_root,
                        "State root cross-validation passed"
                    );
                    Ok(())
                } else {
                    tracing::error!(
                        target: "morph::validator",
                        block_number,
                        ?computed_state_root,
                        ?disk_root,
                        "State root cross-validation FAILED"
                    );
                    Err(ConsensusError::BodyStateRootDiff(
                        GotExpected {
                            got: computed_state_root,
                            expected: disk_root,
                        }
                        .into(),
                    ))
                }
            }
            Err(err) => {
                tracing::warn!(
                    target: "morph::validator",
                    block_number,
                    %err,
                    "Failed to fetch diskRoot from geth, skipping state root validation"
                );
                Ok(())
            }
        }
    }
}

/// Response from geth's `morph_diskRoot` RPC method.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiskAndHeaderRoot {
    disk_root: B256,
}

/// JSON-RPC response wrapper.
#[derive(Debug, serde::Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

/// JSON-RPC error object.
#[derive(Debug, serde::Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

/// Fetches the MPT state root from a geth node via `morph_diskRoot` RPC.
///
/// This calls geth's `morph_diskRoot` method with the given block number to obtain
/// the MPT-format state root (`diskRoot`) for cross-validation against reth's
/// computed root.
fn fetch_geth_disk_root(geth_url: &str, block_number: u64) -> Result<B256, String> {
    let block_hex = format!("0x{block_number:x}");
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "morph_diskRoot",
        "params": [{"blockNumber": block_hex}],
        "id": 1
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))?;

    let resp = client
        .post(geth_url)
        .json(&body)
        .send()
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {} from geth", resp.status()));
    }

    let rpc_resp: JsonRpcResponse<DiskAndHeaderRoot> = resp
        .json()
        .map_err(|e| format!("failed to parse response: {e}"))?;

    if let Some(err) = rpc_resp.error {
        return Err(err.to_string());
    }

    rpc_resp
        .result
        .map(|r| r.disk_root)
        .ok_or_else(|| "morph_diskRoot returned null result".to_string())
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
        let validator = MorphEngineValidator::new(test_chain_spec());
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
}
