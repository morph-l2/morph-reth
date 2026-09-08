use alloy_primitives::B256;
use morph_proofs::{MdbxProofsStorage, MorphProofsStorageError, ProofDbIdentity};

#[test]
fn opening_a_store_persists_and_validates_chain_identity() {
    let directory = tempfile::tempdir().unwrap();
    let identity = ProofDbIdentity::new(2818, B256::repeat_byte(0x11));

    let store = MdbxProofsStorage::open(directory.path(), identity).unwrap();
    assert_eq!(store.proof_window().unwrap(), None);
    drop(store);

    MdbxProofsStorage::open(directory.path(), identity).unwrap();
    let error = MdbxProofsStorage::open(
        directory.path(),
        ProofDbIdentity::new(2819, identity.genesis_hash),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        MorphProofsStorageError::ChainIdentityMismatch("chain_id")
    ));
}
