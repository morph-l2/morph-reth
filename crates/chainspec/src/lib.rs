//! Morph chain specification implementation.
//!
//! This crate provides chain specification types for Morph L2, including:
//!
//! - [`MorphChainSpec`]: The main chain specification type that wraps reth's `ChainSpec`
//!   with Morph-specific configuration.
//! - [`hardfork::MorphHardfork`]: Morph-specific hardfork definitions (Bernoulli, Curie, Morph203, etc.)
//! - [`MorphChainConfig`]: Morph L2-specific chain configuration (fee vault, max tx size, etc.)
//!
//! # Supported Networks
//!
//! - [`MORPH_MAINNET`]: Morph Mainnet (chain ID: 2818)
//! - [`MORPH_HOODI`]: Morph Hoodi Testnet (chain ID: 2910)
//!
//! # Hardfork Activation
//!
//! Morph hardforks use two activation mechanisms:
//! - **Block-based**: Bernoulli, Curie (activated at specific block numbers)
//! - **Timestamp-based**: Morph203, Viridian, Emerald (activated at specific timestamps)
//!
//! # Example
//!
//! ```
//! use morph_chainspec::{MorphChainSpec, MORPH_MAINNET};
//! use morph_chainspec::hardfork::MorphHardforks;
//!
//! // Use predefined mainnet spec
//! let mainnet = MORPH_MAINNET.clone();
//! assert!(mainnet.is_bernoulli_active_at_block(0));
//! ```
//!
//! Create from genesis JSON:
//!
//! ```no_run
//! use alloy_genesis::Genesis;
//! use morph_chainspec::MorphChainSpec;
//!
//! let genesis_json = std::fs::read_to_string("genesis.json").unwrap();
//! let genesis: Genesis = serde_json::from_str(&genesis_json).unwrap();
//! let chain_spec = MorphChainSpec::from(genesis);
//! ```

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

// Used only for feature propagation (alloy-consensus/serde), but declared here
// to silence the unused_crate_dependencies warning.
use alloy_consensus as _;

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

// Re-export hardfork types
pub use hardfork::{MorphHardfork, MorphHardforks};

pub use morph::MORPH_MAINNET;
pub use morph_hoodi::MORPH_HOODI;
pub use spec::MorphChainSpec;

// Convenience re-export of the chain spec provider.
pub use reth_chainspec::ChainSpecProvider;
