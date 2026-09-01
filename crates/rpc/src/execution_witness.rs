//! Historical execution-witness RPC override.
//!
//! Reth's own `debug_executionWitness` reads the parent state through a historical overlay that
//! replays trie changesets backwards from the latest trie, so its cost grows with the distance to
//! the chain tip. These overrides source the same witness from the bounded proof history instead,
//! which stores a versioned trie per retained block and needs no replay.
//!
//! A proof window containing post-state snapshots `[earliest, latest]` can witness existing blocks
//! in `[earliest + 1, latest + 1]`. Requests whose parent state is outside that window, or whose
//! parent predates the MPT transition, are rejected.

use std::{
    sync::{Arc, LazyLock},
    time::Instant,
};

use alloy_consensus::BlockHeader;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::B256;
use alloy_rpc_types_debug::ExecutionWitness;
use async_trait::async_trait;
use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::{ErrorCode, ErrorObject},
};
use morph_chainspec::{hardfork::MorphHardforks, spec::MorphChainSpec};
use morph_proofs::{MorphProofsStorage, MorphProofsStore};
use reth_evm::{ConfigureEvm, execute::Executor};
use reth_metrics::metrics::Label;
use reth_revm::{State, database::StateProviderDatabase, witness::ExecutionWitnessRecord};
use reth_rpc_api::eth::helpers::FullEthApi;
use reth_rpc_eth_api::FromEthApiError;
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::HeaderProvider;
use reth_trie_common::ExecutionWitnessMode;

use crate::{
    eth::proofs::{ProofRpcMetrics, spawn_proof_task},
    state::MorphProofStateProviderFactory,
};

#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
pub trait ExecutionWitnessApiOverride {
    /// Re-executes a block whose parent state is retained and returns its execution witness.
    ///
    /// The optional second argument selects the witness generation mode and defaults to `legacy`.
    #[method(name = "executionWitness")]
    async fn execution_witness(
        &self,
        block: BlockNumberOrTag,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness>;

    /// Same as `debug_executionWitness`, addressing the block by hash.
    ///
    /// Overridden alongside the number-based method on purpose: leaving it on the default
    /// implementation would keep one entry point for the same result on the replay-based slow path.
    #[method(name = "executionWitnessByBlockHash")]
    async fn execution_witness_by_block_hash(
        &self,
        hash: B256,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness>;
}

/// Execution-witness RPC implementation backed exclusively by Morph proof history.
#[derive(Debug)]
pub struct ExecutionWitnessApiExt<Eth, P> {
    state_provider_factory: MorphProofStateProviderFactory<Eth, P>,
    chain_spec: Arc<MorphChainSpec>,
}

impl<Eth, P> ExecutionWitnessApiExt<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    P: MorphProofsStore + Clone + 'static,
{
    /// Creates the execution-witness RPC override.
    pub const fn new(
        eth_api: Eth,
        storage: MorphProofsStorage<P>,
        chain_spec: Arc<MorphChainSpec>,
    ) -> Self {
        Self {
            state_provider_factory: MorphProofStateProviderFactory::new(eth_api, storage),
            chain_spec,
        }
    }
}

impl<Eth, P> ExecutionWitnessApiExt<Eth, P>
where
    Eth: FullEthApi + Clone + Send + Sync + 'static,
    P: MorphProofsStore + Clone + 'static,
{
    /// Generates the witness for the block addressed by `block_id`.
    async fn witness_for_block(
        &self,
        block_id: BlockId,
        mode: Option<ExecutionWitnessMode>,
        metrics: &ProofRpcMetrics,
    ) -> RpcResult<ExecutionWitness> {
        let start = Instant::now();
        metrics.record_request();

        let mode = mode.unwrap_or_default();
        let factory = self.state_provider_factory.clone();
        let eth_api = factory.eth_api().clone();

        // Loading headers is I/O on the runtime; only execution and proof generation belong on the
        // blocking pool, so they are resolved before the permit is taken.
        let block = match eth_api
            .recovered_block(block_id)
            .await
            .map_err(Into::into)
            .and_then(|block| {
                block.ok_or_else(|| {
                    Eth::Error::from_eth_err(EthApiError::HeaderNotFound(block_id)).into()
                })
            }) {
            Ok(block) => block,
            Err(error) => {
                metrics.record_rejection();
                return Err(error);
            }
        };

        let block_number = block.header().number();
        let parent_hash = block.parent_hash();
        let parent_id = match parent_block_id(block_number, parent_hash) {
            Ok(parent) => parent,
            Err(error) => {
                metrics.record_rejection();
                return Err(error);
            }
        };

        let parent_header = match eth_api.provider().sealed_header_by_hash(parent_hash) {
            Ok(Some(header)) => header,
            Ok(None) => {
                metrics.record_rejection();
                return Err(
                    Eth::Error::from_eth_err(EthApiError::HeaderNotFound(parent_id)).into(),
                );
            }
            Err(error) => {
                let result: RpcResult<ExecutionWitness> =
                    Err(Eth::Error::from_eth_err(error).into());
                metrics.record_response(start, &result);
                return result;
            }
        };
        if let Err(error) = ensure_mpt_parent(
            self.chain_spec
                .is_jade_active_at_timestamp(parent_header.timestamp()),
        ) {
            metrics.record_rejection();
            return Err(error);
        }

        let result = spawn_proof_task(eth_api, move || {
            let state = factory
                .state_provider(Some(parent_id))
                .map_err(Eth::Error::from_eth_err)?;
            let mut db = StateProviderDatabase::new(&state);
            let executor = factory.eth_api().evm_config().executor(&mut db);

            // The same `mode` must reach both halves: it decides which bytecodes are collected
            // here and how the trie witness is computed below. Passing different values would ship
            // codes that describe a different execution than the trie nodes.
            let mut record = ExecutionWitnessRecord::default();
            executor
                .execute_with_state_closure(&block, |statedb: &State<_>| {
                    record.record_executed_state(statedb, mode);
                })
                .map_err(|error| Eth::Error::from_eth_err(EthApiError::Internal(error.into())))?;

            record
                .into_execution_witness(&*state, factory.eth_api().provider(), block_number, mode)
                .map_err(Eth::Error::from_eth_err)
        })
        .await;

        metrics.record_response(start, &result);
        result
    }
}

#[async_trait]
impl<Eth, P> ExecutionWitnessApiOverrideServer for ExecutionWitnessApiExt<Eth, P>
where
    Eth: FullEthApi + Clone + Send + Sync + 'static,
    P: MorphProofsStore + Clone + 'static,
{
    async fn execution_witness(
        &self,
        block: BlockNumberOrTag,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness> {
        self.witness_for_block(block.into(), mode, &EXECUTION_WITNESS_METRICS)
            .await
    }

    async fn execution_witness_by_block_hash(
        &self,
        hash: B256,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness> {
        self.witness_for_block(hash.into(), mode, &EXECUTION_WITNESS_BY_HASH_METRICS)
            .await
    }
}

static EXECUTION_WITNESS_METRICS: LazyLock<ProofRpcMetrics> = LazyLock::new(|| {
    ProofRpcMetrics::new_with_labels(vec![Label::new("method", "debug_executionWitness")])
});
static EXECUTION_WITNESS_BY_HASH_METRICS: LazyLock<ProofRpcMetrics> = LazyLock::new(|| {
    ProofRpcMetrics::new_with_labels(vec![Label::new(
        "method",
        "debug_executionWitnessByBlockHash",
    )])
});

/// Returns the parent hash identifier a witness must be generated against.
///
/// Genesis is rejected rather than clamped: it has no parent state, so using its own post-state
/// would silently produce a witness for the wrong transition.
fn parent_block_id(block_number: u64, parent_hash: B256) -> Result<BlockId, ErrorObject<'static>> {
    if block_number == 0 {
        return Err(ErrorObject::owned(
            ErrorCode::InvalidParams.code(),
            "genesis block has no parent state to generate a witness against",
            None::<()>,
        ));
    }
    Ok(parent_hash.into())
}

/// Rejects parent states that use the pre-MPT trie.
fn ensure_mpt_parent(jade_active: bool) -> Result<(), ErrorObject<'static>> {
    if !jade_active {
        return Err(ErrorObject::owned(
            ErrorCode::InvalidParams.code(),
            "execution witnesses require a parent state at or after the Jade hardfork",
            None::<()>,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use alloy_eips::BlockId;
    use alloy_primitives::B256;
    use jsonrpsee::types::ErrorCode;
    use reth_trie_common::ExecutionWitnessMode;

    use super::{
        EXECUTION_WITNESS_BY_HASH_METRICS, EXECUTION_WITNESS_METRICS, ensure_mpt_parent,
        parent_block_id,
    };

    #[test]
    fn builds_per_method_metric_handles() {
        // `new_with_labels` registers lazily, so a bad label or scope would otherwise only panic
        // while serving a real request.
        LazyLock::force(&EXECUTION_WITNESS_METRICS);
        LazyLock::force(&EXECUTION_WITNESS_BY_HASH_METRICS);
    }

    #[test]
    fn resolves_the_parent_by_hash() {
        let parent_hash = B256::repeat_byte(0x11);
        assert_eq!(
            parent_block_id(1, parent_hash).expect("block 1 has a parent"),
            BlockId::Hash(parent_hash.into())
        );
    }

    #[test]
    fn rejects_a_witness_request_for_genesis() {
        let error = parent_block_id(0, B256::ZERO).expect_err("genesis must be rejected");
        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert!(error.message().contains("no parent state"));
    }

    #[test]
    fn defaults_the_witness_mode_to_legacy() {
        assert_eq!(
            ExecutionWitnessMode::default(),
            ExecutionWitnessMode::Legacy
        );
    }

    #[test]
    fn rejects_a_parent_state_from_before_the_mpt_transition() {
        let error = ensure_mpt_parent(false).expect_err("pre-MPT parent must be rejected");
        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert!(error.message().contains("Jade"));
        ensure_mpt_parent(true).expect("post-transition parent must be accepted");
    }
}
