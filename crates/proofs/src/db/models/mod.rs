//! MDBX implementation of [`MorphProofsStore`](crate::MorphProofsStore).
//!
//! This module provides a complete MDBX implementation of the
//! [`MorphProofsStore`](crate::MorphProofsStore) trait. It uses the [`reth_db`] crate for
//! database interactions and defines the necessary tables and models for storing trie branches,
//! accounts, and storage leaves.

mod block;
pub use block::*;
mod version;
pub use version::*;
mod storage;
pub use storage::*;
mod change_set;
mod kv;
mod metadata;

use alloy_primitives::B256;
pub use change_set::*;
pub use kv::*;
pub use metadata::*;
use reth_db::{
    TableSet,
    table::{DupSort, Table, TableInfo},
};
use reth_primitives_traits::Account;
use reth_trie_common::{BranchNodeCompact, StoredNibbles};

/// Declares this database's tables: a marker type with a [`Table`] impl per entry, plus the
/// [`Tables`] set passed to `init_db_for`.
///
/// `reth_db::tables!` cannot be used here: it sets [`Table::TABLE`] to the enum it generates,
/// while that constant has to name an entry of reth's own `Tables`. Tables defined outside that
/// enum leave it at its default of `None`, which makes MDBX resolve them by name.
macro_rules! tables {
    (@is_dupsort) => { false };
    (@is_dupsort $subkey:ty) => { true };

    ($($(#[$attr:meta])* table $name:ident {
        type Key = $key:ty;
        type Value = $value:ty;
        $(type SubKey = $subkey:ty;)?
    })*) => {
        $(
            $(#[$attr])*
            #[derive(Debug)]
            pub enum $name {}

            impl Table for $name {
                const NAME: &'static str = stringify!($name);
                const DUPSORT: bool = tables!(@is_dupsort $($subkey)?);

                type Key = $key;
                type Value = $value;
            }

            $(
                impl DupSort for $name {
                    type SubKey = $subkey;
                }
            )?
        )*

        /// A table in this database.
        #[derive(Clone, Copy, Debug)]
        pub enum Tables {
            $(
                #[doc = concat!("The [`", stringify!($name), "`] table.")]
                $name,
            )*
        }

        impl TableInfo for Tables {
            fn name(&self) -> &'static str {
                match self {
                    $(Self::$name => stringify!($name),)*
                }
            }

            fn is_dupsort(&self) -> bool {
                match self {
                    $(Self::$name => tables!(@is_dupsort $($subkey)?),)*
                }
            }
        }

        impl TableSet for Tables {
            fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
                Box::new(
                    [$(Self::$name,)*]
                        .into_iter()
                        .map(|table| Box::new(table) as Box<dyn TableInfo>),
                )
            }
        }
    };
}

tables! {
    /// Stores historical branch nodes for the account state trie.
    ///
    /// Each entry maps a compact-encoded trie path (`StoredNibbles`) to its versioned branch node.
    /// Multiple versions of the same node are stored using the block number as a subkey.
    table AccountTrieHistory {
        type Key = StoredNibbles;
        type Value = VersionedValue<BranchNodeCompact>;
        type SubKey = u64; // block number
    }

    /// Stores historical branch nodes for the storage trie of each account.
    ///
    /// Each entry is identified by a composite key combining the account’s hashed address and the
    /// compact-encoded trie path. Versions are tracked using block numbers as subkeys.
    table StorageTrieHistory {
        type Key = StorageTrieKey;
        type Value = VersionedValue<BranchNodeCompact>;
        type SubKey = u64; // block number
    }

    /// Stores versioned account state across block history.
    ///
    /// Each entry maps a hashed account address to its serialized account data (balance, nonce,
    /// code hash, storage root).
    table HashedAccountHistory {
        type Key = B256;
        type Value = VersionedValue<Account>;
        type SubKey = u64; // block number
    }

    /// Stores versioned storage state across block history.
    ///
    /// Each entry maps a composite key of (hashed address, storage key) to its stored value.
    /// Used for reconstructing contract storage at any historical block height.
    table HashedStorageHistory {
        type Key = HashedStorageKey;
        type Value = VersionedValue<StorageValue>;
        type SubKey = u64; // block number
    }

    /// Tracks the active proof window in the external historical storage.
    ///
    /// Stores the earliest and latest block numbers (and corresponding hashes)
    /// for which historical trie data is retained.
    table ProofWindow {
      type Key = ProofWindowKey;
      type Value = BlockNumberHash;
    }

    /// Stores schema and canonical-chain identity for fail-fast cold-copy validation.
    table ProofMetadata {
        type Key = ProofMetadataKey;
        type Value = ProofMetadataValue;
    }

    /// A reverse mapping of block numbers to a keys of the tables.
    /// This is used for efficiently locating data by block number.
    table BlockChangeSet {
        type Key = u64; // Block number
        type Value = ChangeSet;
    }
}
