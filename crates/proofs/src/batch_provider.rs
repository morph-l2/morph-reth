//! State provider for an active [`MorphProofsBatchSession`] enabling reads to observe
//! uncommitted writes performed earlier in the same session.

use std::fmt::Debug;

use alloy_primitives::{
    keccak256,
    map::{B256Map, HashMap},
};
use derive_more::Constructor;
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
    StateRoot, StorageRoot, TrieType,
    hashed_cursor::{HashedCursor, HashedPostStateCursorFactory, zero_destroyed_account_storage},
    metrics::TrieRootMetrics,
    proof,
    trie_cursor::InMemoryTrieCursorFactory,
    witness::TrieWitness,
};
use reth_trie_common::{
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedPostStateSorted, HashedStorage,
    KeccakKeyHasher, MultiProof, MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
    updates::TrieUpdates,
};

use crate::{
    MorphProofsBatchHashedAccountCursorFactory, MorphProofsBatchTrieCursorFactory,
    api::MorphProofsBatchSession,
};

/// State provider that reads through an active [`MorphProofsBatchSession`]'s transaction.
#[derive(Constructor)]
pub struct MorphProofsBatchStateProviderRef<'a, S: MorphProofsBatchSession> {
    latest: Box<dyn StateProvider + Send + 'a>,
    session: &'a S,
    block_number: BlockNumber,
}

impl<S> Debug for MorphProofsBatchStateProviderRef<'_, S>
where
    S: MorphProofsBatchSession,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MorphProofsBatchStateProviderRef")
            .field("session", &self.session)
            .field("block_number", &self.block_number)
            .finish()
    }
}

impl<'a, S: MorphProofsBatchSession> MorphProofsBatchStateProviderRef<'a, S> {
    const fn factories(
        &self,
    ) -> (
        MorphProofsBatchTrieCursorFactory<'a, S>,
        MorphProofsBatchHashedAccountCursorFactory<'a, S>,
    ) {
        (
            MorphProofsBatchTrieCursorFactory::new(self.session, self.block_number),
            MorphProofsBatchHashedAccountCursorFactory::new(self.session, self.block_number),
        )
    }
}

impl<S: MorphProofsBatchSession> BlockHashReader for MorphProofsBatchStateProviderRef<'_, S> {
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

impl<S: MorphProofsBatchSession> StateRootProvider for MorphProofsBatchStateProviderRef<'_, S> {
    fn state_root(&self, state: HashedPostState) -> ProviderResult<B256> {
        let prefix_sets = state.construct_prefix_sets().freeze();
        let state_sorted = state.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        StateRoot::new(
            trie_factory,
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(prefix_sets)
        .root()
        .map_err(ProviderError::from)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        let state_sorted = input.state.into_sorted();
        let nodes_sorted = input.nodes.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        StateRoot::new(
            InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted),
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root()
        .map_err(ProviderError::from)
    }

    fn state_root_with_updates(
        &self,
        state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        let prefix_sets = state.construct_prefix_sets().freeze();
        let state_sorted = state.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        StateRoot::new(
            trie_factory,
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(prefix_sets)
        .root_with_updates()
        .map_err(ProviderError::from)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        let state_sorted = input.state.into_sorted();
        let nodes_sorted = input.nodes.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        StateRoot::new(
            InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted),
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root_with_updates()
        .map_err(ProviderError::from)
    }
}

impl<S: MorphProofsBatchSession> StorageRootProvider for MorphProofsBatchStateProviderRef<'_, S> {
    fn storage_root(&self, address: Address, storage: HashedStorage) -> ProviderResult<B256> {
        let prefix_set = storage.construct_prefix_set().freeze();
        let state_sorted =
            HashedPostState::from_hashed_storage(keccak256(address), storage).into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        StorageRoot::new(
            trie_factory,
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
            address,
            prefix_set,
            TrieRootMetrics::new(TrieType::Custom("morph_historical_proofs_storage_batch")),
        )
        .root()
        .map_err(|err| ProviderError::Database(err.into()))
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        let hashed_address = keccak256(address);
        let prefix_set = hashed_storage.construct_prefix_set();
        let state_sorted = HashedPostStateSorted::new(
            Default::default(),
            HashMap::from_iter([(hashed_address, hashed_storage.into_sorted())]),
        );
        let (trie_factory, hashed_factory) = self.factories();
        proof::StorageProof::new(trie_factory, hashed_factory.clone(), address)
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_set_mut(prefix_set)
            .storage_proof(slot)
            .map_err(ProviderError::from)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        let hashed_address = keccak256(address);
        let targets = slots.iter().map(keccak256).collect();
        let prefix_set = hashed_storage.construct_prefix_set();
        let state_sorted = HashedPostStateSorted::new(
            Default::default(),
            HashMap::from_iter([(hashed_address, hashed_storage.into_sorted())]),
        );
        let (trie_factory, hashed_factory) = self.factories();
        proof::StorageProof::new(trie_factory, hashed_factory.clone(), address)
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_set_mut(prefix_set)
            .storage_multiproof(targets)
            .map_err(ProviderError::from)
    }
}

impl<S: MorphProofsBatchSession> StateProofProvider for MorphProofsBatchStateProviderRef<'_, S> {
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        proof::Proof::new(trie_factory.clone(), hashed_factory.clone())
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .account_proof(address, slots)
            .map_err(ProviderError::from)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        proof::Proof::new(trie_factory.clone(), hashed_factory.clone())
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .multiproof(targets)
            .map_err(ProviderError::from)
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
        _mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        let result: B256Map<Bytes> = TrieWitness::new(trie_factory.clone(), hashed_factory.clone())
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .always_include_root_node()
            .compute(target)
            .map_err(ProviderError::from)?;
        Ok(result.into_values().collect())
    }
}

impl<S: MorphProofsBatchSession> HashedPostStateProvider
    for MorphProofsBatchStateProviderRef<'_, S>
{
    fn hashed_post_state(&self, bundle_state: &BundleState) -> ProviderResult<HashedPostState> {
        let mut hashed_state =
            HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state());
        zero_destroyed_account_storage(
            &MorphProofsBatchHashedAccountCursorFactory::new(self.session, self.block_number),
            bundle_state.state(),
            &mut hashed_state,
        )
        .map_err(ProviderError::from)?;
        Ok(hashed_state)
    }
}

impl<S: MorphProofsBatchSession> AccountReader for MorphProofsBatchStateProviderRef<'_, S> {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let hashed_key = keccak256(address.0);
        Ok(self
            .session
            .account_hashed_cursor(self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, account)| (key == hashed_key).then_some(account)))
    }
}

impl<S: MorphProofsBatchSession> StateProvider for MorphProofsBatchStateProviderRef<'_, S> {
    fn storage(&self, address: Address, storage_key: B256) -> ProviderResult<Option<StorageValue>> {
        let hashed_key = keccak256(storage_key);
        Ok(self
            .session
            .storage_hashed_cursor(keccak256(address.0), self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, storage)| (key == hashed_key).then_some(storage)))
    }
}

impl<S: MorphProofsBatchSession> BytecodeReader for MorphProofsBatchStateProviderRef<'_, S> {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.latest.bytecode_by_hash(code_hash)
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumHash;
    use alloy_primitives::U256;
    use reth_provider::noop::NoopProvider;
    use reth_revm::{
        db::{AccountStatus, BundleAccount},
        state::AccountInfo,
    };

    use super::*;
    use crate::{InMemoryProofsStorage, MorphProofsBatchStore, MorphProofsInitialStateStore};

    #[test]
    fn hashed_post_state_zeros_parent_storage_for_destroyed_account() {
        let address = Address::repeat_byte(0x11);
        let hashed_address = keccak256(address);
        let hashed_slot = B256::repeat_byte(0x22);
        let storage = InMemoryProofsStorage::new();
        storage
            .set_initial_state_anchor(BlockNumHash::new(0, B256::repeat_byte(0x01)))
            .unwrap();
        storage
            .store_hashed_storages(hashed_address, vec![(hashed_slot, U256::from(1))])
            .unwrap();
        storage.commit_initial_state().unwrap();

        let mut bundle_state = BundleState::default();
        bundle_state.state.insert(
            address,
            BundleAccount::new(
                Some(AccountInfo::default()),
                None,
                Default::default(),
                AccountStatus::Destroyed,
            ),
        );

        storage
            .with_batch_session(|session| {
                let provider = MorphProofsBatchStateProviderRef::new(
                    Box::<NoopProvider>::default(),
                    session,
                    0,
                );
                let hashed_state = provider.hashed_post_state(&bundle_state).unwrap();

                assert_eq!(
                    hashed_state.storages[&hashed_address].storage[&hashed_slot],
                    U256::ZERO
                );
                Ok(())
            })
            .unwrap();
    }
}
