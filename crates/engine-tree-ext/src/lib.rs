//! Morph extension of reth's engine tree.
//!
//! Verbatim copy of `reth_engine_tree::tree::payload_validator` from reth v2.0.0,
//! used as the foundation for a Jade-gated state-root skip.
//!
//! `trie_updates` is a verbatim copy of the private sibling module from the same
//! source tree, required by `payload_validator::compare_trie_updates_with_serial`.

pub mod gate;
pub mod payload_validator;
pub(crate) mod trie_updates;

pub use payload_validator::MorphBasicEngineValidator;
