//! Proof-history synchronization status RPC.

use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use morph_proofs::{MorphProofsStorage, MorphProofsStorageResult, MorphProofsStore};
use reth_rpc_server_types::result::internal_rpc_err;
use serde::{Deserialize, Serialize};

/// Inclusive proof-history range currently available to RPC requests.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ProofsSyncStatus {
    /// Earliest retained canonical block.
    pub earliest: Option<u64>,
    /// Latest durably indexed canonical block.
    pub latest: Option<u64>,
}

#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
pub trait ProofStatusApiOverride {
    /// Returns the current durable proof-history range.
    #[method(name = "proofsSyncStatus")]
    async fn proofs_sync_status(&self) -> RpcResult<ProofsSyncStatus>;
}

/// `debug_proofsSyncStatus` backed by the proof database.
#[derive(Debug)]
pub struct ProofStatusApiExt<P> {
    storage: MorphProofsStorage<P>,
}

impl<P> ProofStatusApiExt<P> {
    /// Creates a proof-status handler.
    pub const fn new(storage: MorphProofsStorage<P>) -> Self {
        Self { storage }
    }
}

fn proofs_sync_status_with_tx<'tx, 'db, P>(
    storage: &P,
    tx: &'tx P::Tx<'db>,
) -> MorphProofsStorageResult<ProofsSyncStatus>
where
    P: MorphProofsStore + 'db,
    'db: 'tx,
{
    let earliest = storage
        .get_earliest_block_number_with_tx(tx)?
        .map(|(number, _)| number);
    let latest = storage
        .get_latest_block_number_with_tx(tx)?
        .map(|(number, _)| number);
    Ok(ProofsSyncStatus { earliest, latest })
}

#[async_trait]
impl<P> ProofStatusApiOverrideServer for ProofStatusApiExt<P>
where
    P: MorphProofsStore + Clone + Send + Sync + 'static,
{
    async fn proofs_sync_status(&self) -> RpcResult<ProofsSyncStatus> {
        // Keep both bounds on the same MDBX snapshot. A concurrent prune may advance the live
        // earliest block, but it must not produce a status range assembled from two generations.
        let tx = self
            .storage
            .ro_tx()
            .map_err(|error| internal_rpc_err(error.to_string()))?;
        proofs_sync_status_with_tx(&self.storage, &tx)
            .map_err(|error| internal_rpc_err(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
    use alloy_primitives::B256;
    use morph_proofs::{
        BlockStateDiff, MdbxProofsStorage, MorphProofsInitialStateStore, MorphProofsStore,
    };

    use super::{ProofsSyncStatus, proofs_sync_status_with_tx};

    #[test]
    fn empty_status_uses_json_nulls() {
        let json = serde_json::to_value(ProofsSyncStatus {
            earliest: None,
            latest: None,
        })
        .expect("serialize status");
        assert!(json["earliest"].is_null());
        assert!(json["latest"].is_null());
    }

    #[test]
    fn status_bounds_share_one_snapshot_during_prune() {
        let directory = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(MdbxProofsStorage::new(directory.path()).expect("open storage"));
        let genesis_hash = B256::repeat_byte(0x01);
        storage
            .set_initial_state_anchor(BlockNumHash::new(0, genesis_hash))
            .expect("set initial anchor");
        storage
            .commit_initial_state()
            .expect("commit initial state");

        let block_one =
            BlockWithParent::new(genesis_hash, NumHash::new(1, B256::repeat_byte(0x02)));
        storage
            .store_trie_updates(block_one, BlockStateDiff::default())
            .expect("store block one");

        let request_tx = storage.ro_tx().expect("open status snapshot");
        storage
            .prune_earliest_state(block_one)
            .expect("advance live proof window");

        let request_status = proofs_sync_status_with_tx(&storage, &request_tx)
            .expect("read status from pinned snapshot");
        assert_eq!(request_status.earliest, Some(0));
        assert_eq!(request_status.latest, Some(1));

        let current_tx = storage.ro_tx().expect("open current snapshot");
        let current_status =
            proofs_sync_status_with_tx(&storage, &current_tx).expect("read current status");
        assert_eq!(current_status.earliest, Some(1));
        assert_eq!(current_status.latest, Some(1));
    }
}
