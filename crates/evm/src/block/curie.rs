//! Curie fork transition for Morph.
//!
//! The Curie hardfork introduced blob-based DA cost calculation for Morph L2.
//!
//! ## Changes
//!
//! 1. **Fee reduction** - Uses compressed blobs on L1 for lower DA costs.
//! 2. **Updated L1 gas oracle** - New storage slots for blob-based fee calculation:
//!    - `l1BlobBaseFee` slot initialized to 1
//!    - `commitScalar` slot initialized to `InitialCommitScalar`
//!    - `blobScalar` slot initialized to `InitialBlobScalar`
//!    - `isCurie` slot set to 1 (true)
//!
//! ## DA Cost Formula
//!
//! - Pre-Curie: `(l1GasUsed(txRlp) + overhead) * l1BaseFee * scalar`
//! - Post-Curie: `l1BaseFee * commitScalar + len(txRlp) * l1BlobBaseFee * blobScalar`
//!
//! Reference: `consensus/misc/curie.go` in morph go-ethereum

use morph_revm::{CURIE_L1_GAS_PRICE_ORACLE_STORAGE, L1_GAS_PRICE_ORACLE_ADDRESS};
use revm::{
    Database,
    database::{State, states::StorageSlot},
};

/// Applies the Morph Curie hard fork to the state.
///
/// Updates L1GasPriceOracle storage slots:
/// - Sets `l1BlobBaseFee` slot to 1
/// - Sets `commitScalar` slot to initial value
/// - Sets `blobScalar` slot to initial value
/// - Sets `isCurie` slot to 1 (true)
///
/// This function should only be called once at the Curie transition block.
pub(crate) fn apply_curie_hard_fork<DB: Database>(state: &mut State<DB>) -> Result<(), DB::Error> {
    tracing::info!(target: "morph::evm", "Applying Curie hard fork");

    let oracle = state.load_cache_account(L1_GAS_PRICE_ORACLE_ADDRESS)?;

    // Create storage updates
    let new_storage = CURIE_L1_GAS_PRICE_ORACLE_STORAGE
        .into_iter()
        .map(|(slot, present_value)| {
            (
                slot,
                StorageSlot {
                    present_value,
                    previous_or_original_value: oracle.storage_slot(slot).unwrap_or_default(),
                },
            )
        })
        .collect();

    // Get existing account info or use default
    let oracle_info = oracle.account_info().unwrap_or_default();

    // Create transition for oracle storage update
    let transition = oracle.change(oracle_info, new_storage);

    // Add transition to state
    if let Some(s) = state.transition_state.as_mut() {
        s.add_transitions(vec![(L1_GAS_PRICE_ORACLE_ADDRESS, transition)]);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use morph_revm::{
        GPO_BLOB_SCALAR_SLOT, GPO_COMMIT_SCALAR_SLOT, GPO_IS_CURIE_SLOT,
        GPO_L1_BLOB_BASE_FEE_SLOT, GPO_L1_BASE_FEE_SLOT, GPO_OVERHEAD_SLOT,
        GPO_OWNER_SLOT, GPO_SCALAR_SLOT, GPO_WHITELIST_SLOT,
        INITIAL_BLOB_SCALAR, INITIAL_COMMIT_SCALAR, INITIAL_L1_BLOB_BASE_FEE, IS_CURIE,
    };
    use revm::{
        database::{
            EmptyDB, State,
            states::{bundle_state::BundleRetention, plain_account::PlainStorage, StorageSlot},
        },
        primitives::U256,
        state::AccountInfo,
    };

    #[test]
    fn test_apply_curie_fork() {
        // init state
        let db = EmptyDB::new();
        let mut state = State::builder()
            .with_database(db)
            .with_bundle_update()
            .without_state_clear()
            .build();

        // oracle pre fork state
        let oracle_pre_fork = AccountInfo::default();
        let oracle_storage_pre_fork = PlainStorage::from_iter([
            (GPO_OWNER_SLOT, U256::from(0x15f50e5eu64)),  // placeholder owner
            (GPO_L1_BASE_FEE_SLOT, U256::from(0x15f50e5eu64)),
            (GPO_OVERHEAD_SLOT, U256::from(0x38u64)),
            (GPO_SCALAR_SLOT, U256::from(0x3e95ba80u64)),
            (GPO_WHITELIST_SLOT, U256::from(0x53u64)),  // placeholder whitelist
        ]);
        state.insert_account_with_storage(
            L1_GAS_PRICE_ORACLE_ADDRESS,
            oracle_pre_fork.clone(),
            oracle_storage_pre_fork.clone(),
        );

        // apply curie fork
        apply_curie_hard_fork(&mut state).expect("should apply curie fork");

        // merge transitions
        state.merge_transitions(BundleRetention::Reverts);
        let bundle = state.take_bundle();

        // check oracle contract contains storage changeset
        let oracle = bundle.state.get(&L1_GAS_PRICE_ORACLE_ADDRESS).unwrap().clone();
        let mut storage = oracle.storage.into_iter().collect::<Vec<(U256, StorageSlot)>>();
        storage.sort_by(|(a, _), (b, _)| a.cmp(b));

        let expected_storage = [
            (GPO_L1_BLOB_BASE_FEE_SLOT, INITIAL_L1_BLOB_BASE_FEE),
            (GPO_COMMIT_SCALAR_SLOT, INITIAL_COMMIT_SCALAR),
            (GPO_BLOB_SCALAR_SLOT, INITIAL_BLOB_SCALAR),
            (GPO_IS_CURIE_SLOT, IS_CURIE),
        ];

        for (got, expected) in storage.into_iter().zip(expected_storage) {
            assert_eq!(got.0, expected.0);
            assert_eq!(got.1, StorageSlot { present_value: expected.1, ..Default::default() });
        }
    }
}

