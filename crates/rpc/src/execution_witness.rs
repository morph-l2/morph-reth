//! Historical execution-witness RPC override.
//!
//! Reth's own `debug_executionWitness` reads the parent state through a historical overlay that
//! replays trie changesets backwards from the latest trie, so its cost grows with the distance to
//! the chain tip. These overrides source the same witness from the bounded proof history instead,
//! which stores a versioned trie per retained block and needs no replay.
//!
//! Blocks outside the retained window keep failing the way every other proof RPC fails there; the
//! window never reaches back to the ZK-trie era before Jade, where an MPT witness would be
//! meaningless anyway.
//!
//! Witness mode is not part of the request, matching Base and op-reth: only the legacy shape is
//! produced, which is also what reth's default returns when no mode is given.

use std::{sync::LazyLock, time::Instant};

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
use morph_proofs::{MorphProofsStorage, MorphProofsStore};
use reth_evm::{ConfigureEvm, execute::Executor};
use reth_metrics::metrics::Label;
use reth_revm::{State, database::StateProviderDatabase, witness::ExecutionWitnessRecord};
use reth_rpc_api::eth::helpers::FullEthApi;
use reth_rpc_eth_api::FromEthApiError;
use reth_rpc_eth_types::EthApiError;
use reth_trie_common::ExecutionWitnessMode;

use crate::{
    eth::proofs::{ProofRpcMetrics, spawn_proof_task},
    state::MorphProofStateProviderFactory,
};

static EXECUTION_WITNESS_METRICS: LazyLock<ProofRpcMetrics> = LazyLock::new(|| {
    ProofRpcMetrics::new_with_labels(vec![Label::new("method", "debug_executionWitness")])
});
static EXECUTION_WITNESS_BY_HASH_METRICS: LazyLock<ProofRpcMetrics> = LazyLock::new(|| {
    ProofRpcMetrics::new_with_labels(vec![Label::new(
        "method",
        "debug_executionWitnessByBlockHash",
    )])
});

/// Resolves the parent block a witness must be generated against.
///
/// Genesis is rejected rather than clamped: `parent_num_hash()` saturates at zero, so a witness
/// request for block 0 would otherwise be served from block 0's own post-state and silently return
/// a witness for the wrong state.
fn parent_block_number(block_number: u64) -> Result<u64, ErrorObject<'static>> {
    block_number.checked_sub(1).ok_or_else(|| {
        ErrorObject::owned(
            ErrorCode::InvalidParams.code(),
            "genesis block has no parent state to generate a witness against",
            None::<()>,
        )
    })
}

#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
pub trait ExecutionWitnessApiOverride {
    /// Re-executes a retained canonical block and returns its execution witness.
    #[method(name = "executionWitness")]
    async fn execution_witness(&self, block: BlockNumberOrTag) -> RpcResult<ExecutionWitness>;

    /// Same as `debug_executionWitness`, addressing the block by hash.
    ///
    /// Overridden alongside the number-based method on purpose: leaving it on the default
    /// implementation would keep one entry point for the same result on the replay-based slow path.
    #[method(name = "executionWitnessByBlockHash")]
    async fn execution_witness_by_block_hash(&self, hash: B256) -> RpcResult<ExecutionWitness>;
}

/// Execution-witness RPC implementation backed exclusively by Morph proof history.
#[derive(Debug)]
pub struct ExecutionWitnessApiExt<Eth, P> {
    state_provider_factory: MorphProofStateProviderFactory<Eth, P>,
}

impl<Eth, P> ExecutionWitnessApiExt<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    P: MorphProofsStore + Clone + 'static,
{
    /// Creates the execution-witness RPC override.
    pub const fn new(eth_api: Eth, storage: MorphProofsStorage<P>) -> Self {
        Self {
            state_provider_factory: MorphProofStateProviderFactory::new(eth_api, storage),
        }
    }
}

impl<Eth, P> ExecutionWitnessApiExt<Eth, P>
where
    Eth: FullEthApi + Clone + Send + Sync + 'static,
    P: MorphProofsStore + Clone + 'static,
{
    /// Generates the witness for a canonical block addressed by `block_id`.
    async fn witness_for_block(
        &self,
        block_id: BlockId,
        metrics: &ProofRpcMetrics,
    ) -> RpcResult<ExecutionWitness> {
        let start = Instant::now();
        metrics.record_request();

        // Witness mode is not exposed on the wire, matching Base and op-reth. The proof-history
        // provider only implements the legacy shape, so accepting a `canonical` request would
        // return something that is neither format. Reth's default for this method is legacy too,
        // so replacing it does not change what a caller receives.
        let mode = ExecutionWitnessMode::default();
        let factory = self.state_provider_factory.clone();
        let eth_api = factory.eth_api().clone();

        // Loading the block is I/O on the runtime; only the replay below belongs on the blocking
        // pool, so it is resolved before the permit is taken.
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
        let parent_number = match parent_block_number(block_number) {
            Ok(number) => number,
            Err(error) => {
                metrics.record_rejection();
                return Err(error);
            }
        };

        let result = spawn_proof_task(eth_api, move || {
            let state = factory
                .state_provider(Some(BlockId::Number(parent_number.into())))
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
    async fn execution_witness(&self, block: BlockNumberOrTag) -> RpcResult<ExecutionWitness> {
        self.witness_for_block(block.into(), &EXECUTION_WITNESS_METRICS)
            .await
    }

    async fn execution_witness_by_block_hash(&self, hash: B256) -> RpcResult<ExecutionWitness> {
        self.witness_for_block(hash.into(), &EXECUTION_WITNESS_BY_HASH_METRICS)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use jsonrpsee::types::ErrorCode;
    use reth_trie_common::ExecutionWitnessMode;

    use super::{
        EXECUTION_WITNESS_BY_HASH_METRICS, EXECUTION_WITNESS_METRICS, parent_block_number,
    };

    #[test]
    fn builds_per_method_metric_handles() {
        // `new_with_labels` registers lazily, so a bad label or scope would otherwise only panic
        // while serving a real request.
        LazyLock::force(&EXECUTION_WITNESS_METRICS);
        LazyLock::force(&EXECUTION_WITNESS_BY_HASH_METRICS);
    }

    #[test]
    fn resolves_the_parent_of_a_non_genesis_block() {
        assert_eq!(parent_block_number(1).expect("block 1 has a parent"), 0);
        assert_eq!(parent_block_number(1_000).expect("has a parent"), 999);
    }

    #[test]
    fn rejects_a_witness_request_for_genesis() {
        // Clamping instead would serve genesis' own post-state as its parent state.
        let error = parent_block_number(0).expect_err("genesis must be rejected");
        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert!(error.message().contains("no parent state"));
    }

    #[test]
    fn pins_the_witness_mode_to_legacy() {
        // This RPC hardcodes the upstream default rather than exposing `mode`, so the two must stay
        // the same value. If a reth bump flipped the default, callers would silently start
        // receiving a canonical-shaped witness that this proof provider does not actually produce.
        assert_eq!(
            ExecutionWitnessMode::default(),
            ExecutionWitnessMode::Legacy
        );
    }
}
