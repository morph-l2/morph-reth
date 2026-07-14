//! Provides proof operation implementations for [`MorphProofsStorage`].

use alloy_primitives::{
    Address, B256, Bytes, keccak256,
    map::{B256Map, HashMap},
};
use reth_db::DatabaseError;
use reth_execution_errors::{StateProofError, StateRootError, StorageRootError, TrieWitnessError};
use reth_trie::{
    StateRoot, StorageRoot, TrieType,
    hashed_cursor::HashedPostStateCursorFactory,
    metrics::TrieRootMetrics,
    proof::{self, Proof},
    trie_cursor::InMemoryTrieCursorFactory,
    witness::TrieWitness,
};
use reth_trie_common::{
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedPostStateSorted, HashedStorage,
    MultiProof, MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
    updates::TrieUpdates,
};

use crate::{
    MorphProofsHashedAccountCursorFactory, MorphProofsStorage, MorphProofsStore,
    MorphProofsTrieCursorFactory,
};

/// Build the trie + hashed cursor factories sharing one read transaction at the given block.
const fn from_tx<'tx, 'db, S>(
    storage: &'db MorphProofsStorage<S>,
    tx: &'tx <MorphProofsStorage<S> as MorphProofsStore>::Tx<'db>,
    block_number: u64,
) -> (
    MorphProofsTrieCursorFactory<'tx, 'db, S>,
    MorphProofsHashedAccountCursorFactory<'tx, 'db, S>,
)
where
    S: MorphProofsStore + 'db,
    'db: 'tx,
{
    (
        MorphProofsTrieCursorFactory::new(storage, tx, block_number),
        MorphProofsHashedAccountCursorFactory::new(storage, tx, block_number),
    )
}

/// Extends [`Proof`] with operations specific for working with [`MorphProofsStorage`].
pub trait DatabaseProof<'tx, S: MorphProofsStore + 'tx> {
    /// Generates the state proof for target account based on [`TrieInput`].
    fn overlay_account_proof(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> Result<AccountProof, StateProofError>;

    /// Generates an account proof while reusing an existing read transaction.
    fn overlay_account_proof_with_tx<'cursor>(
        storage: &'tx MorphProofsStorage<S>,
        tx: &'cursor <MorphProofsStorage<S> as MorphProofsStore>::Tx<'tx>,
        block_number: u64,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> Result<AccountProof, StateProofError>
    where
        'tx: 'cursor;

    /// Generates the state [`MultiProof`] for target hashed account and storage keys.
    fn overlay_multiproof(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> Result<MultiProof, StateProofError>;

    /// Generates a state multiproof while reusing an existing read transaction.
    fn overlay_multiproof_with_tx<'cursor>(
        storage: &'tx MorphProofsStorage<S>,
        tx: &'cursor <MorphProofsStorage<S> as MorphProofsStore>::Tx<'tx>,
        block_number: u64,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> Result<MultiProof, StateProofError>
    where
        'tx: 'cursor;
}

impl<'tx, S> DatabaseProof<'tx, S>
    for Proof<
        MorphProofsTrieCursorFactory<'tx, 'tx, S>,
        MorphProofsHashedAccountCursorFactory<'tx, 'tx, S>,
    >
where
    S: MorphProofsStore + 'tx + Clone,
{
    fn overlay_account_proof(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> Result<AccountProof, StateProofError> {
        let tx = storage.ro_tx().map_err(Into::<DatabaseError>::into)?;
        Self::overlay_account_proof_with_tx(storage, &tx, block_number, input, address, slots)
    }

    fn overlay_account_proof_with_tx<'cursor>(
        storage: &'tx MorphProofsStorage<S>,
        tx: &'cursor <MorphProofsStorage<S> as MorphProofsStore>::Tx<'tx>,
        block_number: u64,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> Result<AccountProof, StateProofError>
    where
        'tx: 'cursor,
    {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        let (trie_factory, hashed_factory) = from_tx(storage, tx, block_number);
        Proof::new(trie_factory.clone(), hashed_factory.clone())
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .account_proof(address, slots)
    }

    fn overlay_multiproof(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> Result<MultiProof, StateProofError> {
        let tx = storage.ro_tx().map_err(Into::<DatabaseError>::into)?;
        Self::overlay_multiproof_with_tx(storage, &tx, block_number, input, targets)
    }

    fn overlay_multiproof_with_tx<'cursor>(
        storage: &'tx MorphProofsStorage<S>,
        tx: &'cursor <MorphProofsStorage<S> as MorphProofsStore>::Tx<'tx>,
        block_number: u64,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> Result<MultiProof, StateProofError>
    where
        'tx: 'cursor,
    {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        let (trie_factory, hashed_factory) = from_tx(storage, tx, block_number);
        Proof::new(trie_factory.clone(), hashed_factory.clone())
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .multiproof(targets)
    }
}

/// Extends [`StorageProof`] with operations specific for working with [`MorphProofsStorage`].
pub trait DatabaseStorageProof<'tx, S: MorphProofsStore + 'tx> {
    /// Generates the storage proof for target slot based on [`TrieInput`].
    fn overlay_storage_proof(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        address: Address,
        slot: B256,
        storage: HashedStorage,
    ) -> Result<StorageProof, StateProofError>;

    /// Generates the storage multiproof for target slots based on [`TrieInput`].
    fn overlay_storage_multiproof(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        address: Address,
        slots: &[B256],
        storage: HashedStorage,
    ) -> Result<StorageMultiProof, StateProofError>;
}

impl<'tx, S> DatabaseStorageProof<'tx, S>
    for proof::StorageProof<
        'static,
        MorphProofsTrieCursorFactory<'tx, 'tx, S>,
        MorphProofsHashedAccountCursorFactory<'tx, 'tx, S>,
    >
where
    S: MorphProofsStore + 'tx + Clone,
{
    fn overlay_storage_proof(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> Result<StorageProof, StateProofError> {
        let hashed_address = keccak256(address);
        let prefix_set = hashed_storage.construct_prefix_set();
        let state_sorted = HashedPostStateSorted::new(
            Default::default(),
            HashMap::from_iter([(hashed_address, hashed_storage.into_sorted())]),
        );
        let tx = storage.ro_tx().map_err(Into::<DatabaseError>::into)?;
        let (trie_factory, hashed_factory) = from_tx(storage, &tx, block_number);
        proof::StorageProof::new(trie_factory, hashed_factory.clone(), address)
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_set_mut(prefix_set)
            .storage_proof(slot)
    }

    fn overlay_storage_multiproof(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> Result<StorageMultiProof, StateProofError> {
        let hashed_address = keccak256(address);
        let targets = slots.iter().map(keccak256).collect();
        let prefix_set = hashed_storage.construct_prefix_set();
        let state_sorted = HashedPostStateSorted::new(
            Default::default(),
            HashMap::from_iter([(hashed_address, hashed_storage.into_sorted())]),
        );
        let tx = storage.ro_tx().map_err(Into::<DatabaseError>::into)?;
        let (trie_factory, hashed_factory) = from_tx(storage, &tx, block_number);
        proof::StorageProof::new(trie_factory, hashed_factory.clone(), address)
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_set_mut(prefix_set)
            .storage_multiproof(targets)
    }
}

/// Extends [`StateRoot`] with operations specific for working with [`MorphProofsStorage`].
pub trait DatabaseStateRoot<'tx, S: MorphProofsStore + 'tx + Clone>: Sized {
    /// Calculate the state root for this [`HashedPostState`].
    /// Internally, this method retrieves prefixsets and uses them
    /// to calculate incremental state root.
    ///
    /// # Returns
    ///
    /// The state root for this [`HashedPostState`].
    fn overlay_root(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        post_state: HashedPostState,
    ) -> Result<B256, StateRootError>;

    /// Calculates the state root for this [`HashedPostState`] and returns it alongside trie
    /// updates. See [`Self::overlay_root`] for more info.
    fn overlay_root_with_updates(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        post_state: HashedPostState,
    ) -> Result<(B256, TrieUpdates), StateRootError>;

    /// Calculates the state root for provided [`HashedPostState`] using cached intermediate nodes.
    fn overlay_root_from_nodes(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        input: TrieInput,
    ) -> Result<B256, StateRootError>;

    /// Calculates the state root and trie updates for provided [`HashedPostState`] using
    /// cached intermediate nodes.
    fn overlay_root_from_nodes_with_updates(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        input: TrieInput,
    ) -> Result<(B256, TrieUpdates), StateRootError>;
}

impl<'tx, S> DatabaseStateRoot<'tx, S>
    for StateRoot<
        MorphProofsTrieCursorFactory<'tx, 'tx, S>,
        MorphProofsHashedAccountCursorFactory<'tx, 'tx, S>,
    >
where
    S: MorphProofsStore + 'tx + Clone,
{
    fn overlay_root(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        post_state: HashedPostState,
    ) -> Result<B256, StateRootError> {
        let prefix_sets = post_state.construct_prefix_sets().freeze();
        let state_sorted = post_state.into_sorted();
        let tx = storage.ro_tx().map_err(Into::<DatabaseError>::into)?;
        let (trie_factory, hashed_factory) = from_tx(storage, &tx, block_number);
        StateRoot::new(
            trie_factory,
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(prefix_sets)
        .root()
    }

    fn overlay_root_with_updates(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        post_state: HashedPostState,
    ) -> Result<(B256, TrieUpdates), StateRootError> {
        let prefix_sets = post_state.construct_prefix_sets().freeze();
        let state_sorted = post_state.into_sorted();
        let tx = storage.ro_tx().map_err(Into::<DatabaseError>::into)?;
        let (trie_factory, hashed_factory) = from_tx(storage, &tx, block_number);
        StateRoot::new(
            trie_factory,
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(prefix_sets)
        .root_with_updates()
    }

    fn overlay_root_from_nodes(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        input: TrieInput,
    ) -> Result<B256, StateRootError> {
        let state_sorted = input.state.into_sorted();
        let nodes_sorted = input.nodes.into_sorted();
        let tx = storage.ro_tx().map_err(Into::<DatabaseError>::into)?;
        let (trie_factory, hashed_factory) = from_tx(storage, &tx, block_number);
        StateRoot::new(
            InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted),
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root()
    }

    fn overlay_root_from_nodes_with_updates(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        input: TrieInput,
    ) -> Result<(B256, TrieUpdates), StateRootError> {
        let state_sorted = input.state.into_sorted();
        let nodes_sorted = input.nodes.into_sorted();
        let tx = storage.ro_tx().map_err(Into::<DatabaseError>::into)?;
        let (trie_factory, hashed_factory) = from_tx(storage, &tx, block_number);
        StateRoot::new(
            InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted),
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root_with_updates()
    }
}

/// Extends [`StorageRoot`] with operations specific for working with [`MorphProofsStorage`].
pub trait DatabaseStorageRoot<'tx, S: MorphProofsStore + 'tx + Clone> {
    /// Calculates the storage root for provided [`HashedStorage`].
    fn overlay_root(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> Result<B256, StorageRootError>;
}

impl<'tx, S> DatabaseStorageRoot<'tx, S>
    for StorageRoot<
        MorphProofsTrieCursorFactory<'tx, 'tx, S>,
        MorphProofsHashedAccountCursorFactory<'tx, 'tx, S>,
    >
where
    S: MorphProofsStore + 'tx + Clone,
{
    fn overlay_root(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> Result<B256, StorageRootError> {
        let prefix_set = hashed_storage.construct_prefix_set().freeze();
        let state_sorted =
            HashedPostState::from_hashed_storage(keccak256(address), hashed_storage).into_sorted();
        let tx = storage.ro_tx().map_err(Into::<DatabaseError>::into)?;
        let (trie_factory, hashed_factory) = from_tx(storage, &tx, block_number);
        StorageRoot::new(
            trie_factory,
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
            address,
            prefix_set,
            TrieRootMetrics::new(TrieType::Custom("morph_historical_proofs_storage")),
        )
        .root()
    }
}

/// Extends [`TrieWitness`] with operations specific for working with [`MorphProofsStorage`].
pub trait DatabaseTrieWitness<'tx, S: MorphProofsStore + 'tx + Clone> {
    /// Generates the trie witness for the target state based on [`TrieInput`].
    fn overlay_witness(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> Result<B256Map<Bytes>, TrieWitnessError>;

    /// Generates the trie witness for the target state, reusing `tx`.
    fn overlay_witness_with_tx<'cursor>(
        storage: &'tx MorphProofsStorage<S>,
        tx: &'cursor <MorphProofsStorage<S> as MorphProofsStore>::Tx<'tx>,
        block_number: u64,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> Result<B256Map<Bytes>, TrieWitnessError>
    where
        'tx: 'cursor;
}

impl<'tx, S> DatabaseTrieWitness<'tx, S>
    for TrieWitness<
        MorphProofsTrieCursorFactory<'tx, 'tx, S>,
        MorphProofsHashedAccountCursorFactory<'tx, 'tx, S>,
    >
where
    S: MorphProofsStore + 'tx + Clone,
{
    fn overlay_witness(
        storage: &'tx MorphProofsStorage<S>,
        block_number: u64,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> Result<B256Map<Bytes>, TrieWitnessError> {
        let tx = storage.ro_tx().map_err(|error| {
            let error = Into::<DatabaseError>::into(error);
            StateProofError::from(error)
        })?;
        Self::overlay_witness_with_tx(storage, &tx, block_number, input, target, mode)
    }

    fn overlay_witness_with_tx<'cursor>(
        storage: &'tx MorphProofsStorage<S>,
        tx: &'cursor <MorphProofsStorage<S> as MorphProofsStore>::Tx<'tx>,
        block_number: u64,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> Result<B256Map<Bytes>, TrieWitnessError>
    where
        'tx: 'cursor,
    {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        let (trie_factory, hashed_factory) = from_tx(storage, tx, block_number);
        TrieWitness::new(trie_factory.clone(), hashed_factory.clone())
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .always_include_root_node()
            .with_execution_witness_mode(mode)
            .compute(target)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
    use alloy_primitives::{Address, B256, U256, keccak256};
    use reth_primitives_traits::Account;
    use reth_trie::EMPTY_ROOT_HASH;

    use super::*;
    use crate::{
        BlockStateDiff, InMemoryProofsStorage, MdbxProofsStorage, MorphProofsInitialStateStore,
        MorphProofsStore,
    };

    fn assert_account_and_storage_proof<S>(storage: S)
    where
        S: MorphProofsStore + MorphProofsInitialStateStore + Clone,
    {
        let genesis_hash = B256::repeat_byte(0x01);
        storage
            .set_initial_state_anchor(BlockNumHash::new(0, genesis_hash))
            .unwrap();
        storage.commit_initial_state().unwrap();

        let address = Address::repeat_byte(0x11);
        let slot = B256::repeat_byte(0x22);
        let value = U256::from(42);
        let account = Account {
            nonce: 7,
            balance: U256::from(1_000_000),
            bytecode_hash: None,
        };
        let hashed_address = keccak256(address);
        let post_state = HashedPostState::default()
            .with_accounts([(hashed_address, Some(account))])
            .with_storages([(
                hashed_address,
                HashedStorage::from_iter(false, [(keccak256(slot), value)]),
            )]);

        let (state_root, trie_updates) =
            StateRoot::overlay_root_with_updates(&storage, 0, post_state.clone()).unwrap();
        let block_hash = B256::repeat_byte(0x02);
        storage
            .store_trie_updates(
                BlockWithParent::new(genesis_hash, NumHash::new(1, block_hash)),
                BlockStateDiff {
                    sorted_trie_updates: trie_updates.into_sorted(),
                    sorted_post_state: post_state.into_sorted(),
                },
            )
            .unwrap();

        let proof =
            Proof::overlay_account_proof(&storage, 1, TrieInput::default(), address, &[slot])
                .unwrap();

        assert_eq!(proof.info, Some(account));
        assert_eq!(proof.storage_proofs.len(), 1);
        assert_eq!(proof.storage_proofs[0].value, value);
        proof.verify(state_root).unwrap();
    }

    #[test]
    fn account_and_storage_proof_verify_in_memory() {
        assert_account_and_storage_proof(InMemoryProofsStorage::new());
    }

    #[test]
    fn account_and_storage_proof_verify_in_mdbx() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Arc::new(MdbxProofsStorage::new(directory.path()).unwrap());
        assert_account_and_storage_proof(storage);
    }

    #[test]
    fn proof_snapshot_survives_concurrent_prune_boundary_move() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Arc::new(MdbxProofsStorage::new(directory.path()).unwrap());
        let genesis_hash = B256::repeat_byte(0x01);
        storage
            .set_initial_state_anchor(BlockNumHash::new(0, genesis_hash))
            .unwrap();
        storage.commit_initial_state().unwrap();

        let address = Address::repeat_byte(0x11);
        let post_state = HashedPostState::default().with_accounts([(
            keccak256(address),
            Some(Account {
                nonce: 1,
                balance: U256::from(1),
                bytecode_hash: None,
            }),
        )]);
        let (_, trie_updates) =
            StateRoot::overlay_root_with_updates(&storage, 0, post_state.clone()).unwrap();
        let block_one =
            BlockWithParent::new(genesis_hash, NumHash::new(1, B256::repeat_byte(0x02)));
        storage
            .store_trie_updates(
                block_one,
                BlockStateDiff {
                    sorted_trie_updates: trie_updates.into_sorted(),
                    sorted_post_state: post_state.into_sorted(),
                },
            )
            .unwrap();

        let request_tx = storage.ro_tx().unwrap();
        assert_eq!(
            storage
                .get_earliest_block_number_with_tx(&request_tx)
                .unwrap()
                .unwrap()
                .0,
            0
        );

        storage.prune_earliest_state(block_one).unwrap();
        assert_eq!(storage.get_earliest_block_number().unwrap().unwrap().0, 1);
        assert_eq!(
            storage
                .get_earliest_block_number_with_tx(&request_tx)
                .unwrap()
                .unwrap()
                .0,
            0,
            "request snapshot must retain the validated proof window"
        );

        let proof = Proof::overlay_account_proof_with_tx(
            &storage,
            &request_tx,
            0,
            TrieInput::default(),
            address,
            &[],
        )
        .unwrap();
        assert!(proof.info.is_none());
        proof.verify(EMPTY_ROOT_HASH).unwrap();
    }

    #[test]
    fn exclusion_proofs_verify_at_historical_block() {
        let storage = InMemoryProofsStorage::new();
        let genesis_hash = B256::repeat_byte(0x01);
        storage
            .set_initial_state_anchor(BlockNumHash::new(0, genesis_hash))
            .unwrap();
        storage.commit_initial_state().unwrap();

        let address = Address::repeat_byte(0x11);
        let post_state = HashedPostState::default().with_accounts([(
            keccak256(address),
            Some(Account {
                nonce: 1,
                balance: U256::from(1),
                bytecode_hash: None,
            }),
        )]);
        let (state_root, trie_updates) =
            StateRoot::overlay_root_with_updates(&storage, 0, post_state.clone()).unwrap();
        storage
            .store_trie_updates(
                BlockWithParent::new(genesis_hash, NumHash::new(1, B256::repeat_byte(0x02))),
                BlockStateDiff {
                    sorted_trie_updates: trie_updates.into_sorted(),
                    sorted_post_state: post_state.into_sorted(),
                },
            )
            .unwrap();

        let missing_address = Address::repeat_byte(0x33);
        let missing_slot = B256::repeat_byte(0x44);
        let proof = Proof::overlay_account_proof(
            &storage,
            1,
            TrieInput::default(),
            missing_address,
            &[missing_slot],
        )
        .unwrap();

        assert!(proof.info.is_none());
        assert_eq!(proof.storage_proofs.len(), 1);
        assert_eq!(proof.storage_proofs[0].value, U256::ZERO);
        proof.verify(state_root).unwrap();
    }
}
