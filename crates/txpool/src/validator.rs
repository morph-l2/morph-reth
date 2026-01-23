//! Transaction validator for Morph L2.
//!
//! This module provides Morph-specific transaction validation that extends the standard
//! Ethereum transaction validation with L2 checks:
//! - Rejection of EIP-4844 blob transactions
//! - Rejection of L1 message transactions from the pool
//! - L1 data fee validation

use alloy_consensus::BlockHeader;
use alloy_eips::Encodable2718;
use morph_chainspec::hardfork::MorphHardforks;
use morph_primitives::MorphTxEnvelope;
use morph_revm::L1BlockInfo;
use parking_lot::RwLock;
use reth_chainspec::ChainSpecProvider;
use reth_primitives_traits::{
    Block, GotExpected, SealedBlock, transaction::error::InvalidTransactionError,
};
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};
use reth_transaction_pool::{
    EthPoolTransaction, EthTransactionValidator, TransactionOrigin, TransactionValidationOutcome,
    TransactionValidator,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Tracks L1 block info for the current chain head.
///
/// This is used to cache L1 fee parameters and update them when the chain head changes.
#[derive(Debug, Default)]
pub struct MorphL1BlockInfo {
    /// The current L1 block info.
    l1_block_info: RwLock<L1BlockInfo>,
    /// Current block timestamp.
    timestamp: AtomicU64,
    /// Current block number.
    number: AtomicU64,
}

impl MorphL1BlockInfo {
    /// Creates a new instance with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current L1 block info.
    pub fn l1_block_info(&self) -> L1BlockInfo {
        self.l1_block_info.read().clone()
    }

    /// Updates the L1 block info.
    pub fn update(&self, info: L1BlockInfo, timestamp: u64, number: u64) {
        *self.l1_block_info.write() = info;
        self.timestamp.store(timestamp, Ordering::Relaxed);
        self.number.store(number, Ordering::Relaxed);
    }

    /// Returns the current block timestamp.
    pub fn timestamp(&self) -> u64 {
        self.timestamp.load(Ordering::Relaxed)
    }

    /// Returns the current block number.
    pub fn number(&self) -> u64 {
        self.number.load(Ordering::Relaxed)
    }
}

/// Validator for Morph L2 transactions.
///
/// This validator extends [`EthTransactionValidator`] with Morph-specific checks:
/// - Rejects EIP-4844 blob transactions (not supported on L2)
/// - Rejects L1 message transactions (only included by sequencer)
/// - Optionally validates L1 data fee affordability
#[derive(Debug)]
pub struct MorphTransactionValidator<Client, Tx> {
    /// The type that performs the actual validation.
    inner: EthTransactionValidator<Client, Tx>,
    /// Additional block info required for validation.
    block_info: Arc<MorphL1BlockInfo>,
    /// If true, ensure that the transaction's sender has enough balance to cover the L1 gas fee
    /// derived from the tracked L1 block info.
    require_l1_data_gas_fee: bool,
}

impl<Client, Tx> MorphTransactionValidator<Client, Tx> {
    /// Returns the configured chain spec.
    pub fn chain_spec(&self) -> Arc<Client::ChainSpec>
    where
        Client: ChainSpecProvider,
    {
        self.inner.chain_spec()
    }

    /// Returns the configured client.
    pub const fn client(&self) -> &Client {
        self.inner.client()
    }

    /// Returns the current block timestamp.
    fn block_timestamp(&self) -> u64 {
        self.block_info.timestamp()
    }

    /// Returns the current block number.
    fn block_number(&self) -> u64 {
        self.block_info.number()
    }

    /// Whether to ensure that the transaction's sender has enough balance to also cover the L1 gas
    /// fee.
    pub fn require_l1_data_gas_fee(self, require_l1_data_gas_fee: bool) -> Self {
        Self {
            require_l1_data_gas_fee,
            ..self
        }
    }

    /// Returns whether this validator also requires the transaction's sender to have enough balance
    /// to cover the L1 gas fee.
    pub const fn requires_l1_data_gas_fee(&self) -> bool {
        self.require_l1_data_gas_fee
    }

    /// Returns a reference to the block info tracker.
    pub fn block_info(&self) -> &Arc<MorphL1BlockInfo> {
        &self.block_info
    }
}

impl<Client, Tx> MorphTransactionValidator<Client, Tx>
where
    Client: ChainSpecProvider<ChainSpec: MorphHardforks> + StateProviderFactory + BlockReaderIdExt,
    Tx: EthPoolTransaction<Consensus = MorphTxEnvelope>,
{
    /// Create a new [`MorphTransactionValidator`].
    pub fn new(inner: EthTransactionValidator<Client, Tx>) -> Self {
        let this = Self::with_block_info(inner, MorphL1BlockInfo::default());
        if let Ok(Some(block)) = this
            .inner
            .client()
            .block_by_number_or_tag(alloy_eips::BlockNumberOrTag::Latest)
        {
            this.block_info
                .timestamp
                .store(block.header().timestamp(), Ordering::Relaxed);
            this.block_info
                .number
                .store(block.header().number(), Ordering::Relaxed);
            this.update_l1_block_info(block.header());
        }

        this
    }

    /// Create a new [`MorphTransactionValidator`] with the given [`MorphL1BlockInfo`].
    pub fn with_block_info(
        inner: EthTransactionValidator<Client, Tx>,
        block_info: MorphL1BlockInfo,
    ) -> Self {
        Self {
            inner,
            block_info: Arc::new(block_info),
            require_l1_data_gas_fee: true,
        }
    }

    /// Update the L1 block info for the given header.
    pub fn update_l1_block_info<H>(&self, header: &H)
    where
        H: BlockHeader,
    {
        self.block_info
            .timestamp
            .store(header.timestamp(), Ordering::Relaxed);
        self.block_info
            .number
            .store(header.number(), Ordering::Relaxed);

        let provider = match self
            .client()
            .state_by_block_number_or_tag(header.number().into())
        {
            Ok(provider) => provider,
            Err(err) => {
                tracing::warn!(target: "morph_txpool", %err, "Failed to get state provider for L1 block info update");
                return;
            }
        };

        let mut db = StateProviderDatabase::new(provider);
        let hardfork = self
            .chain_spec()
            .morph_hardfork_at(header.number(), header.timestamp());

        match L1BlockInfo::try_fetch(&mut db, hardfork) {
            Ok(l1_block_info) => {
                *self.block_info.l1_block_info.write() = l1_block_info;
            }
            Err(err) => {
                tracing::warn!(target: "morph_txpool", ?err, "Failed to fetch L1 block info");
            }
        }
    }

    /// Validates a single transaction.
    ///
    /// See also [`TransactionValidator::validate_transaction`]
    ///
    /// This behaves the same as [`EthTransactionValidator::validate_one`], but in addition:
    /// - Rejects EIP-4844 blob transactions
    /// - Rejects L1 message transactions
    /// - Ensures that the account has enough balance to cover the L1 gas cost (if enabled)
    pub fn validate_one(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
    ) -> TransactionValidationOutcome<Tx> {
        // Reject EIP-4844 blob transactions - not supported on L2
        if transaction.is_eip4844() {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidTransactionError::Eip4844Disabled.into(),
            );
        }

        // Reject L1 message transactions - only included by sequencer
        if is_l1_message(&transaction) {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidTransactionError::TxTypeNotSupported.into(),
            );
        }

        let outcome = self.inner.validate_one(origin, transaction);
        if outcome.is_invalid() || outcome.is_error() {
            tracing::trace!(target: "morph_txpool", ?outcome, "tx pool validation failed");
            return outcome;
        }

        if !self.requires_l1_data_gas_fee() {
            // No need to check L1 gas fee
            return outcome;
        }

        // Ensure that the account has enough balance to cover the L1 gas cost
        if let TransactionValidationOutcome::Valid {
            balance,
            state_nonce,
            transaction: valid_tx,
            propagate,
            bytecode_hash,
            authorities,
        } = outcome
        {
            let l1_block_info = self.block_info.l1_block_info.read().clone();
            let hardfork = self
                .chain_spec()
                .morph_hardfork_at(self.block_number(), self.block_timestamp());

            // Encode the transaction for L1 fee calculation
            let mut encoded = Vec::with_capacity(valid_tx.transaction().encoded_length());
            let tx = valid_tx.transaction().clone_into_consensus();
            tx.encode_2718(&mut encoded);

            // Calculate L1 data cost
            let cost_addition = l1_block_info.calculate_tx_l1_cost(&encoded, hardfork);

            let cost = valid_tx.transaction().cost().saturating_add(cost_addition);

            // Check if the user has enough balance to cover L2 gas + L1 data fee
            if cost > balance {
                return TransactionValidationOutcome::Invalid(
                    valid_tx.into_transaction(),
                    InvalidTransactionError::InsufficientFunds(
                        GotExpected {
                            got: balance,
                            expected: cost,
                        }
                        .into(),
                    )
                    .into(),
                );
            }

            return TransactionValidationOutcome::Valid {
                balance,
                state_nonce,
                bytecode_hash,
                transaction: valid_tx,
                propagate,
                authorities,
            };
        }

        outcome
    }

    /// Validates all given transactions.
    ///
    /// Returns all outcomes for the given transactions in the same order.
    ///
    /// See also [`Self::validate_one`]
    pub fn validate_all(
        &self,
        transactions: Vec<(TransactionOrigin, Tx)>,
    ) -> Vec<TransactionValidationOutcome<Tx>> {
        transactions
            .into_iter()
            .map(|(origin, tx)| self.validate_one(origin, tx))
            .collect()
    }
}

impl<Client, Tx> TransactionValidator for MorphTransactionValidator<Client, Tx>
where
    Client: ChainSpecProvider<ChainSpec: MorphHardforks> + StateProviderFactory + BlockReaderIdExt,
    Tx: EthPoolTransaction<Consensus = MorphTxEnvelope>,
{
    type Transaction = Tx;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        self.validate_one(origin, transaction)
    }

    async fn validate_transactions(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        self.validate_all(transactions)
    }

    fn on_new_head_block<B>(&self, new_tip_block: &SealedBlock<B>)
    where
        B: Block,
    {
        self.inner.on_new_head_block(new_tip_block);
        self.update_l1_block_info(new_tip_block.header());
    }
}

/// Helper function to check if a transaction is an L1 message.
fn is_l1_message<Tx>(tx: &Tx) -> bool
where
    Tx: EthPoolTransaction<Consensus = MorphTxEnvelope>,
{
    tx.clone_into_consensus().is_l1_msg()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_morph_l1_block_info_default() {
        let info = MorphL1BlockInfo::new();
        assert_eq!(info.timestamp(), 0);
        assert_eq!(info.number(), 0);
    }

    #[test]
    fn test_morph_l1_block_info_update() {
        let info = MorphL1BlockInfo::new();
        let l1_info = L1BlockInfo::default();
        info.update(l1_info, 1234, 100);

        assert_eq!(info.timestamp(), 1234);
        assert_eq!(info.number(), 100);
    }
}
