//! Transaction pool for Morph L2 node.
//!
//! This crate provides transaction pool validation and ordering for Morph L2.
//!
//! # Key Components
//!
//! - [`MorphPooledTransaction`]: Pool transaction wrapper with L2-specific caching
//! - [`MorphTransactionValidator`]: Transaction validator with L1 fee validation
//! - [`MorphL1BlockInfo`]: L1 block info tracker for fee calculation
//! - [`MorphTransactionPool`]: Type alias for the default Morph transaction pool
//!
//! # Transaction Validation
//!
//! The validator performs the following Morph-specific checks:
//! - Rejects EIP-4844 blob transactions (not supported on L2)
//! - Rejects L1 message transactions (only included by sequencer)
//! - Validates L1 data fee affordability (optional)
//!
//! # Usage
//!
//! ```
//! use morph_txpool::{MorphTransactionValidator, MorphPooledTransaction};
//! use reth_transaction_pool::{EthTransactionValidator, Pool, TransactionValidationTaskExecutor};
//!
//! // Create the inner Ethereum validator
//! let eth_validator = EthTransactionValidatorBuilder::new(client)
//!     .no_cancun()
//!     .build(blob_store);
//!
//! // Wrap with Morph-specific validation
//! let validator = MorphTransactionValidator::new(eth_validator);
//!
//! // Create the pool
//! let pool = Pool::new(
//!     TransactionValidationTaskExecutor::eth_builder(validator),
//!     ordering,
//!     blob_store,
//! );
//! ```

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg), allow(unexpected_cfgs))]

mod transaction;
pub use transaction::MorphPooledTransaction;

mod validator;
pub use validator::{MorphL1BlockInfo, MorphTransactionValidator};

use reth_transaction_pool::{CoinbaseTipOrdering, Pool, TransactionValidationTaskExecutor};

/// Type alias for default Morph transaction pool.
///
/// This pool uses:
/// - [`MorphTransactionValidator`] for transaction validation
/// - [`CoinbaseTipOrdering`] for transaction ordering (by effective gas tip)
/// - [`MorphPooledTransaction`] as the pooled transaction type
pub type MorphTransactionPool<Client, S, T = MorphPooledTransaction> = Pool<
    TransactionValidationTaskExecutor<MorphTransactionValidator<Client, T>>,
    CoinbaseTipOrdering<T>,
    S,
>;
