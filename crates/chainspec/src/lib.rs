//! Morph chainspec implementation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

// Used only in tests, but declared here to silence unused_crate_dependencies warning
use serde_json as _;

pub mod hardfork;
pub mod spec;
pub use spec::MorphChainSpec;
