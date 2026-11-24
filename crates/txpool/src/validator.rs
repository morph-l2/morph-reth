//! Transaction validator for Morph L2.
//!
//! This module provides Morph-specific transaction validation that extends the standard
//! Ethereum transaction validation with L2 checks:
//! - Rejection of EIP-4844 blob transactions
//! - EIP-3860 max initcode size enforcement
//! - Rejection of L1 message transactions from the pool
//! - L1 data fee validation
//! - MorphTx (0x7F) ERC20 token balance validation

use crate::{
    MorphTxError,
    morph_tx_validation::validate_morph_tx_with,
    state_cache::{StateCache, TokenFeeValidator},
};
use alloy_consensus::{BlockHeader, Transaction};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_primitives::{Address, B256, U256};
use morph_chainspec::hardfork::{MorphHardfork, MorphHardforks};
use morph_primitives::MorphTxEnvelope;
use morph_revm::{L1BlockInfo, MorphBlockEnv, MorphEvmEnv, morph_evm_env};
use parking_lot::RwLock;
use reth_chainspec::ChainSpecProvider;
use reth_evm::{ConfigureEvm, database::StateProviderDatabase};
use reth_primitives_traits::{
    Block, BlockTy, GotExpected, SealedBlock, transaction::error::InvalidTransactionError,
};
use reth_storage_api::{
    BlockReaderIdExt, StateProvider as _, StateProviderBox, StateProviderFactory,
    errors::ProviderResult,
};
use reth_transaction_pool::{
    EthPoolTransaction, EthTransactionValidator, PoolTransaction, TransactionOrigin,
    TransactionValidationOutcome, TransactionValidator,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// EIP-3860 max initcode size (`2 * MAX_CODE_SIZE = 2 * 24 576 = 49 152` bytes).
///
/// Reuses evm2's canonical constant, the base of the limit reth's
/// `EthTransactionValidator` applies, so it stays pinned to the EIP-170
/// code-size base instead of a hand-copied literal. Enforced unconditionally at
/// the txpool layer because Morph has been post-Shanghai since genesis; see
/// `validate_one_with_state` for why we can't rely on reth's Shanghai-gated check.
const MAX_INITCODE_SIZE: usize = evm2::constants::MAX_INITCODE_SIZE;

/// Tracks L1 block info and head header context for the current chain head.
///
/// This is used to cache L1 fee parameters and construct the matching [`MorphEvmEnv`]
/// when the chain head changes.
#[derive(Debug)]
pub struct MorphL1BlockInfo {
    /// The current L1 block info.
    l1_block_info: RwLock<L1BlockInfo>,
    /// Current block base fee per gas.
    base_fee_per_gas: RwLock<Option<u64>>,
    /// Current block timestamp.
    timestamp: AtomicU64,
    /// Current block number.
    number: AtomicU64,
    /// Current block gas limit.
    gas_limit: AtomicU64,
    /// Current block beneficiary.
    beneficiary: RwLock<Address>,
    /// Current block prevrandao / mix_hash.
    prevrandao: RwLock<B256>,
    /// Current block difficulty.
    difficulty: RwLock<U256>,
}

impl Default for MorphL1BlockInfo {
    fn default() -> Self {
        Self {
            l1_block_info: RwLock::default(),
            base_fee_per_gas: RwLock::default(),
            timestamp: AtomicU64::new(0),
            number: AtomicU64::new(0),
            gas_limit: AtomicU64::new(u64::MAX),
            beneficiary: RwLock::default(),
            prevrandao: RwLock::default(),
            difficulty: RwLock::default(),
        }
    }
}

impl MorphL1BlockInfo {
    /// Creates a new instance with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current L1 block info.
    pub fn l1_block_info(&self) -> L1BlockInfo {
        *self.l1_block_info.read()
    }

    /// Updates the L1 block info.
    pub fn update(
        &self,
        info: L1BlockInfo,
        timestamp: u64,
        number: u64,
        base_fee_per_gas: Option<u64>,
    ) {
        *self.l1_block_info.write() = info;
        *self.base_fee_per_gas.write() = base_fee_per_gas;
        self.timestamp.store(timestamp, Ordering::Relaxed);
        self.number.store(number, Ordering::Relaxed);
    }

    /// Records the execution-visible header fields from `header`.
    fn update_header<H: BlockHeader>(&self, header: &H) {
        self.timestamp.store(header.timestamp(), Ordering::Relaxed);
        self.number.store(header.number(), Ordering::Relaxed);
        self.gas_limit.store(header.gas_limit(), Ordering::Relaxed);
        *self.base_fee_per_gas.write() = header.base_fee_per_gas();
        *self.beneficiary.write() = header.beneficiary();
        *self.prevrandao.write() = header.mix_hash().unwrap_or_default();
        *self.difficulty.write() = header.difficulty();
    }

    /// Builds a [`MorphEvmEnv`] from the tracked head header fields.
    fn evm_env(&self, hardfork: MorphHardfork, chain_id: u64) -> MorphEvmEnv {
        let gas_limit = self.gas_limit.load(Ordering::Relaxed);
        let block_env = MorphBlockEnv {
            number: U256::from(self.number()),
            beneficiary: *self.beneficiary.read(),
            timestamp: U256::from(self.timestamp()),
            difficulty: *self.difficulty.read(),
            prevrandao: U256::from_be_bytes(self.prevrandao.read().0),
            gas_limit: U256::from(gas_limit),
            basefee: U256::from(self.base_fee_per_gas().unwrap_or_default()),
            blob_basefee: U256::ONE,
            ..Default::default()
        };
        morph_evm_env(hardfork, chain_id, gas_limit, block_env)
    }

    /// Returns the current block timestamp.
    pub fn timestamp(&self) -> u64 {
        self.timestamp.load(Ordering::Relaxed)
    }

    /// Returns the current block number.
    pub fn number(&self) -> u64 {
        self.number.load(Ordering::Relaxed)
    }

    /// Returns the current block base fee per gas.
    pub fn base_fee_per_gas(&self) -> Option<u64> {
        *self.base_fee_per_gas.read()
    }
}

/// Validator for Morph L2 transactions.
///
/// This validator extends [`EthTransactionValidator`] with Morph-specific checks:
/// - Rejects EIP-4844 blob transactions (not supported on L2)
/// - Rejects L1 message transactions (only included by sequencer)
/// - Validates L1 data fee affordability
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
pub struct MorphTransactionValidator<Client, Tx, Evm = morph_evm::MorphEvmConfig> {
    /// The type that performs the actual validation.
    inner: EthTransactionValidator<Client, Tx, Evm>,
    /// Additional block info required for validation.
    block_info: Arc<MorphL1BlockInfo>,
    /// Tip hash and shared state read cache across validations at the same canonical tip.
    cached_state: RwLock<(B256, Arc<StateCache>)>,
}

impl<Client, Tx, Evm> MorphTransactionValidator<Client, Tx, Evm> {
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

    /// Returns a reference to the block info tracker.
    pub fn block_info(&self) -> &Arc<MorphL1BlockInfo> {
        &self.block_info
    }
}

fn insufficient_funds_outcome<Tx: PoolTransaction>(
    transaction: Tx,
    balance: U256,
    cost: U256,
) -> TransactionValidationOutcome<Tx> {
    TransactionValidationOutcome::Invalid(
        transaction,
        InvalidTransactionError::InsufficientFunds(
            GotExpected {
                got: balance,
                expected: cost,
            }
            .into(),
        )
        .into(),
    )
}

impl<Client, Tx, Evm> MorphTransactionValidator<Client, Tx, Evm>
where
    Client: ChainSpecProvider<ChainSpec: MorphHardforks> + StateProviderFactory + BlockReaderIdExt,
    Tx: EthPoolTransaction<Consensus = MorphTxEnvelope>,
    Evm: ConfigureEvm,
{
    /// Create a new [`MorphTransactionValidator`].
    pub fn new(inner: EthTransactionValidator<Client, Tx, Evm>) -> Self {
        let this = Self::with_block_info(inner, MorphL1BlockInfo::default());
        if let Ok(Some(block)) = this
            .inner
            .client()
            .block_by_number_or_tag(alloy_eips::BlockNumberOrTag::Latest)
        {
            this.update_l1_block_info(block.header());
        }

        this
    }

    /// Create a new [`MorphTransactionValidator`] with the given [`MorphL1BlockInfo`].
    pub fn with_block_info(
        inner: EthTransactionValidator<Client, Tx, Evm>,
        block_info: MorphL1BlockInfo,
    ) -> Self {
        Self {
            inner,
            block_info: Arc::new(block_info),
            cached_state: RwLock::new((B256::ZERO, Arc::new(StateCache::default()))),
        }
    }

    /// Update the L1 block info for the given header.
    pub fn update_l1_block_info<H>(&self, header: &H)
    where
        H: BlockHeader,
    {
        *self.cached_state.write() = (B256::ZERO, Arc::new(StateCache::default()));
        self.update_l1_block_info_with_hash(header, None);
    }

    fn update_l1_block_info_with_hash<H>(&self, header: &H, tip_hash: Option<B256>)
    where
        H: BlockHeader,
    {
        self.block_info.update_header(header);

        let provider_res = match tip_hash {
            Some(hash) => self.client().state_by_block_hash(hash).or_else(|_| {
                self.client()
                    .state_by_block_number_or_tag(header.number().into())
            }),
            None => self
                .client()
                .state_by_block_number_or_tag(header.number().into()),
        };

        let provider = match provider_res {
            Ok(provider) => provider,
            Err(err) => {
                tracing::warn!(target: "morph::txpool", %err, "Failed to get state provider for L1 block info update");
                return;
            }
        };

        let mut db = StateProviderDatabase::new((&provider).into_evm_state_provider());
        let hardfork = self
            .chain_spec()
            .morph_hardfork_at(header.number(), header.timestamp());

        match L1BlockInfo::try_fetch(&mut db, hardfork) {
            Ok(l1_block_info) => {
                *self.block_info.l1_block_info.write() = l1_block_info;
            }
            Err(err) => {
                tracing::warn!(target: "morph::txpool", ?err, "Failed to fetch L1 block info");
            }
        }
    }

    /// Returns the state provider for the current canonical tip together with its tip-scoped cache.
    fn latest_state_provider_and_cache(
        &self,
    ) -> ProviderResult<(StateProviderBox, Arc<StateCache>)> {
        if let Ok(info) = self.client().chain_info()
            && !info.best_hash.is_zero()
            && let Ok(provider) = self.client().state_by_block_hash(info.best_hash)
        {
            return Ok((provider, self.state_cache_for_tip(info.best_hash)));
        }

        let provider = self
            .client()
            .state_by_block_number_or_tag(self.block_number().into())
            .or_else(|_| self.client().latest())?;
        Ok((provider, Arc::new(StateCache::default())))
    }

    fn state_cache_for_tip(&self, tip_hash: B256) -> Arc<StateCache> {
        {
            let cached = self.cached_state.read();
            if cached.0 == tip_hash {
                return Arc::clone(&cached.1);
            }
        }
        let mut cached = self.cached_state.write();
        if cached.0 != tip_hash {
            *cached = (tip_hash, Arc::new(StateCache::default()));
        }
        Arc::clone(&cached.1)
    }

    /// Validates a single transaction.
    ///
    /// See also [`TransactionValidator::validate_transaction`]
    ///
    /// This behaves the same as [`EthTransactionValidator::validate_one`], but in addition:
    /// - Rejects EIP-4844 blob transactions
    /// - Rejects L1 message transactions
    /// - Validates MorphTx (0x7F) ERC20 token balance and fee_limit
    /// - Ensures that the account has enough balance to cover the L1 gas cost
    pub fn validate_one(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
    ) -> TransactionValidationOutcome<Tx> {
        let mut outcomes = self.validate_all(vec![(origin, transaction)]);
        outcomes
            .pop()
            .expect("single input produces single outcome")
    }

    /// Validates a single transaction, reusing an optional state provider.
    ///
    /// When `state` is `None`, a fresh provider is fetched from the database on
    /// first use and stored back into `state` for reuse by subsequent calls.
    /// This avoids creating a new [`StateProvider`] for every transaction in a
    /// batch, which is the main source of txpool validation slowdown as the
    /// state trie grows.
    pub fn validate_one_with_state(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: &mut Option<Box<dyn reth_storage_api::AccountInfoReader + Send>>,
    ) -> TransactionValidationOutcome<Tx> {
        self.validate_one_with_context(origin, transaction, state, None)
    }

    fn validate_one_with_context(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: &mut Option<Box<dyn reth_storage_api::AccountInfoReader + Send>>,
        mut token_validator: Option<Result<&mut TokenFeeValidator<'_>, &str>>,
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

        // Reject EIP-7702 transactions before Viridian hardfork (PRAGUE)
        if transaction.is_eip7702()
            && !self
                .chain_spec()
                .is_viridian_active_at_timestamp(self.block_timestamp())
        {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidTransactionError::TxTypeNotSupported.into(),
            );
        }

        // Check if this is a MorphTx (0x7F) - need special handling for ERC20 gas payment
        let is_morph_tx = is_morph_tx(&transaction);

        // Reject MorphTx (0x7F) before Emerald hardfork.
        // go-ethereum's MakeSigner only registers MorphTxType from forks.Emerald onwards.
        if is_morph_tx
            && !self
                .chain_spec()
                .is_emerald_active_at_timestamp(self.block_timestamp())
        {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidTransactionError::TxTypeNotSupported.into(),
            );
        }

        // EIP-3860: enforce max initcode size on contract-creation transactions.
        //
        // Morph has been post-Shanghai since genesis (morph-geth's
        // `shanghaiBlock = 0` / `IsShanghai(num)`), so this check must always
        // fire. We can't reuse reth's `EthTransactionValidator` Shanghai gating
        // because morph-mainnet/hoodi genesis uses the non-standard
        // `shanghaiBlock` field (inherited from scroll-tech), which alloy's
        // `Genesis` parser ignores in favour of `shanghaiTime`. Force-activating
        // Shanghai/Cancun in the chainspec would also flip
        // `is_shanghai_active_at_timestamp` for `EthStorage::read_block_bodies`,
        // making body.withdrawals leak into RPC responses and diverging from
        // morph-geth, which has no withdrawals slot. Enforcing the bound here
        // keeps the chainspec untouched and the RPC output bit-identical to
        // morph-geth.
        //
        // Placed after the L1-message / EIP-7702 / MorphTx type gates so a
        // transaction rejected purely on its type still surfaces that type error
        // (matching morph-geth), and before the inner validator so we skip its
        // state lookups for oversized payloads.
        if let Err(err) = transaction.ensure_max_init_code_size(MAX_INITCODE_SIZE) {
            return TransactionValidationOutcome::Invalid(transaction, err);
        }

        let outcome = self
            .inner
            .validate_one_with_state(origin, transaction, state);
        if outcome.is_invalid() || outcome.is_error() {
            tracing::trace!(target: "morph::txpool", ?outcome, "tx pool validation failed");
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
            let l1_block_info = *self.block_info.l1_block_info.read();
            let hardfork = self
                .chain_spec()
                .morph_hardfork_at(self.block_number(), self.block_timestamp());

            // Calculate L1 data fee (always calculated for all transactions).
            // Clone consensus tx once — reused for both L1 fee encoding and MorphTx validation.
            let consensus_tx = valid_tx.transaction().clone_into_consensus();
            let mut encoded = Vec::with_capacity(consensus_tx.encode_2718_len());
            consensus_tx.encode_2718(&mut encoded);
            let l1_data_fee = l1_block_info.calculate_tx_l1_cost(&encoded, hardfork);

            if is_morph_tx {
                // MorphTx: validate structural rules and ERC20 token balance via
                // the shared helper used by both admission and maintenance.
                // Pass &MorphTxEnvelope directly to avoid a second clone_into_consensus().
                let sender = valid_tx.transaction().sender();
                let validation_res = match token_validator.take() {
                    Some(Ok(validator)) => self.validate_morph_tx_balance_with_validator(
                        &consensus_tx,
                        sender,
                        balance,
                        l1_data_fee,
                        hardfork,
                        Ok(validator),
                    ),
                    Some(Err(err_msg)) => self.validate_morph_tx_balance_with_validator(
                        &consensus_tx,
                        sender,
                        balance,
                        l1_data_fee,
                        hardfork,
                        Err(err_msg),
                    ),
                    None => self.validate_morph_tx_balance(
                        &consensus_tx,
                        sender,
                        balance,
                        l1_data_fee,
                        hardfork,
                    ),
                };
                if let Err(err) = validation_res {
                    return TransactionValidationOutcome::Invalid(
                        valid_tx.into_transaction(),
                        err.into(),
                    );
                }
            } else {
                // Regular transaction: validate ETH balance covers cost + L1 fee
                let cost = valid_tx.transaction().cost().saturating_add(l1_data_fee);
                if cost > balance {
                    return insufficient_funds_outcome(valid_tx.into_transaction(), balance, cost);
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
    /// Accepts `&Recovered<MorphTxEnvelope>` directly (already cloned by the caller)
    /// to avoid a redundant second `clone_into_consensus()`.
    ///
    /// This method performs the following checks (reference: go-ethereum tx_pool.go:727-791):
    /// 1. `fee_token_id == 0`: ETH-fee path, require ETH affordability for `cost + l1_fee`
    /// 2. `fee_token_id > 0`: token must be registered and active in L2TokenRegistry
    /// 3. Token price ratio must be valid (non-zero)
    /// 4. Effective token limit must cover required token amount
    /// 5. ETH balance must be >= transaction value (value is still in ETH)
    fn validate_morph_tx_balance(
        &self,
        consensus_tx: &reth_primitives_traits::Recovered<MorphTxEnvelope>,
        sender: Address,
        eth_balance: U256,
        l1_data_fee: U256,
        hardfork: MorphHardfork,
    ) -> Result<crate::MorphTxValidationResult, MorphTxError> {
        let (provider, cache) = match self.latest_state_provider_and_cache() {
            Ok(pair) => pair,
            Err(err) => {
                let err_msg = err.to_string();
                return self.validate_morph_tx_balance_with_validator(
                    consensus_tx,
                    sender,
                    eth_balance,
                    l1_data_fee,
                    hardfork,
                    Err(&err_msg),
                );
            }
        };
        let evm_state_provider = (&provider).into_evm_state_provider();
        let env = self.block_info.evm_env(hardfork, self.inner.chain_id());
        let mut validator =
            TokenFeeValidator::new(&cache, StateProviderDatabase::new(&evm_state_provider), env);
        self.validate_morph_tx_balance_with_validator(
            consensus_tx,
            sender,
            eth_balance,
            l1_data_fee,
            hardfork,
            Ok(&mut validator),
        )
    }

    fn validate_morph_tx_balance_with_validator(
        &self,
        consensus_tx: &reth_primitives_traits::Recovered<MorphTxEnvelope>,
        sender: Address,
        eth_balance: U256,
        l1_data_fee: U256,
        hardfork: MorphHardfork,
        token_validator: Result<&mut TokenFeeValidator<'_>, &str>,
    ) -> Result<crate::MorphTxValidationResult, MorphTxError> {
        let input = crate::MorphTxValidationInput {
            consensus_tx,
            sender,
            eth_balance,
            l1_data_fee,
            hardfork,
        };

        let result = validate_morph_tx_with(&input, |token_id, caller| match token_validator {
            Ok(validator) => validator.resolve_token_fee_info(token_id, caller),
            Err(message) => Err(MorphTxError::TokenInfoFetchFailed {
                token_id,
                message: message.to_string(),
            }),
        })?;
        let token_balance = result
            .token_info
            .as_ref()
            .map(|info| info.balance)
            .unwrap_or_default();

        tracing::trace!(
            target: "morph::txpool",
            fee_token_id = ?consensus_tx.fee_token_id(),
            fee_limit = ?consensus_tx.fee_limit(),
            uses_token_fee = result.uses_token_fee,
            required_token_amount = ?result.required_token_amount,
            token_balance = ?token_balance,
            l1_data_fee = ?l1_data_fee,
            eth_balance = ?eth_balance,
            tx_value = ?consensus_tx.value(),
            "MorphTx validation passed"
        );

        Ok(result)
    }

    /// Validates all given transactions, reusing a single state provider and `MorphEvm`
    /// across the batch while sharing [`StateCache`] reads across concurrent validations at the
    /// same canonical tip.
    ///
    /// Returns all outcomes for the given transactions in the same order.
    ///
    /// See also [`Self::validate_one`]
    pub fn validate_all(
        &self,
        transactions: Vec<(TransactionOrigin, Tx)>,
    ) -> Vec<TransactionValidationOutcome<Tx>> {
        let mut state = None;
        if !transactions.iter().any(|(_, tx)| is_morph_tx(tx)) {
            return transactions
                .into_iter()
                .map(|(origin, tx)| self.validate_one_with_context(origin, tx, &mut state, None))
                .collect();
        }

        match self.latest_state_provider_and_cache() {
            Ok((provider, cache)) => {
                let evm_state_provider = (&provider).into_evm_state_provider();
                let hardfork = self
                    .chain_spec()
                    .morph_hardfork_at(self.block_number(), self.block_timestamp());
                let env = self.block_info.evm_env(hardfork, self.inner.chain_id());
                let mut token_validator = TokenFeeValidator::new(
                    &cache,
                    StateProviderDatabase::new(&evm_state_provider),
                    env,
                );
                transactions
                    .into_iter()
                    .map(|(origin, tx)| {
                        self.validate_one_with_context(
                            origin,
                            tx,
                            &mut state,
                            Some(Ok(&mut token_validator)),
                        )
                    })
                    .collect()
            }
            Err(err) => {
                let err_msg = err.to_string();
                transactions
                    .into_iter()
                    .map(|(origin, tx)| {
                        self.validate_one_with_context(
                            origin,
                            tx,
                            &mut state,
                            Some(Err(err_msg.as_str())),
                        )
                    })
                    .collect()
            }
        }
    }
}

impl<Client, Tx, Evm> TransactionValidator for MorphTransactionValidator<Client, Tx, Evm>
where
    Client: ChainSpecProvider<ChainSpec: MorphHardforks> + StateProviderFactory + BlockReaderIdExt,
    Tx: EthPoolTransaction<Consensus = MorphTxEnvelope>,
    Evm: ConfigureEvm,
{
    type Transaction = Tx;
    type Block = BlockTy<Evm::Primitives>;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        self.validate_one(origin, transaction)
    }

    async fn validate_transactions(
        &self,
        transactions: impl IntoIterator<Item = (TransactionOrigin, Self::Transaction), IntoIter: Send>
        + Send,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        self.validate_all(transactions.into_iter().collect())
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);
        *self.cached_state.write() = (new_tip_block.hash(), Arc::new(StateCache::default()));
        self.update_l1_block_info_with_hash(new_tip_block.header(), Some(new_tip_block.hash()));
    }
}

/// Helper function to check if a transaction is an L1 message.
fn is_l1_message(tx: &impl Typed2718) -> bool {
    tx.ty() == morph_primitives::L1_TX_TYPE_ID
}

/// Helper function to check if a transaction is a MorphTx (0x7F).
fn is_morph_tx(tx: &impl Typed2718) -> bool {
    tx.ty() == morph_primitives::MORPH_TX_TYPE_ID
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxEip1559, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{B256, Signature, TxKind, address};
    use morph_chainspec::{MORPH_MAINNET, MorphChainSpec};
    use morph_evm::MorphEvmConfig;
    use morph_primitives::{MorphPrimitives, TxL1Msg, TxMorph};
    use morph_revm::{
        L2_TOKEN_REGISTRY_ADDRESS, compute_mapping_slot, compute_mapping_slot_for_address,
    };
    use reth_primitives_traits::Recovered;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::{
        blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
    };

    fn new_mock_provider() -> MockEthProvider<MorphPrimitives, MorphChainSpec> {
        MockEthProvider::<MorphPrimitives, _>::new()
            .with_chain_spec((**MORPH_MAINNET).clone())
            .with_genesis_block()
    }

    fn storage_key(slot: U256) -> B256 {
        B256::from(slot.to_be_bytes::<32>())
    }

    fn token_id_key(token_id: u16) -> [u8; 32] {
        let mut key = [0u8; 32];
        key[30..32].copy_from_slice(&token_id.to_be_bytes());
        key
    }

    fn token_registry_account(
        token_id: u16,
        token_address: alloy_primitives::Address,
        balance_slot: U256,
        token_balance: U256,
    ) -> ExtendedAccount {
        let token_registry_slot = U256::from(151);
        let price_ratio_slot = U256::from(153);
        let token_key = token_id_key(token_id);
        let base = compute_mapping_slot(token_registry_slot, &token_key);

        let mut slot_2 = [0u8; 32];
        slot_2[30] = 18;
        slot_2[31] = 1;

        ExtendedAccount::new(0, U256::ZERO).extend_storage([
            (
                storage_key(base),
                U256::from_be_bytes(token_address.into_word().0),
            ),
            (
                storage_key(base + U256::from(1)),
                balance_slot + U256::from(1),
            ),
            (
                storage_key(base + U256::from(2)),
                U256::from_be_bytes(slot_2),
            ),
            (storage_key(base + U256::from(3)), U256::from(1)),
            (
                storage_key(compute_mapping_slot(price_ratio_slot, &token_key)),
                U256::from(1),
            ),
            (
                storage_key(compute_mapping_slot_for_address(
                    balance_slot,
                    address!("0000000000000000000000000000000000000001"),
                )),
                token_balance,
            ),
        ])
    }

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
        info.update(l1_info, 1234, 100, Some(42));

        assert_eq!(info.timestamp(), 1234);
        assert_eq!(info.number(), 100);
        assert_eq!(info.base_fee_per_gas(), Some(42));
    }

    #[test]
    fn validate_l1_message_rejected() {
        // Create validator with mock provider
        let client = new_mock_provider();
        let morph_evm_config = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone());
        let eth_validator: EthTransactionValidator<
            _,
            crate::MorphPooledTransaction,
            MorphEvmConfig,
        > = EthTransactionValidatorBuilder::new(client, morph_evm_config)
            .no_shanghai()
            .no_cancun()
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default());
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
        let client = new_mock_provider();
        let signer = address!("0000000000000000000000000000000000000001");
        client.add_account(signer, ExtendedAccount::new(0, U256::from(10u128.pow(18))));
        let morph_evm_config = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone());
        let eth_validator: EthTransactionValidator<
            _,
            crate::MorphPooledTransaction,
            MorphEvmConfig,
        > = EthTransactionValidatorBuilder::new(client, morph_evm_config)
            .no_shanghai()
            .no_cancun()
            .disable_balance_check()
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default());
        let validator = MorphTransactionValidator::new(eth_validator);

        let origin = TransactionOrigin::External;

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
        let client = new_mock_provider();
        let signer = address!("0000000000000000000000000000000000000001");
        client.add_account(signer, ExtendedAccount::new(0, U256::from(10u128.pow(18))));
        let morph_evm_config = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone());
        let eth_validator: EthTransactionValidator<
            _,
            crate::MorphPooledTransaction,
            MorphEvmConfig,
        > = EthTransactionValidatorBuilder::new(client, morph_evm_config)
            .no_shanghai()
            .no_cancun()
            .disable_balance_check()
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default());
        let validator = MorphTransactionValidator::new(eth_validator);

        let origin = TransactionOrigin::External;

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

    /// EIP-3860 mempool admission: contract creation transactions whose initcode
    /// exceeds 49 152 bytes must be rejected. This used to slip through because
    /// morph-mainnet/hoodi genesis use the non-standard `shanghaiBlock` field
    /// (inherited from scroll-tech go-ethereum) that alloy's `Genesis` parser
    /// ignores, leaving Shanghai un-registered in the hardforks table and
    /// `is_shanghai_active_at_timestamp` permanently `false` — which means reth's
    /// Shanghai-gated EIP-3860 check (`EthTransactionValidator`, `eth.rs:468`) is
    /// always skipped. The rejection is therefore enforced unconditionally by
    /// `MorphTransactionValidator` itself (see `validate_one_with_state`), not by
    /// the chainspec or the inner reth validator.
    #[test]
    fn validate_rejects_oversized_initcode() {
        let client = new_mock_provider();
        let signer = address!("0000000000000000000000000000000000000001");
        client.add_account(signer, ExtendedAccount::new(0, U256::from(10u128.pow(18))));
        let morph_evm_config = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone());

        // Built from the real `MORPH_MAINNET` chainspec, under which Shanghai is
        // never active, so the inner `EthTransactionValidator` does not enforce
        // EIP-3860. This test verifies that `MorphTransactionValidator` rejects
        // the oversized initcode on its own.
        let eth_validator: EthTransactionValidator<
            _,
            crate::MorphPooledTransaction,
            MorphEvmConfig,
        > = EthTransactionValidatorBuilder::new(client, morph_evm_config)
            .disable_balance_check()
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default());
        let validator = MorphTransactionValidator::new(eth_validator);

        let oversize_initcode = vec![0u8; MAX_INITCODE_SIZE + 1];
        let tx = TxLegacy {
            chain_id: Some(2818),
            nonce: 0,
            gas_limit: 30_000_000,
            to: TxKind::Create,
            value: U256::ZERO,
            input: oversize_initcode.into(),
            gas_price: 2_000_000_000,
        };
        let signed_tx = Signed::new_unchecked(tx, Signature::test_signature(), B256::ZERO);
        let envelope = MorphTxEnvelope::Legacy(signed_tx);
        let recovered = Recovered::new_unchecked(envelope, signer);
        let len = recovered.encode_2718_len();
        let pooled_tx = crate::MorphPooledTransaction::new(recovered, len);

        let outcome = validator.validate_one(TransactionOrigin::External, pooled_tx);

        match outcome {
            TransactionValidationOutcome::Invalid(_, err) => {
                let msg = err.to_string();
                assert!(
                    msg.contains("max_init_code_size") || msg.contains("init code size"),
                    "expected EIP-3860 rejection, got: {msg}"
                );
            }
            other => panic!("expected oversized initcode to be rejected, got: {other:?}"),
        }
    }

    /// Counterpart to `validate_rejects_oversized_initcode`: an initcode
    /// exactly at the EIP-3860 limit (49 152 bytes) must still be admitted.
    #[test]
    fn validate_accepts_initcode_at_limit() {
        let client = new_mock_provider();
        let signer = address!("0000000000000000000000000000000000000001");
        client.add_account(signer, ExtendedAccount::new(0, U256::from(10u128.pow(18))));
        let morph_evm_config = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone());
        let eth_validator: EthTransactionValidator<
            _,
            crate::MorphPooledTransaction,
            MorphEvmConfig,
        > = EthTransactionValidatorBuilder::new(client, morph_evm_config)
            .disable_balance_check()
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default());
        let validator = MorphTransactionValidator::new(eth_validator);

        let initcode_at_limit = vec![0u8; MAX_INITCODE_SIZE];
        let tx = TxLegacy {
            chain_id: Some(2818),
            nonce: 0,
            gas_limit: 30_000_000,
            to: TxKind::Create,
            value: U256::ZERO,
            input: initcode_at_limit.into(),
            gas_price: 2_000_000_000,
        };
        let signed_tx = Signed::new_unchecked(tx, Signature::test_signature(), B256::ZERO);
        let envelope = MorphTxEnvelope::Legacy(signed_tx);
        let recovered = Recovered::new_unchecked(envelope, signer);
        let len = recovered.encode_2718_len();
        let pooled_tx = crate::MorphPooledTransaction::new(recovered, len);

        let outcome = validator.validate_one(TransactionOrigin::External, pooled_tx);
        // The validator may still reject this because of intrinsic gas or
        // balance checks downstream; the only thing this test asserts is that
        // EIP-3860 itself does NOT fire at exactly the limit.
        if let TransactionValidationOutcome::Invalid(_, err) = &outcome {
            let msg = err.to_string();
            assert!(
                !msg.contains("max_init_code_size") && !msg.contains("init code size"),
                "EIP-3860 must not reject initcode of exactly the size limit; got: {msg}"
            );
        }
    }

    #[test]
    fn validate_morph_tx_uses_max_fee_for_token_fee_admission() {
        let client = new_mock_provider();
        let signer = address!("0000000000000000000000000000000000000001");
        let token = address!("5300000000000000000000000000000000000042");
        let balance_slot = U256::from(7);

        client.add_account(signer, ExtendedAccount::new(0, U256::ZERO));
        client.add_account(
            L2_TOKEN_REGISTRY_ADDRESS,
            token_registry_account(1, token, balance_slot, U256::from(300_000u64)),
        );
        client.add_account(
            token,
            ExtendedAccount::new(0, U256::ZERO).extend_storage([(
                storage_key(compute_mapping_slot_for_address(balance_slot, signer)),
                U256::from(300_000u64),
            )]),
        );

        let morph_evm_config = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone());
        let eth_validator: EthTransactionValidator<
            _,
            crate::MorphPooledTransaction,
            MorphEvmConfig,
        > = EthTransactionValidatorBuilder::new(client, morph_evm_config)
            .no_shanghai()
            .no_cancun()
            .disable_balance_check()
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default());
        let validator = MorphTransactionValidator::new(eth_validator);
        // Simulate an active chain head with base_fee_per_gas = 10. Execution
        // would use effective gas price = min(100, 10 + 1) = 11, but txpool
        // admission follows geth's conservative max_fee_per_gas budget.
        validator
            .block_info
            .update(L1BlockInfo::default(), 0, 0, Some(10));

        let tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::from(300_000u64),
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            input: Default::default(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));
        let recovered = Recovered::new_unchecked(envelope, signer);
        let err = validator
            .validate_morph_tx_balance(
                &recovered,
                signer,
                U256::ZERO,
                U256::ZERO,
                morph_chainspec::hardfork::MorphHardfork::Viridian,
            )
            .expect_err("MorphTx should require the max-fee token budget in txpool admission");

        assert!(matches!(
            err,
            crate::MorphTxError::InsufficientTokenBalance {
                required,
                balance,
                ..
            } if required == U256::from(2_100_000u64) && balance == U256::from(300_000u64)
        ));
    }

    #[test]
    fn validate_morph_tx_call_mode_uses_real_head_chain_id_and_timestamp() {
        let client = new_mock_provider();
        let signer = address!("0000000000000000000000000000000000000001");
        let token = address!("5300000000000000000000000000000000000042");
        let token_key = token_id_key(1);
        let base = compute_mapping_slot(U256::from(151), &token_key);
        let mut slot_2 = [0u8; 32];
        slot_2[30] = 18;
        slot_2[31] = 1;

        client.add_account(signer, ExtendedAccount::new(0, U256::ZERO));
        client.add_account(
            L2_TOKEN_REGISTRY_ADDRESS,
            ExtendedAccount::new(0, U256::ZERO).extend_storage([
                (storage_key(base), U256::from_be_bytes(token.into_word().0)),
                // `0` -> call-mode (`balanceOf`)
                (storage_key(base + U256::from(1)), U256::ZERO),
                (
                    storage_key(base + U256::from(2)),
                    U256::from_be_bytes(slot_2),
                ),
                (storage_key(base + U256::from(3)), U256::ONE),
                (
                    storage_key(compute_mapping_slot(U256::from(153), &token_key)),
                    U256::ONE,
                ),
            ]),
        );
        // `balanceOf` returns `CHAINID + TIMESTAMP`:
        // CHAINID (0x46), TIMESTAMP (0x42), ADD (0x01), PUSH0 (0x5f), MSTORE (0x52),
        // PUSH1 0x20, PUSH0, RETURN (0xf3)
        client.add_account(
            token,
            ExtendedAccount::new(0, U256::ZERO)
                .with_bytecode(alloy_primitives::bytes!("4642015f5260205ff3")),
        );

        let morph_evm_config = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone());
        let eth_validator: EthTransactionValidator<
            _,
            crate::MorphPooledTransaction,
            MorphEvmConfig,
        > = EthTransactionValidatorBuilder::new(client, morph_evm_config)
            .no_shanghai()
            .no_cancun()
            .disable_balance_check()
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default());
        let validator = MorphTransactionValidator::new(eth_validator);
        const HEAD_TIMESTAMP: u64 = 1_767_765_600;
        validator
            .block_info
            .update(L1BlockInfo::default(), HEAD_TIMESTAMP, 100, Some(1));

        let tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::ZERO,
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            input: Default::default(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));
        let recovered = Recovered::new_unchecked(envelope, signer);
        let res = validator
            .validate_morph_tx_balance(
                &recovered,
                signer,
                U256::ZERO,
                U256::ZERO,
                MorphHardfork::Emerald,
            )
            .expect("call-mode MorphTx must succeed under real head env");

        assert_eq!(
            res.token_info.unwrap().balance,
            U256::from(2818 + HEAD_TIMESTAMP)
        );
    }
}
