//! Morph-specific revm implementations.
//!
//! This crate provides low-level EVM execution logic for Morph L2, including
//! custom transaction validation, fee handling, and L1 data fee calculation.
//!
//! # Main Components
//!
//! - [`MorphEvm`]: The main EVM type with Morph-specific handler
//! - [`handler::MorphEvmHandler`]: Custom handler implementing fee deduction and reimbursement
//! - [`L1BlockInfo`]: L1 gas price oracle data for L1 data fee calculation
//! - [`TokenFeeInfo`]: Token registry data for AltFee transaction support
//!
//! # Fee Handling
//!
//! Morph L2 has three types of fees:
//!
//! 1. **L2 Execution Fee**: Standard gas fee for EVM execution
//! 2. **L1 Data Fee**: Fee for posting transaction data to L1 (calculated from L1 gas price)
//! 3. **AltFee Token Fee**: Optional ERC20 token payment instead of ETH
//!
//! # L1 Messages
//!
//! L1 message transactions (type `0x7E`) are special:
//! - No signature validation required
//! - No fee deduction (paid on L1)
//! - Sender is recovered from the transaction itself
//!
//! # Error Types
//!
//! - [`MorphHaltReason`]: EVM halt reasons (extends revm's `HaltReason`)
//! - [`MorphInvalidTransaction`]: Invalid transaction errors

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod block;
// Suppress unused_crate_dependencies warnings
#[cfg(not(test))]
use alloy_consensus as _;
#[cfg(not(test))]
use alloy_sol_types as _;
#[cfg(not(test))]
use tracing as _;

mod common;
pub use common::{MorphStateAccess, MorphTx};
pub mod error;
pub mod evm;
pub mod exec;
pub mod handler;
pub mod l1block;
pub mod precompiles;
pub mod token_fee;
mod tx;

pub use block::MorphBlockEnv;
pub use error::{MorphHaltReason, MorphInvalidTransaction};
pub use evm::MorphEvm;
pub use l1block::{
    CURIE_L1_GAS_PRICE_ORACLE_STORAGE,
    GPO_BLOB_SCALAR_SLOT,
    GPO_COMMIT_SCALAR_SLOT,
    GPO_IS_CURIE_SLOT,
    GPO_L1_BASE_FEE_SLOT,
    GPO_L1_BLOB_BASE_FEE_SLOT,
    GPO_OVERHEAD_SLOT,
    // Storage slots
    GPO_OWNER_SLOT,
    GPO_SCALAR_SLOT,
    GPO_WHITELIST_SLOT,
    INITIAL_BLOB_SCALAR,
    INITIAL_COMMIT_SCALAR,
    // Curie initial values
    INITIAL_L1_BLOB_BASE_FEE,
    IS_CURIE,
    L1_GAS_PRICE_ORACLE_ADDRESS,
    L1BlockInfo,
};
pub use token_fee::{L2_TOKEN_REGISTRY_ADDRESS, TokenFeeInfo, get_erc20_balance_with_evm};
pub use tx::{MorphTxEnv, MorphTxExt};
