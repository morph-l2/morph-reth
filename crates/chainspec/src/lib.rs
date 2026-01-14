//! Morph chainspec implementation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

// Used only in tests, but declared here to silence unused_crate_dependencies warning
use serde_json as _;

pub mod constants;
pub mod genesis;
pub mod hardfork;
pub mod morph;
pub mod morph_hoodi;
pub mod spec;

// Re-export constants
pub use constants::*;

// Re-export genesis types
pub use genesis::{MorphChainConfig, MorphGenesisInfo, MorphHardforkInfo};

pub use morph::MORPH_MAINNET;
pub use morph_hoodi::MORPH_HOODI;
pub use spec::MorphChainSpec;

// Convenience re-export of the chain spec provider.
pub use reth_chainspec::ChainSpecProvider;
