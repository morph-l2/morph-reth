//! Transaction validator for Morph L2.
//!
//! This module provides Morph-specific transaction validation that extends the standard
//! Ethereum transaction validation with L2 checks:
//! - Rejection of EIP-4844 blob transactions
//! - Rejection of L1 message transactions from the pool
//! - L1 data fee validation
//! - MorphTx (0x7F) ERC20 token balance validation

use crate::MorphTxError;
use alloy_consensus::{BlockHeader, Transaction};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, U256};
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
/// - Validates MorphTx (0x7F) ERC20 token balance and fee_limit
///
/// # MorphTx Validation
///
/// For MorphTx (type 0x7F), this validator performs additional checks:
/// 1. Token must be registered and active in L2TokenRegistry
/// 2. Fee limit must be sufficient for the calculated token cost
/// 3. Token balance must cover the fee
/// 4. ETH balance must cover the transaction value (value is still in ETH)
///
/// # Balance Check Configuration
///
/// When using MorphTx, the inner `EthTransactionValidator` should have balance
/// checking disabled via `disable_balance_check()`, since MorphTx users may have
/// zero ETH balance but sufficient ERC20 tokens for gas payment.
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
    /// - Validates MorphTx (0x7F) ERC20 token balance and fee_limit
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

        // Check if this is a MorphTx (0x7F) - need special handling for ERC20 gas payment
        let is_morph_tx = is_morph_tx(&transaction);

        let outcome = self.inner.validate_one(origin, transaction);
        if outcome.is_invalid() || outcome.is_error() {
            tracing::trace!(target: "morph_txpool", ?outcome, "tx pool validation failed");
            return outcome;
        }

        // Ensure that the account has enough balance to cover fees
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

            // Calculate L1 data fee (always calculated for all transactions)
            let consensus_tx = valid_tx.transaction().clone_into_consensus();
            let mut encoded = Vec::with_capacity(consensus_tx.encode_2718_len());
            consensus_tx.encode_2718(&mut encoded);
            let l1_data_fee = l1_block_info.calculate_tx_l1_cost(&encoded, hardfork);

            if is_morph_tx {
                // MorphTx: validate ERC20 token balance
                let sender = valid_tx.transaction().sender();
                if let Err(err) = self.validate_morph_tx_balance(
                    valid_tx.transaction(),
                    sender,
                    balance,
                    l1_data_fee,
                    hardfork,
                ) {
                    return TransactionValidationOutcome::Invalid(
                        valid_tx.into_transaction(),
                        err.into(),
                    );
                }
            } else if self.requires_l1_data_gas_fee() {
                // Regular transaction: validate ETH balance covers cost + L1 fee
                let cost = valid_tx.transaction().cost().saturating_add(l1_data_fee);
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

    /// Validates MorphTx (0x7F) ERC20 token balance and fee_limit.
    ///
    /// This method performs the following checks (reference: go-ethereum tx_pool.go:727-791):
    /// 1. Token ID must be non-zero (0 is reserved for ETH)
    /// 2. Token must be registered in L2TokenRegistry
    /// 3. Token must be active for gas payment
    /// 4. Token price ratio must be valid (non-zero)
    /// 5. fee_limit must be >= required token amount
    /// 6. Token balance must be >= required token amount
    /// 7. ETH balance must be >= transaction value (value is still in ETH)
    fn validate_morph_tx_balance(
        &self,
        tx: &Tx,
        sender: Address,
        eth_balance: U256,
        l1_data_fee: U256,
        hardfork: morph_chainspec::hardfork::MorphHardfork,
    ) -> Result<(), MorphTxError> {
        let consensus_tx = tx.clone_into_consensus();

        // Get state provider for token info lookup
        let provider = self
            .client()
            .state_by_block_number_or_tag(self.block_number().into())
            .map_err(|err| MorphTxError::TokenInfoFetchFailed {
                token_id: 0, // token_id not yet extracted
                message: err.to_string(),
            })?;

        let mut db = StateProviderDatabase::new(provider);

        // Use shared validation logic with unified API (includes ETH balance check)
        let input = crate::MorphTxValidationInput {
            consensus_tx: &consensus_tx,
            sender,
            eth_balance,
            l1_data_fee,
            hardfork,
        };

        let result = crate::validate_morph_tx(&mut db, &input)?;

        tracing::trace!(
            target: "morph_txpool",
            fee_token_id = ?consensus_tx.fee_token_id(),
            fee_limit = ?consensus_tx.fee_limit(),
            required_token_amount = ?result.required_token_amount,
            token_balance = ?result.token_info.balance,
            l1_data_fee = ?l1_data_fee,
            eth_balance = ?eth_balance,
            tx_value = ?consensus_tx.value(),
            "MorphTx validation passed"
        );

        Ok(())
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

/// Helper function to check if a transaction is a MorphTx (0x7F).
fn is_morph_tx<Tx>(tx: &Tx) -> bool
where
    Tx: EthPoolTransaction<Consensus = MorphTxEnvelope>,
{
    tx.clone_into_consensus().is_morph_tx()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxEip1559, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{B256, Signature, TxKind, address};
    use morph_chainspec::MORPH_MAINNET;
    use morph_primitives::TxL1Msg;
    use reth_primitives_traits::Recovered;
    use reth_provider::test_utils::MockEthProvider;
    use reth_transaction_pool::{
        blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
    };

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

    #[test]
    fn validate_l1_message_rejected() {
        // Create validator with mock provider
        let client = MockEthProvider::default().with_chain_spec(MORPH_MAINNET.clone());
        let eth_validator = EthTransactionValidatorBuilder::new(client)
            .no_shanghai()
            .no_cancun()
            .build(InMemoryBlobStore::default());
        let validator = MorphTransactionValidator::new(eth_validator);

        let origin = TransactionOrigin::External;
        let signer = address!("0000000000000000000000000000000000000001");

        // Create L1 message transaction (type 0x7E)
        let l1_msg_tx = TxL1Msg {
            queue_index: 0,
            gas_limit: 21_000,
            to: address!("0000000000000000000000000000000000000002"),
            value: U256::ZERO,
            input: Default::default(),
            sender: signer,
        };
        let envelope = MorphTxEnvelope::L1Msg(alloy_consensus::Sealed::new_unchecked(
            l1_msg_tx,
            B256::ZERO,
        ));
        let recovered = Recovered::new_unchecked(envelope, signer);
        let len = recovered.encode_2718_len();
        let pooled_tx = crate::MorphPooledTransaction::new(recovered, len);

        // Validate and check rejection
        let outcome = validator.validate_one(origin, pooled_tx);

        let err = match outcome {
            TransactionValidationOutcome::Invalid(_, err) => err,
            _ => panic!("Expected invalid transaction for L1 message"),
        };
        assert_eq!(err.to_string(), "transaction type not supported");
    }

    #[test]
    fn validate_valid_eip1559_transaction() {
        // Create validator with mock provider and disable balance check for simplicity
        let client = MockEthProvider::default().with_chain_spec(MORPH_MAINNET.clone());
        let eth_validator = EthTransactionValidatorBuilder::new(client)
            .no_shanghai()
            .no_cancun()
            .disable_balance_check()
            .build(InMemoryBlobStore::default());
        let validator =
            MorphTransactionValidator::new(eth_validator).require_l1_data_gas_fee(false); // Disable L1 fee check for simplicity

        let origin = TransactionOrigin::External;
        let signer = address!("0000000000000000000000000000000000000001");

        // Create valid EIP-1559 transaction
        let tx = TxEip1559 {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            input: Default::default(),
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            access_list: Default::default(),
        };
        let signature = Signature::test_signature();
        let signed_tx = Signed::new_unchecked(tx, signature, B256::ZERO);
        let envelope = MorphTxEnvelope::Eip1559(signed_tx);
        let recovered = Recovered::new_unchecked(envelope, signer);
        let len = recovered.encode_2718_len();
        let pooled_tx = crate::MorphPooledTransaction::new(recovered, len);

        // Validate and check acceptance
        let outcome = validator.validate_one(origin, pooled_tx);

        match outcome {
            TransactionValidationOutcome::Valid { .. } => {
                // Success - transaction was accepted
            }
            TransactionValidationOutcome::Invalid(_, err) => {
                panic!("Expected valid transaction, got invalid: {err}");
            }
            TransactionValidationOutcome::Error(_, err) => {
                panic!("Expected valid transaction, got error: {err:?}");
            }
        }
    }

    #[test]
    fn validate_valid_legacy_transaction() {
        // Create validator with mock provider and disable balance check for simplicity
        let client = MockEthProvider::default().with_chain_spec(MORPH_MAINNET.clone());
        let eth_validator = EthTransactionValidatorBuilder::new(client)
            .no_shanghai()
            .no_cancun()
            .disable_balance_check()
            .build(InMemoryBlobStore::default());
        let validator =
            MorphTransactionValidator::new(eth_validator).require_l1_data_gas_fee(false); // Disable L1 fee check for simplicity

        let origin = TransactionOrigin::External;
        let signer = address!("0000000000000000000000000000000000000001");

        // Create valid Legacy transaction
        let tx = TxLegacy {
            chain_id: Some(2818),
            nonce: 0,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            input: Default::default(),
            gas_price: 2_000_000_000,
        };
        let signature = Signature::test_signature();
        let signed_tx = Signed::new_unchecked(tx, signature, B256::ZERO);
        let envelope = MorphTxEnvelope::Legacy(signed_tx);
        let recovered = Recovered::new_unchecked(envelope, signer);
        let len = recovered.encode_2718_len();
        let pooled_tx = crate::MorphPooledTransaction::new(recovered, len);

        // Validate and check acceptance
        let outcome = validator.validate_one(origin, pooled_tx);

        match outcome {
            TransactionValidationOutcome::Valid { .. } => {
                // Success - transaction was accepted
            }
            TransactionValidationOutcome::Invalid(_, err) => {
                panic!("Expected valid transaction, got invalid: {err}");
            }
            TransactionValidationOutcome::Error(_, err) => {
                panic!("Expected valid transaction, got error: {err:?}");
            }
        }
    }
}
