//! Provider for external proofs storage

use std::fmt::Debug;

use alloy_primitives::keccak256;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, ProviderError,
    ProviderResult, StateProofProvider, StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_revm::{
    db::BundleState,
    primitives::{Address, B256, Bytes, StorageValue, alloy_primitives::BlockNumber},
};
use reth_trie::{
    StateRoot, StorageRoot,
    hashed_cursor::HashedCursor,
    proof::{self, Proof},
    witness::TrieWitness,
};
use reth_trie_common::{
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedStorage, KeccakKeyHasher,
    MultiProof, MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
    updates::TrieUpdates,
};

use crate::{
    MorphProofsStorage, MorphProofsStorageError, MorphProofsStore,
    proof::{
        DatabaseProof, DatabaseStateRoot, DatabaseStorageProof, DatabaseStorageRoot,
        DatabaseTrieWitness,
    },
};

/// State provider for external proofs storage.
pub struct MorphProofsStateProviderRef<'a, Storage: MorphProofsStore> {
    /// Historical state provider for non-state related tasks.
    latest: Box<dyn StateProvider + Send + 'a>,

    /// Storage provider for state lookups.
    storage: &'a MorphProofsStorage<Storage>,

    /// Max block number that can be used for state lookups.
    block_number: BlockNumber,

    /// Lazily-acquired read-only transaction shared across all EVM state reads.
    ///
    /// Acquired once on the first [`basic_account`](AccountReader::basic_account) or
    /// [`storage`](StateProvider::storage) call and reused for the lifetime of this provider,
    /// so that all EVM state reads within a single execution context share one database snapshot
    /// and avoid per-call transaction-acquisition contention.
    lazy_tx: Mutex<Option<Storage::Tx<'a>>>,
}

impl<'a, Storage: MorphProofsStore> MorphProofsStateProviderRef<'a, Storage> {
    /// Creates a new state provider.
    pub fn new(
        latest: Box<dyn StateProvider + Send + 'a>,
        storage: &'a MorphProofsStorage<Storage>,
        block_number: BlockNumber,
    ) -> Self {
        Self {
            latest,
            storage,
            block_number,
            lazy_tx: Mutex::new(None),
        }
    }

    /// Creates a state provider pinned to an existing proof-database read transaction.
    pub fn new_with_tx(
        latest: Box<dyn StateProvider + Send + 'a>,
        storage: &'a MorphProofsStorage<Storage>,
        block_number: BlockNumber,
        tx: Storage::Tx<'a>,
    ) -> Self {
        Self {
            latest,
            storage,
            block_number,
            lazy_tx: Mutex::new(Some(tx)),
        }
    }

    fn ensure_tx(&self) -> ProviderResult<MappedMutexGuard<'_, Storage::Tx<'a>>> {
        let mut guard = self.lazy_tx.lock();
        if guard.is_none() {
            *guard = Some(self.storage.ro_tx().map_err(Into::<ProviderError>::into)?);
        }

        Ok(MutexGuard::map(guard, |tx| {
            tx.as_mut().expect("read-only transaction initialized")
        }))
    }
}

impl<'a, Storage> Debug for MorphProofsStateProviderRef<'a, Storage>
where
    Storage: MorphProofsStore + 'a + Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MorphProofsStateProviderRef")
            .field("storage", &self.storage)
            .field("block_number", &self.block_number)
            .finish()
    }
}

impl<'a, Storage: MorphProofsStore + Clone> MorphProofsStateProviderRef<'a, Storage> {
    fn storage_by_hashed_key(
        &self,
        address: Address,
        hashed_key: B256,
    ) -> ProviderResult<Option<StorageValue>> {
        let tx = self.ensure_tx()?;
        Ok(self
            .storage
            .storage_hashed_cursor_with_tx(&tx, keccak256(address.0), self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, val)| (key == hashed_key).then_some(val)))
    }
}

impl From<MorphProofsStorageError> for ProviderError {
    fn from(error: MorphProofsStorageError) -> Self {
        Self::other(error)
    }
}

impl<'a, Storage: MorphProofsStore> BlockHashReader for MorphProofsStateProviderRef<'a, Storage> {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.latest.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.latest.canonical_hashes_range(start, end)
    }
}

impl<'a, Storage: MorphProofsStore + Clone> StateRootProvider
    for MorphProofsStateProviderRef<'a, Storage>
{
    fn state_root(&self, state: HashedPostState) -> ProviderResult<B256> {
        Ok(StateRoot::overlay_root(
            self.storage,
            self.block_number,
            state,
        )?)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        Ok(StateRoot::overlay_root_from_nodes(
            self.storage,
            self.block_number,
            input,
        )?)
    }

    fn state_root_with_updates(
        &self,
        state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok(StateRoot::overlay_root_with_updates(
            self.storage,
            self.block_number,
            state,
        )?)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok(StateRoot::overlay_root_from_nodes_with_updates(
            self.storage,
            self.block_number,
            input,
        )?)
    }
}

impl<'a, Storage: MorphProofsStore + Clone> StorageRootProvider
    for MorphProofsStateProviderRef<'a, Storage>
{
    fn storage_root(&self, address: Address, storage: HashedStorage) -> ProviderResult<B256> {
        StorageRoot::overlay_root(self.storage, self.block_number, address, storage)
            .map_err(|err| ProviderError::Database(err.into()))
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        proof::StorageProof::overlay_storage_proof(
            self.storage,
            self.block_number,
            address,
            slot,
            storage,
        )
        .map_err(ProviderError::from)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        proof::StorageProof::overlay_storage_multiproof(
            self.storage,
            self.block_number,
            address,
            slots,
            storage,
        )
        .map_err(ProviderError::from)
    }
}

impl<'a, Storage: MorphProofsStore + Clone> StateProofProvider
    for MorphProofsStateProviderRef<'a, Storage>
{
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        let tx = self.ensure_tx()?;
        Proof::overlay_account_proof_with_tx(
            self.storage,
            &tx,
            self.block_number,
            input,
            address,
            slots,
        )
        .map_err(ProviderError::from)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        let tx = self.ensure_tx()?;
        Proof::overlay_multiproof_with_tx(self.storage, &tx, self.block_number, input, targets)
            .map_err(ProviderError::from)
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        let tx = self.ensure_tx()?;
        TrieWitness::overlay_witness_with_tx(
            self.storage,
            &tx,
            self.block_number,
            input,
            target,
            mode,
        )
        .map_err(ProviderError::from)
        .map(|hm| {
            let mut values: Vec<_> = hm.into_values().collect();
            if mode.is_canonical() {
                values.sort_unstable();
            }
            values
        })
    }
}

impl<'a, Storage: MorphProofsStore> HashedPostStateProvider
    for MorphProofsStateProviderRef<'a, Storage>
{
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state())
    }
}

impl<'a, Storage: MorphProofsStore> AccountReader for MorphProofsStateProviderRef<'a, Storage> {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let hashed_key = keccak256(address.0);
        let tx = self.ensure_tx()?;
        Ok(self
            .storage
            .account_hashed_cursor_with_tx(&tx, self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, account)| (key == hashed_key).then_some(account)))
    }
}

impl<'a, Storage> StateProvider for MorphProofsStateProviderRef<'a, Storage>
where
    Storage: MorphProofsStore + Clone,
{
    fn storage(&self, address: Address, storage_key: B256) -> ProviderResult<Option<StorageValue>> {
        let hashed_key = keccak256(storage_key);
        self.storage_by_hashed_key(address, hashed_key)
    }
}

impl<'a, Storage: MorphProofsStore> BytecodeReader for MorphProofsStateProviderRef<'a, Storage> {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.latest.bytecode_by_hash(code_hash)
    }
}

#[cfg(test)]
mod tests {
    use reth_provider::noop::NoopProvider;

    use super::*;
    use crate::InMemoryProofsStorage;

    #[test]
    fn test_morph_proofs_state_provider_ref_debug() {
        let latest: Box<dyn StateProvider + Send> = Box::<NoopProvider>::default();
        let storage: crate::MorphProofsStorage<InMemoryProofsStorage> =
            InMemoryProofsStorage::new();
        let block_number = 42u64;

        let provider = MorphProofsStateProviderRef::new(latest, &storage, block_number);

        assert_eq!(
            format!("{provider:?}"),
            "MorphProofsStateProviderRef { storage: InMemoryProofsStorage { inner: RwLock { data: InMemoryStorageInner { account_branches: {}, storage_branches: {}, hashed_accounts: {}, hashed_storages: {}, trie_updates: {}, post_states: {}, earliest_block: None, anchor_block: None } } }, block_number: 42 }"
        );
    }
}
