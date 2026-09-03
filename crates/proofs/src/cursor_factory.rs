//! Implements [`TrieCursorFactory`] and [`HashedCursorFactory`] for [`MorphProofsStore`] types.
//!
//! Both factories borrow one read-only [`MorphProofsStore::Tx`] for their entire lifetime
//! and route every cursor allocation through the `*_with_tx` fast path. This mirrors
//! reth's own `DatabaseTrieCursorFactory` / `DatabaseHashedCursorFactory` pattern and is
//! what lets proof, state-root, and witness requests acquire exactly one MDBX
//! transaction.

use alloy_primitives::B256;
use reth_db::DatabaseError;
use reth_trie::{hashed_cursor::HashedCursorFactory, trie_cursor::TrieCursorFactory};

use crate::{
    MorphProofsHashedAccountCursor, MorphProofsHashedStorageCursor, MorphProofsStorage,
    MorphProofsStore, MorphProofsTrieCursor,
    api::MorphProofsBatchSession,
    cursor::{
        MorphProofsHashedAccountCursor as RawHashedAccountCursor,
        MorphProofsHashedStorageCursor as RawHashedStorageCursor,
        MorphProofsTrieCursor as RawTrieCursor,
    },
};

/// Request-scoped factory that opens trie cursors against a shared read-only transaction.
///
/// Holds a borrow of the transaction so every cursor allocation reuses the same MDBX
/// reader slot. See [`MorphProofsStore::Tx`] for the underlying contention story.
#[derive(Debug, Clone)]
pub struct MorphProofsTrieCursorFactory<'tx, 'db, S: MorphProofsStore> {
    storage: &'db MorphProofsStorage<S>,
    tx: &'tx <MorphProofsStorage<S> as MorphProofsStore>::Tx<'db>,
    block_number: u64,
}

impl<'tx, 'db, S: MorphProofsStore> MorphProofsTrieCursorFactory<'tx, 'db, S> {
    /// Initializes a request-scoped trie cursor factory bound to `tx`.
    pub const fn new(
        storage: &'db MorphProofsStorage<S>,
        tx: &'tx <MorphProofsStorage<S> as MorphProofsStore>::Tx<'db>,
        block_number: u64,
    ) -> Self {
        Self {
            storage,
            tx,
            block_number,
        }
    }
}

impl<'tx, 'db, S> TrieCursorFactory for MorphProofsTrieCursorFactory<'tx, 'db, S>
where
    for<'a> S: MorphProofsStore + 'db,
    'db: 'tx,
{
    type AccountTrieCursor<'a>
        = MorphProofsTrieCursor<S::AccountTrieCursor<'a>>
    where
        Self: 'a;
    type StorageTrieCursor<'a>
        = MorphProofsTrieCursor<S::StorageTrieCursor<'a>>
    where
        Self: 'a;

    fn account_trie_cursor(&self) -> Result<Self::AccountTrieCursor<'_>, DatabaseError> {
        Ok(MorphProofsTrieCursor::new(
            self.storage
                .account_trie_cursor_with_tx(self.tx, self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor<'_>, DatabaseError> {
        Ok(MorphProofsTrieCursor::new(
            self.storage
                .storage_trie_cursor_with_tx(self.tx, hashed_address, self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }
}

/// Request-scoped factory that opens hashed cursors against a shared read-only transaction.
///
/// Mirror of [`MorphProofsTrieCursorFactory`] for the hashed account/storage tries.
#[derive(Debug, Clone)]
pub struct MorphProofsHashedAccountCursorFactory<'tx, 'db, S: MorphProofsStore> {
    storage: &'db MorphProofsStorage<S>,
    tx: &'tx <MorphProofsStorage<S> as MorphProofsStore>::Tx<'db>,
    block_number: u64,
}

impl<'tx, 'db, S: MorphProofsStore> MorphProofsHashedAccountCursorFactory<'tx, 'db, S> {
    /// Initializes a request-scoped hashed cursor factory bound to `tx`.
    pub const fn new(
        storage: &'db MorphProofsStorage<S>,
        tx: &'tx <MorphProofsStorage<S> as MorphProofsStore>::Tx<'db>,
        block_number: u64,
    ) -> Self {
        Self {
            storage,
            tx,
            block_number,
        }
    }
}

impl<'tx, 'db, S> HashedCursorFactory for MorphProofsHashedAccountCursorFactory<'tx, 'db, S>
where
    for<'a> S: MorphProofsStore + 'db,
    'db: 'tx,
{
    type AccountCursor<'a>
        = MorphProofsHashedAccountCursor<S::AccountHashedCursor<'a>>
    where
        Self: 'a;
    type StorageCursor<'a>
        = MorphProofsHashedStorageCursor<S::StorageCursor<'a>>
    where
        Self: 'a;

    fn hashed_account_cursor(&self) -> Result<Self::AccountCursor<'_>, DatabaseError> {
        Ok(MorphProofsHashedAccountCursor::new(
            self.storage
                .account_hashed_cursor_with_tx(self.tx, self.block_number)?,
        ))
    }

    fn hashed_storage_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageCursor<'_>, DatabaseError> {
        Ok(MorphProofsHashedStorageCursor::new(
            self.storage.storage_hashed_cursor_with_tx(
                self.tx,
                hashed_address,
                self.block_number,
            )?,
        ))
    }
}

/// Session-scoped trie cursor factory backed by a [`MorphProofsBatchSession`].
///
/// Cursors read from the session's active transaction and therefore observe writes
/// from earlier `store_trie_updates` calls in the same session.
#[derive(Debug)]
pub struct MorphProofsBatchTrieCursorFactory<'a, S: MorphProofsBatchSession> {
    session: &'a S,
    block_number: u64,
}

impl<S: MorphProofsBatchSession> Clone for MorphProofsBatchTrieCursorFactory<'_, S> {
    fn clone(&self) -> Self {
        Self {
            session: self.session,
            block_number: self.block_number,
        }
    }
}

impl<'a, S: MorphProofsBatchSession> MorphProofsBatchTrieCursorFactory<'a, S> {
    /// Initializes a session-scoped trie cursor factory.
    pub const fn new(session: &'a S, block_number: u64) -> Self {
        Self {
            session,
            block_number,
        }
    }
}

impl<S> TrieCursorFactory for MorphProofsBatchTrieCursorFactory<'_, S>
where
    S: MorphProofsBatchSession,
{
    type AccountTrieCursor<'a>
        = RawTrieCursor<S::AccountTrieCursor<'a>>
    where
        Self: 'a;
    type StorageTrieCursor<'a>
        = RawTrieCursor<S::StorageTrieCursor<'a>>
    where
        Self: 'a;

    fn account_trie_cursor(&self) -> Result<Self::AccountTrieCursor<'_>, DatabaseError> {
        Ok(RawTrieCursor::new(
            self.session
                .account_trie_cursor(self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor<'_>, DatabaseError> {
        Ok(RawTrieCursor::new(
            self.session
                .storage_trie_cursor(hashed_address, self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }
}

/// Session-scoped hashed cursor factory backed by a [`MorphProofsBatchSession`].
#[derive(Debug)]
pub struct MorphProofsBatchHashedAccountCursorFactory<'a, S: MorphProofsBatchSession> {
    session: &'a S,
    block_number: u64,
}

impl<S: MorphProofsBatchSession> Clone for MorphProofsBatchHashedAccountCursorFactory<'_, S> {
    fn clone(&self) -> Self {
        Self {
            session: self.session,
            block_number: self.block_number,
        }
    }
}

impl<'a, S: MorphProofsBatchSession> MorphProofsBatchHashedAccountCursorFactory<'a, S> {
    /// Initializes a session-scoped hashed cursor factory.
    pub const fn new(session: &'a S, block_number: u64) -> Self {
        Self {
            session,
            block_number,
        }
    }
}

impl<S> HashedCursorFactory for MorphProofsBatchHashedAccountCursorFactory<'_, S>
where
    S: MorphProofsBatchSession,
{
    type AccountCursor<'a>
        = RawHashedAccountCursor<S::AccountHashedCursor<'a>>
    where
        Self: 'a;
    type StorageCursor<'a>
        = RawHashedStorageCursor<S::StorageCursor<'a>>
    where
        Self: 'a;

    fn hashed_account_cursor(&self) -> Result<Self::AccountCursor<'_>, DatabaseError> {
        Ok(RawHashedAccountCursor::new(
            self.session.account_hashed_cursor(self.block_number)?,
        ))
    }

    fn hashed_storage_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageCursor<'_>, DatabaseError> {
        Ok(RawHashedStorageCursor::new(
            self.session
                .storage_hashed_cursor(hashed_address, self.block_number)?,
        ))
    }
}
