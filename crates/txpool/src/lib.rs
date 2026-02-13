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
//! - [`MorphTxError`]: Error types for MorphTx (0x7F) validation
//!
//! # Transaction Validation
//!
//! The validator performs the following Morph-specific checks:
//! - Rejects EIP-4844 blob transactions (not supported on L2)
//! - Rejects L1 message transactions (only included by sequencer)
//! - Validates L1 data fee affordability
//! - Validates MorphTx (0x7F) ERC20 token balance and fee_limit
//!
//! # MorphTx (0x7F) Validation
//!
//! MorphTx allows users to pay gas fees using ERC20 tokens. The validator:
//! 1. Checks the token is registered and active in L2TokenRegistry
//! 2. Calculates required token amount: `eth_to_token(gas_fee + l1_data_fee)`
//! 3. Uses effective token limit semantics:
//!    - `fee_limit == 0` => treat as token balance
//!    - `fee_limit > balance` => cap by token balance
//! 4. Verifies effective limit can cover required token amount
//! 5. Verifies `eth_balance >= value` (and for `fee_token_id == 0`, ETH covers full cost)

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg), allow(unexpected_cfgs))]

mod error;
pub use error::MorphTxError;

mod transaction;
pub use transaction::MorphPooledTransaction;

mod validator;
pub use validator::{MorphL1BlockInfo, MorphTransactionValidator};

mod maintain;
pub use maintain::maintain_morph_pool;

mod morph_tx_validation;
pub use morph_tx_validation::{MorphTxValidationInput, MorphTxValidationResult, validate_morph_tx};

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
