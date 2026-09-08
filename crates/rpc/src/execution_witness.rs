//! Historical execution-witness RPC override.
//!
//! Reth's own `debug_executionWitness` reads the parent state through a historical overlay that
//! replays trie changesets backwards from the latest trie, so its cost grows with the distance to
//! the chain tip. These overrides source the same witness from the bounded proof history instead,
//! which stores a versioned trie per retained block and needs no replay.
//!
//! A proof window containing post-state snapshots `[earliest, latest]` can witness existing blocks
//! in `[earliest + 1, latest + 1]`: only the parent's post-state is needed, so the window's own tip
//! is still a servable parent. Requests whose parent state falls outside that window are rejected.
//!
//! Nothing here guards the ZK-trie era before Jade, where an MPT witness would be meaningless. The
//! retained window spans days while Jade activated months ago, so no request can reach back that
//! far; a guard would only fire if the window were widened across the fork, and it would report a
//! fork error where the honest answer is that such a configuration was never supported.

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

/// Returns the parent identifier a witness must be generated against.
///
/// Addressed by hash, not by `block_number - 1`: `executionWitnessByBlockHash` accepts any block
/// still in the database, including one on an abandoned branch, and a number would then resolve to
/// the canonical block at that height. That would replay the requested block against a sibling
/// branch's state and return a successful but meaningless witness. A hash instead reaches the
/// canonical check in [`MorphProofStateProviderFactory`], which rejects the request outright.
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

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use alloy_eips::BlockId;
    use alloy_primitives::B256;
    use jsonrpsee::types::ErrorCode;
    use reth_trie_common::ExecutionWitnessMode;

    use super::{EXECUTION_WITNESS_BY_HASH_METRICS, EXECUTION_WITNESS_METRICS, parent_block_id};

    #[test]
    fn builds_per_method_metric_handles() {
        // `new_with_labels` registers lazily, so a bad label or scope would otherwise only panic
        // while serving a real request.
        LazyLock::force(&EXECUTION_WITNESS_METRICS);
        LazyLock::force(&EXECUTION_WITNESS_BY_HASH_METRICS);
    }

    #[test]
    fn resolves_the_parent_by_hash() {
        // By hash, never by height: a height would silently resolve to the canonical block when the
        // requested one sits on an abandoned branch.
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
        // `mode` is optional on the wire, so the upstream default is this RPC's default. A reth bump
        // that flipped it would silently change the shape every existing caller receives.
        assert_eq!(
            ExecutionWitnessMode::default(),
            ExecutionWitnessMode::Legacy
        );
    }
}
