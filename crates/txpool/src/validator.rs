//! Transaction validator for Morph L2.
//!
//! This module provides Morph-specific transaction validation that extends the standard
//! Ethereum transaction validation with L2 checks:
//! - Rejection of EIP-4844 blob transactions
//! - EIP-3860 max initcode size enforcement
//! - Rejection of L1 message transactions from the pool
//! - Rejection of EIP-7702 transactions before Viridian and MorphTx before Emerald
//! - L1 data fee validation
//! - MorphTx (0x7F) ERC20 token balance validation

use crate::{
    MorphTxError, MorphTxValidationError,
    morph_tx_validation::{validate_morph_tx_stateless, validate_morph_tx_with},
    state_cache::{StateCache, StateCacheDb, TokenFeeValidator},
};
use alloy_consensus::{BlockHeader, Sealable, Transaction};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_primitives::{Address, B256, U256};
use morph_chainspec::hardfork::MorphHardforks;
use morph_primitives::MorphTxEnvelope;
use morph_revm::{L1BlockInfo, MorphEvmEnv};
use parking_lot::RwLock;
use reth_chainspec::ChainSpecProvider;
use reth_evm::{ConfigureEvm, EvmEnvFor, database::StateProviderDatabase};
use reth_primitives_traits::{
    BlockTy, GotExpected, HeaderTy, SealedBlock, SealedHeader,
    transaction::error::InvalidTransactionError,
};
use reth_storage_api::{BlockReaderIdExt, StateProvider as _, StateProviderBox, StateProviderFactory};
use reth_transaction_pool::{
    EthPoolTransaction, EthTransactionValidator, PoolTransaction, TransactionOrigin,
    TransactionValidationOutcome, TransactionValidator, error::InvalidPoolTransactionError,
};
use std::sync::Arc;

/// EIP-3860 max initcode size (`2 * MAX_CODE_SIZE = 2 * 24 576 = 49 152` bytes).
///
/// Reuses evm2's canonical constant, the base of the limit reth's
/// `EthTransactionValidator` applies, so it stays pinned to the EIP-170
/// code-size base instead of a hand-copied literal. Enforced unconditionally at
/// the txpool layer because Morph has been post-Shanghai since genesis; see
/// `validate_one_with_state` for why we can't rely on reth's Shanghai-gated check.
const MAX_INITCODE_SIZE: usize = evm2::constants::MAX_INITCODE_SIZE;

/// A complete set of fee-validation inputs and tip-scoped state cache for one block.
#[derive(Debug)]
pub(crate) struct MorphValidationHead {
    pub(crate) hash: B256,
    pub(crate) number: u64,
    pub(crate) timestamp: u64,
    pub(crate) base_fee_per_gas: Option<u64>,
    pub(crate) l1_block_info: L1BlockInfo,
    pub(crate) evm_env: MorphEvmEnv,
    pub(crate) state_cache: Arc<StateCache>,
}

/// Tracks L1 fee parameters, the matching block environment, and the tip-scoped state cache.
///
/// A complete head is published atomically. Readers retain an immutable snapshot while
/// later canonical updates prepare and publish a replacement.
#[derive(Debug, Default)]
pub struct MorphL1BlockInfo {
    head: RwLock<Option<Arc<MorphValidationHead>>>,
}

impl MorphL1BlockInfo {
    /// Creates an uninitialized tracker; the validator publishes each canonical head into it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current L1 block info, or its default before initialization.
    pub fn l1_block_info(&self) -> L1BlockInfo {
        self.head
            .read()
            .as_ref()
            .map(|head| head.l1_block_info)
            .unwrap_or_default()
    }

    /// Publishes fee parameters and the environment for the supplied header together.
    ///
    /// `info` must be read from this header's post-state and `evm_env` must be built
    /// for the same header. Partial updates are not supported.
    pub fn update<H: BlockHeader + Sealable>(
        &self,
        info: L1BlockInfo,
        header: &H,
        evm_env: MorphEvmEnv,
    ) {
        *self.head.write() = Some(Arc::new(MorphValidationHead {
            hash: header.hash_slow(),
            number: header.number(),
            timestamp: header.timestamp(),
            base_fee_per_gas: header.base_fee_per_gas(),
            l1_block_info: info,
            evm_env,
            state_cache: Arc::new(StateCache::default()),
        }));
    }

    /// Returns the current block timestamp, or zero before initialization.
    pub fn timestamp(&self) -> u64 {
        self.head
            .read()
            .as_ref()
            .map(|head| head.timestamp)
            .unwrap_or_default()
    }

    /// Returns the current block number, or zero before initialization.
    pub fn number(&self) -> u64 {
        self.head
            .read()
            .as_ref()
            .map(|head| head.number)
            .unwrap_or_default()
    }

    /// Returns the current block base fee per gas.
    pub fn base_fee_per_gas(&self) -> Option<u64> {
        self.head
            .read()
            .as_ref()
            .and_then(|head| head.base_fee_per_gas)
    }
}

/// State and fee-validation inputs pinned to one block for a transaction batch.
///
/// Created lazily by [`MorphTransactionValidator::validate_one_with_state`]; pass the same
/// `&mut Option<MorphValidationState>` across calls in a batch to reuse the state provider,
/// L1 fee parameters, and tip-scoped [`StateCache`].
pub struct MorphValidationState {
    pub(crate) head: Arc<MorphValidationHead>,
    pub(crate) provider: StateProviderBox,
}

/// Opens the state, L1 fee parameters, EVM environment, and a fresh tip-scoped [`StateCache`]
/// for `header`.
pub(crate) fn validation_state_for_header<Client, Evm>(
    client: &Client,
    evm_config: &Evm,
    header: &HeaderTy<Evm::Primitives>,
) -> Result<MorphValidationState, Box<dyn std::error::Error + Send + Sync>>
where
    Client: StateProviderFactory,
    Evm: ConfigureEvm,
    EvmEnvFor<Evm>: Into<MorphEvmEnv>,
{
    let hash = header.hash_slow();
    let provider = client.state_by_block_hash(hash)?;
    let evm_env: MorphEvmEnv = evm_config.evm_env(header)?.into();
    let state_cache = Arc::new(StateCache::default());
    let hardfork = evm_env.spec;
    let l1_block_info = {
        let evm_state_provider = (&provider).into_evm_state_provider();
        let mut db = StateCacheDb::new(
            &state_cache,
            StateProviderDatabase::new(&evm_state_provider),
        );
        L1BlockInfo::try_fetch(&mut db, hardfork)?
    };

    Ok(MorphValidationState {
        head: Arc::new(MorphValidationHead {
            hash,
            number: header.number(),
            timestamp: header.timestamp(),
            base_fee_per_gas: header.base_fee_per_gas(),
            l1_block_info,
            evm_env,
            state_cache,
        }),
        provider,
    })
}

/// Validator for Morph L2 transactions.
///
/// This validator extends [`EthTransactionValidator`] with Morph-specific checks:
/// - Rejects EIP-4844 blob transactions (not supported on L2)
/// - Rejects L1 message transactions (only included by sequencer)
/// - Rejects EIP-7702 transactions before Viridian and MorphTx before Emerald
/// - Enforces EIP-3860 max initcode size regardless of Shanghai activation
/// - Validates L1 data fee affordability
/// - Validates MorphTx (0x7F) ERC20 token balance and fee_limit
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
    Client: ChainSpecProvider<ChainSpec: MorphHardforks>
        + StateProviderFactory
        + BlockReaderIdExt<Header = HeaderTy<Evm::Primitives>>,
    Tx: EthPoolTransaction<Consensus = MorphTxEnvelope>,
    Evm: ConfigureEvm,
    // Pins the cached environment to Morph's, so call-mode `balanceOf` runs in
    // exactly what the execution layer would use.
    EvmEnvFor<Evm>: Into<MorphEvmEnv>,
{
    /// Create a new [`MorphTransactionValidator`].
    pub fn new(inner: EthTransactionValidator<Client, Tx, Evm>) -> Self {
        let this = Self::with_block_info(inner, MorphL1BlockInfo::default());
        if let Ok(Some(header)) = this.inner.client().latest_header() {
            this.sync_inner_to_header(header.clone());
            this.update_l1_block_info(header.header());
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
        }
    }

    /// Synchronizes the inner [`EthTransactionValidator`]'s `block_gas_limit`,
    /// `fork_tracker`, and validation gas rules with `header`.
    ///
    /// Upstream `EthTransactionValidator::on_new_head_block` only inspects the
    /// block header, so wrapping the [`SealedHeader`] with an empty default body
    /// refreshes inner limits and fork flags without loading a full block body.
    fn sync_inner_to_header(&self, header: SealedHeader<HeaderTy<Evm::Primitives>>) {
        let block =
            SealedBlock::<BlockTy<Evm::Primitives>>::from_sealed_parts(header, Default::default());
        self.inner.on_new_head_block(&block);
    }

    /// Update the L1 block info for the given header.
    pub fn update_l1_block_info(&self, header: &HeaderTy<Evm::Primitives>) {
        match validation_state_for_header(self.client(), self.inner.evm_config(), header) {
            Ok(state) => *self.block_info.head.write() = Some(state.head),
            Err(err) => {
                // Clear any stale/orphaned head so `current_validation_state` falls back
                // to `latest_header()` instead of staying pinned to an unreadable block.
                *self.block_info.head.write() = None;
                tracing::warn!(target: "morph::txpool", %err, "Failed to refresh fee-validation head");
            }
        }
    }

    /// Opens the state of the head last published through `on_new_head_block`.
    ///
    /// If no head has been published yet, if the canonical tip has moved ahead
    /// (for example after a `depth > max_update_depth` catch-up where upstream
    /// maintenance skips `on_new_head_block`), or if the cached head's state is
    /// no longer available (pruned or orphaned), falls back to the provider's
    /// latest canonical header and refreshes the cached head and inner validator
    /// without downgrading a newer concurrent `on_new_head_block`.
    fn current_validation_state(
        &self,
    ) -> Result<MorphValidationState, Box<dyn std::error::Error + Send + Sync>> {
        let cached_head = self.block_info.head.read().clone();
        let canonical_tip = self.client().chain_info().ok();

        if let Some(ref head) = cached_head {
            let matches_tip = canonical_tip
                .as_ref()
                .is_none_or(|tip| tip.best_hash.is_zero() || tip.best_hash == head.hash);
            if matches_tip {
                match self.client().state_by_block_hash(head.hash) {
                    Ok(provider) => {
                        return Ok(MorphValidationState {
                            head: Arc::clone(head),
                            provider,
                        });
                    }
                    Err(err) => {
                        tracing::debug!(
                            target: "morph::txpool",
                            cached_hash = %head.hash,
                            cached_number = head.number,
                            %err,
                            "Cached validation head state unavailable; refreshing from latest header"
                        );
                    }
                }
            }
        }

        let latest = self
            .client()
            .latest_header()?
            .ok_or_else(|| std::io::Error::other("latest validation header is unavailable"))?;
        let state =
            validation_state_for_header(self.client(), self.inner.evm_config(), latest.header())?;

        // Publish the refreshed head and sync `self.inner` together under the write lock
        // only when no newer `on_new_head_block` raced ahead.
        let mut guard = self.block_info.head.write();
        let can_replace = match (guard.as_ref(), cached_head.as_ref()) {
            (None, _) => true,
            (Some(current), Some(seen)) if current.hash == seen.hash => true,
            (Some(current), _) if current.number < state.head.number => true,
            _ => false,
        };
        if can_replace {
            self.sync_inner_to_header(latest);
            *guard = Some(Arc::clone(&state.head));
        }

        Ok(state)
    }

    /// Validates a single transaction.
    ///
    /// See also [`TransactionValidator::validate_transaction`]
    ///
    /// This behaves the same as [`EthTransactionValidator::validate_one`], but in addition:
    /// - Rejects EIP-4844 blob transactions
    /// - Rejects L1 message transactions
    /// - Rejects EIP-7702 transactions before Viridian and MorphTx before Emerald
    /// - Enforces the EIP-3860 initcode size limit regardless of Shanghai activation
    /// - Validates MorphTx (0x7F) ERC20 token balance and fee_limit
    /// - Ensures that the account has enough balance to cover the L1 gas cost
    pub fn validate_one(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
    ) -> TransactionValidationOutcome<Tx> {
        self.validate_one_with_state(origin, transaction, &mut None)
    }

    /// Validates a single transaction, reusing a state and head snapshot.
    ///
    /// When `state` is `None`, validation opens the state of the head last published by
    /// `on_new_head_block` (refreshing automatically if the canonical head has moved). Both
    /// the provider and the tip-scoped [`StateCache`] are reused for subsequent transactions
    /// in the batch. Reset `state` to `None` to pick up a newer head.
    pub fn validate_one_with_state(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: &mut Option<MorphValidationState>,
    ) -> TransactionValidationOutcome<Tx> {
        if let Some(err) = reject_unsupported_tx_type(&transaction) {
            return TransactionValidationOutcome::Invalid(transaction, err);
        }
        if state.is_none() {
            match self.current_validation_state() {
                Ok(snapshot) => *state = Some(snapshot),
                Err(err) => return TransactionValidationOutcome::Error(*transaction.hash(), err),
            }
        }
        let snapshot = state.as_ref().expect("validation state initialized above");
        self.validate_one_with_snapshot(origin, transaction, snapshot, None)
    }

    fn validate_one_with_snapshot(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: &MorphValidationState,
        mut token_validator: Option<&mut TokenFeeValidator<'_>>,
    ) -> TransactionValidationOutcome<Tx> {
        if let Some(err) = reject_unsupported_tx_type(&transaction) {
            return TransactionValidationOutcome::Invalid(transaction, err);
        }
        let head = &state.head;

        // Reject EIP-7702 transactions before Viridian hardfork (PRAGUE).
        // Use `ForkNotActive` (`is_bad_transaction() == false`) so a peer one block ahead
        // across the fork boundary is not penalized.
        if transaction.is_eip7702()
            && !self
                .chain_spec()
                .is_viridian_active_at_timestamp(head.timestamp)
        {
            return TransactionValidationOutcome::Invalid(
                transaction,
                MorphTxError::ForkNotActive {
                    reason: "EIP-7702 transactions are not yet active (viridian fork not reached)"
                        .to_string(),
                }
                .into(),
            );
        }

        // Check if this is a MorphTx (0x7F) - need special handling for ERC20 gas payment
        let is_morph_tx = is_morph_tx(&transaction);

        // Reject MorphTx (0x7F) before Emerald hardfork.
        // go-ethereum's MakeSigner only registers MorphTxType from forks.Emerald onwards.
        if is_morph_tx
            && !self
                .chain_spec()
                .is_emerald_active_at_timestamp(head.timestamp)
        {
            return TransactionValidationOutcome::Invalid(
                transaction,
                MorphTxError::ForkNotActive {
                    reason: "MorphTx type is not yet active (emerald fork not reached)".to_string(),
                }
                .into(),
            );
        }

        let hardfork = head.evm_env.spec;

        // Run stateless MorphTx fork gates (Jade V1 / Celadon V2) and structural checks
        // (`TxMorph::validate`) BEFORE `inner.validate_stateful` so malformed or pre-fork
        // MorphTx payloads are rejected without paying for DB / account / authority reads.
        if is_morph_tx {
            let stateless_res = match transaction.consensus_ref().into_inner() {
                MorphTxEnvelope::Morph(signed) => validate_morph_tx_stateless(signed.tx(), hardfork),
                _ => Err(MorphTxError::InvalidTokenId),
            };
            if let Err(err) = stateless_res {
                return TransactionValidationOutcome::Invalid(transaction, err.into());
            }
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

        // Token-fee MorphTx reports only its ETH value through cost(), so reth's
        // cost() - value() fee-cap check sees zero. Preserve the configured local
        // fee cap using the gas budget, independently of the pool's ETH budget.
        if is_morph_tx
            && self
                .inner
                .local_transactions_config()
                .is_local(origin, transaction.sender_ref())
            && let Some(tx_fee_cap_wei) = self.inner.tx_fee_cap().filter(|cap| *cap != 0)
        {
            let max_tx_fee_wei = U256::from(transaction.gas_limit())
                .saturating_mul(U256::from(transaction.max_fee_per_gas()));
            if max_tx_fee_wei > U256::from(tx_fee_cap_wei) {
                return TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidPoolTransactionError::ExceedsFeeCap {
                        max_tx_fee_wei: max_tx_fee_wei.saturating_to(),
                        tx_fee_cap_wei,
                    },
                );
            }
        }

        if let Err(err) = self.inner.validate_stateless(origin, &transaction) {
            return TransactionValidationOutcome::Invalid(transaction, err);
        }
        let outcome = self
            .inner
            .validate_stateful(origin, transaction, &state.provider);
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
            let l1_block_info = head.l1_block_info;

            // Calculate L1 data fee (always calculated for all transactions).
            // Clone consensus tx once — reused for both L1 fee encoding and MorphTx validation.
            let consensus_tx = valid_tx.transaction().clone_into_consensus();
            let mut encoded = Vec::with_capacity(consensus_tx.encode_2718_len());
            consensus_tx.encode_2718(&mut encoded);
            let l1_data_fee = l1_block_info.calculate_tx_l1_cost(&encoded, hardfork);

            if is_morph_tx {
                // MorphTx: validate structural rules and ERC20 token balance via
                // the shared helper used by both admission and maintenance.
                let sender = valid_tx.transaction().sender();
                let validation_res = match token_validator.take() {
                    Some(validator) => self.validate_morph_tx_balance_with_validator(
                        &consensus_tx,
                        sender,
                        balance,
                        l1_data_fee,
                        head,
                        validator,
                    ),
                    None => self.validate_morph_tx_balance(
                        &consensus_tx,
                        sender,
                        balance,
                        l1_data_fee,
                        state,
                    ),
                };
                if let Err(err) = validation_res {
                    return morph_tx_validation_outcome(valid_tx.into_transaction(), err);
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

    /// Validates MorphTx (0x7F) ERC20 token balance and fee_limit against `state`.
    fn validate_morph_tx_balance(
        &self,
        consensus_tx: &reth_primitives_traits::Recovered<MorphTxEnvelope>,
        sender: Address,
        eth_balance: U256,
        l1_data_fee: U256,
        state: &MorphValidationState,
    ) -> Result<crate::MorphTxValidationResult, MorphTxValidationError<String>> {
        let evm_state_provider = (&state.provider).into_evm_state_provider();
        let mut validator = TokenFeeValidator::new(
            &state.head.state_cache,
            StateProviderDatabase::new(&evm_state_provider),
            state.head.evm_env.clone(),
        );
        self.validate_morph_tx_balance_with_validator(
            consensus_tx,
            sender,
            eth_balance,
            l1_data_fee,
            &state.head,
            &mut validator,
        )
    }

    fn validate_morph_tx_balance_with_validator(
        &self,
        consensus_tx: &reth_primitives_traits::Recovered<MorphTxEnvelope>,
        sender: Address,
        eth_balance: U256,
        l1_data_fee: U256,
        head: &MorphValidationHead,
        token_validator: &mut TokenFeeValidator<'_>,
    ) -> Result<crate::MorphTxValidationResult, MorphTxValidationError<String>> {
        let input = crate::MorphTxValidationInput {
            consensus_tx,
            sender,
            eth_balance,
            l1_data_fee,
            hardfork: head.evm_env.spec,
            evm_env: &head.evm_env,
        };

        let result = validate_morph_tx_with(&input, |token_id, caller| {
            token_validator.resolve_token_fee_info(token_id, caller)
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
        if !transactions.iter().any(|(_, tx)| is_morph_tx(tx)) {
            let mut state = None;
            return transactions
                .into_iter()
                .map(|(origin, tx)| self.validate_one_with_state(origin, tx, &mut state))
                .collect();
        }

        let snapshot = match self.current_validation_state() {
            Ok(snapshot) => snapshot,
            Err(err) => {
                // Avoid dropping transactions silently; report the same state error per tx.
                let message = err.to_string();
                return transactions
                    .into_iter()
                    .map(|(_origin, tx)| {
                        if let Some(err) = reject_unsupported_tx_type(&tx) {
                            return TransactionValidationOutcome::Invalid(tx, err);
                        }
                        TransactionValidationOutcome::Error(
                            *tx.hash(),
                            Box::new(std::io::Error::other(message.clone())),
                        )
                    })
                    .collect();
            }
        };

        let evm_state_provider = (&snapshot.provider).into_evm_state_provider();
        let mut token_validator = TokenFeeValidator::new(
            &snapshot.head.state_cache,
            StateProviderDatabase::new(&evm_state_provider),
            snapshot.head.evm_env.clone(),
        );

        // Reuse `token_validator` across the whole batch so `MorphEvm` and its pooled frames
        // are allocated at most once per batch.
        transactions
            .into_iter()
            .map(|(origin, tx)| {
                self.validate_one_with_snapshot(
                    origin,
                    tx,
                    &snapshot,
                    Some(&mut token_validator),
                )
            })
            .collect()
    }
}

impl<Client, Tx, Evm> TransactionValidator for MorphTransactionValidator<Client, Tx, Evm>
where
    Client: ChainSpecProvider<ChainSpec: MorphHardforks>
        + StateProviderFactory
        + BlockReaderIdExt<Header = HeaderTy<Evm::Primitives>>,
    Tx: EthPoolTransaction<Consensus = MorphTxEnvelope>,
    Evm: ConfigureEvm,
    EvmEnvFor<Evm>: Into<MorphEvmEnv>,
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
        // Compute the new L1/EVM validation state outside the write lock, then publish
        // `self.inner` and `self.block_info.head` together under the write lock so a
        // concurrent `current_validation_state` fallback cannot race and downgrade
        // `self.inner` to an older block.
        let refreshed = validation_state_for_header(
            self.client(),
            self.inner.evm_config(),
            new_tip_block.header(),
        );
        let mut guard = self.block_info.head.write();
        self.inner.on_new_head_block(new_tip_block);
        match refreshed {
            Ok(state) => *guard = Some(state.head),
            Err(err) => {
                *guard = None;
                tracing::warn!(target: "morph::txpool", %err, "Failed to refresh fee-validation head");
            }
        }
    }
}

/// Maps a [`MorphTxValidationError`] onto the right validation outcome.
///
/// [`TransactionValidationOutcome::Invalid`] is a verdict on the transaction: the pool
/// can record it as known-bad. Peer penalties are decided separately by the error type.
/// A failed state read is not such a verdict — the transaction may be perfectly valid and
/// simply could not be checked — so it is reported as
/// [`TransactionValidationOutcome::Error`], which discards this attempt without blaming
/// anyone and leaves the sender free to try again.
fn morph_tx_validation_outcome<Tx: EthPoolTransaction>(
    transaction: Tx,
    err: MorphTxValidationError<String>,
) -> TransactionValidationOutcome<Tx> {
    match err {
        MorphTxValidationError::State(err) => {
            TransactionValidationOutcome::Error(*transaction.hash(), Box::new(std::io::Error::other(err)))
        }
        MorphTxValidationError::Invalid(err) => {
            TransactionValidationOutcome::Invalid(transaction, err.into())
        }
    }
}

/// Helper function to check if a transaction is an L1 message.
fn is_l1_message(tx: &impl Typed2718) -> bool {
    tx.ty() == morph_primitives::L1_TX_TYPE_ID
}

/// Rejects transaction types that are unconditionally disallowed in the Morph txpool
/// (EIP-4844 blob transactions and L1 message transactions).
fn reject_unsupported_tx_type<Tx: EthPoolTransaction>(
    transaction: &Tx,
) -> Option<InvalidPoolTransactionError> {
    if transaction.is_eip4844() {
        return Some(InvalidTransactionError::Eip4844Disabled.into());
    }
    if is_l1_message(transaction) {
        return Some(InvalidTransactionError::TxTypeNotSupported.into());
    }
    None
}

/// Helper function to check if a transaction is a MorphTx (0x7F).
fn is_morph_tx(tx: &impl Typed2718) -> bool {
    tx.ty() == morph_primitives::MORPH_TX_TYPE_ID
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MorphPooledTransaction;
    use crate::morph_tx_validation::tests::{
        BALANCE_OF_RUNTIME, FAILING_BALANCE_OF, call_mode_registry_storage, test_evm_env,
        token_balance_key,
    };
    use alloy_consensus::{Signed, TxEip1559, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{B256, Bytes, Signature, TxKind, address};
    use morph_chainspec::{MORPH_MAINNET, MorphChainSpec, MorphHardfork};
    use morph_evm::MorphEvmConfig;
    use morph_primitives::{MorphHeader, MorphPrimitives, TxL1Msg, TxMorph};
    use morph_revm::L2_TOKEN_REGISTRY_ADDRESS;
    use reth_chainspec::EthChainSpec;
    use reth_primitives_traits::Recovered;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::{
        LocalTransactionConfig, blobstore::InMemoryBlobStore,
        validate::EthTransactionValidatorBuilder,
    };

    const FEE_TOKEN: Address = address!("5300000000000000000000000000000000000042");
    const SIGNER: Address = address!("0000000000000000000000000000000000000001");

    fn new_mock_provider() -> MockEthProvider<MorphPrimitives, MorphChainSpec> {
        MockEthProvider::<MorphPrimitives, _>::new()
            .with_chain_spec((**MORPH_MAINNET).clone())
            .with_genesis_block()
    }

    fn storage_key(slot: U256) -> B256 {
        B256::from(slot.to_be_bytes::<32>())
    }

    fn call_mode_token_accounts(
        token_id: u16,
        token: Address,
        payer: Address,
        token_balance: U256,
        runtime: &[u8],
    ) -> [(Address, ExtendedAccount); 2] {
        let registry = ExtendedAccount::new(0, U256::ZERO).extend_storage(
            call_mode_registry_storage(token_id, token)
                .into_iter()
                .map(|(k, v)| (storage_key(k), v)),
        );
        let token_account = ExtendedAccount::new(0, U256::ZERO)
            .with_bytecode(Bytes::copy_from_slice(runtime))
            .extend_storage([(storage_key(token_balance_key(payer)), token_balance)]);
        [(L2_TOKEN_REGISTRY_ADDRESS, registry), (token, token_account)]
    }

    fn head_at(number: u64, timestamp: u64, gas_limit: u64) -> MorphHeader {
        MorphHeader {
            inner: alloy_consensus::Header {
                number,
                timestamp,
                gas_limit,
                base_fee_per_gas: Some(1),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn validator_at_head(
        client: MockEthProvider<MorphPrimitives, MorphChainSpec>,
        header: MorphHeader,
        tx_fee_cap: u128,
    ) -> MorphTransactionValidator<
        MockEthProvider<MorphPrimitives, MorphChainSpec>,
        MorphPooledTransaction,
        MorphEvmConfig,
    > {
        let hash = header.hash_slow();
        let block = morph_primitives::Block::new(header, Default::default());
        client.add_block(hash, block);
        let morph_evm_config = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone());
        let eth_validator = EthTransactionValidatorBuilder::new(client, morph_evm_config)
            .disable_balance_check()
            .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
            .set_tx_fee_cap(tx_fee_cap)
            .with_local_transactions_config(LocalTransactionConfig::default())
            .no_shanghai()
            .no_cancun()
            .build::<MorphPooledTransaction, _>(InMemoryBlobStore::default());
        MorphTransactionValidator::new(eth_validator)
    }

    fn pooled_morph_tx(
        version: u8,
        fee_token_id: u16,
        gas_limit: u64,
        max_fee_per_gas: u128,
        fee_limit: U256,
        value: U256,
    ) -> MorphPooledTransaction {
        let tx = TxMorph {
            chain_id: MORPH_MAINNET.chain().id(),
            nonce: 0,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas: max_fee_per_gas,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value,
            access_list: Default::default(),
            version,
            fee_token_id,
            fee_limit,
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            input: Bytes::new(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));
        let recovered = Recovered::new_unchecked(envelope, SIGNER);
        let len = recovered.encode_2718_len();
        MorphPooledTransaction::new(recovered, len)
    }

    #[test]
    fn test_morph_l1_block_info_default() {
        let info = MorphL1BlockInfo::new();
        assert_eq!(info.timestamp(), 0);
        assert_eq!(info.number(), 0);
        assert_eq!(info.base_fee_per_gas(), None);
    }

    #[test]
    fn test_morph_l1_block_info_update() {
        let info = MorphL1BlockInfo::new();
        let header = head_at(100, 1234, 30_000_000);
        info.update(
            L1BlockInfo::default(),
            &header,
            test_evm_env(MorphHardfork::Celadon),
        );

        assert_eq!(info.timestamp(), 1234);
        assert_eq!(info.number(), 100);
        assert_eq!(info.base_fee_per_gas(), Some(1));
    }

    #[test]
    fn validate_l1_message_rejected() {
        let client = new_mock_provider();
        let morph_evm_config = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone());
        let eth_validator = EthTransactionValidatorBuilder::new(client, morph_evm_config)
            .no_shanghai()
            .no_cancun()
            .build::<MorphPooledTransaction, _>(InMemoryBlobStore::default());
        let validator = MorphTransactionValidator::new(eth_validator);

        let l1_msg_tx = TxL1Msg {
            queue_index: 0,
            gas_limit: 21_000,
            to: address!("0000000000000000000000000000000000000002"),
            value: U256::ZERO,
            input: Default::default(),
            sender: SIGNER,
        };
        let envelope = MorphTxEnvelope::L1Msg(alloy_consensus::Sealed::new_unchecked(
            l1_msg_tx,
            B256::ZERO,
        ));
        let recovered = Recovered::new_unchecked(envelope, SIGNER);
        let len = recovered.encode_2718_len();
        let pooled_tx = MorphPooledTransaction::new(recovered, len);

        let outcome = validator.validate_one(TransactionOrigin::External, pooled_tx);
        assert!(outcome.is_invalid());
    }

    #[test]
    fn fork_not_active_rejections_do_not_penalize_peers() {
        use alloy_primitives::TxHash;
        use reth_transaction_pool::error::PoolError;

        let client = new_mock_provider();
        client.add_account(SIGNER, ExtendedAccount::new(0, U256::from(10u128.pow(18))));
        // Timestamp 0 is before Viridian, Emerald, Jade, and Celadon on mainnet.
        let validator = validator_at_head(client, head_at(1, 0, 30_000_000), 0);

        // Pre-Emerald MorphTx -> ForkNotActive, not bad tx.
        let tx = pooled_morph_tx(0, 0, 21_000, 1_000_000, U256::ZERO, U256::ZERO);
        let outcome = validator.validate_one(TransactionOrigin::External, tx);
        let TransactionValidationOutcome::Invalid(_, err) = outcome else {
            panic!("expected pre-Emerald MorphTx to be rejected");
        };
        assert!(!PoolError::new(TxHash::ZERO, err).is_bad_transaction());
    }

    #[test]
    fn malformed_morph_tx_rejected_before_stateful_validation() {
        let client = new_mock_provider();
        // Post-Celadon timestamp (way in the future) so fork gates pass.
        let validator = validator_at_head(client, head_at(1, 2_000_000_000, 30_000_000), 0);
        // Invalid format: fee_token_id == 0 with non-zero fee_limit (V1).
        let tx = pooled_morph_tx(1, 0, 21_000, 1_000_000, U256::from(1u64), U256::ZERO);
        let outcome = validator.validate_one(TransactionOrigin::External, tx);
        let TransactionValidationOutcome::Invalid(_, err) = outcome else {
            panic!("expected malformed MorphTx to be rejected");
        };
        assert!(
            err.to_string().contains("invalid MorphTx format"),
            "expected InvalidFormat before stateful check, got: {err}"
        );
    }

    #[test]
    fn local_token_fee_morph_tx_still_enforces_tx_fee_cap() {
        let client = new_mock_provider();
        for (addr, acc) in call_mode_token_accounts(
            1,
            FEE_TOKEN,
            SIGNER,
            U256::from(10u128.pow(18)),
            BALANCE_OF_RUNTIME,
        ) {
            client.add_account(addr, acc);
        }
        client.add_account(SIGNER, ExtendedAccount::new(0, U256::ZERO));

        // Cap at 1_000_000 wei; tx budget is 21_000 * 100 = 2_100_000 wei > cap.
        let validator =
            validator_at_head(client, head_at(1, 2_000_000_000, 30_000_000), 1_000_000);
        let tx = pooled_morph_tx(0, 1, 21_000, 100, U256::ZERO, U256::ZERO);
        let outcome = validator.validate_one(TransactionOrigin::Local, tx);
        assert!(matches!(
            outcome,
            TransactionValidationOutcome::Invalid(_, InvalidPoolTransactionError::ExceedsFeeCap { .. })
        ));
    }

    #[test]
    fn call_mode_token_fee_validation_and_reverts() {
        for (label, runtime) in FAILING_BALANCE_OF {
            let client = new_mock_provider();
            for (addr, acc) in
                call_mode_token_accounts(1, FEE_TOKEN, SIGNER, U256::from(10_000_000u64), runtime)
            {
                client.add_account(addr, acc);
            }
            client.add_account(SIGNER, ExtendedAccount::new(0, U256::ZERO));
            let validator = validator_at_head(client, head_at(1, 2_000_000_000, 30_000_000), 0);
            let tx = pooled_morph_tx(0, 1, 21_000, 100, U256::ZERO, U256::ZERO);
            let outcome = validator.validate_one(TransactionOrigin::External, tx);
            let TransactionValidationOutcome::Invalid(_, err) = outcome else {
                panic!("expected invalid outcome for failing balanceOf case {label}");
            };
            assert!(err.to_string().contains("balanceOf failed"));
        }
    }

    #[test]
    fn stale_head_auto_refreshes_when_canonical_tip_advances() {
        let client = new_mock_provider();
        client.add_account(SIGNER, ExtendedAccount::new(0, U256::from(10u128.pow(18))));
        // Start at timestamp 0 (pre-Emerald), with block gas limit 100_000.
        let validator = validator_at_head(client.clone(), head_at(1, 0, 100_000), 0);
        assert_eq!(validator.block_info().number(), 1);

        // Advance canonical tip in provider WITHOUT calling `on_new_head_block`
        // (simulating deep catch-up where upstream maintenance skips `on_new_head_block`).
        let new_header = head_at(50, 2_000_000_000, 30_000_000);
        let new_hash = new_header.hash_slow();
        client.add_block(
            new_hash,
            morph_primitives::Block::new(new_header, Default::default()),
        );

        // A post-Celadon MorphTx with gas_limit 200_000 (above old 100_000 limit!) must now pass
        // because `current_validation_state` refreshes both `block_info` and `self.inner`.
        let tx = pooled_morph_tx(2, 0, 200_000, 100, U256::ZERO, U256::ZERO);
        let outcome = validator.validate_one(TransactionOrigin::Local, tx);
        assert!(
            outcome.is_valid(),
            "expected stale validation head to auto-refresh, got {outcome:?}"
        );
        assert_eq!(validator.block_info().number(), 50);
    }

    #[test]
    fn post_viridian_calldata_heavy_tx_accepted_without_eip7623_floor_gas() {
        let client = new_mock_provider();
        client.add_account(SIGNER, ExtendedAccount::new(0, U256::from(10u128.pow(18))));
        let validator = validator_at_head(client, head_at(1, 2_000_000_000, 30_000_000), 0);

        // 1,024 non-zero calldata bytes:
        // - standard intrinsic gas: 21_000 + 16 * 1_024 = 37_384
        // - EIP-7623 floor gas:     21_000 + 40 * 1_024 = 61_960
        let tx = TxEip1559 {
            chain_id: MORPH_MAINNET.chain().id(),
            nonce: 0,
            gas_limit: 40_000,
            max_fee_per_gas: 1_000_000,
            max_priority_fee_per_gas: 100,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::from(vec![0xab; 1_024]),
        };
        let envelope = MorphTxEnvelope::Eip1559(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));
        let recovered = Recovered::new_unchecked(envelope, SIGNER);
        let len = recovered.encode_2718_len();
        let pooled = MorphPooledTransaction::new(recovered, len);

        let outcome = validator.validate_one(TransactionOrigin::External, pooled);
        assert!(
            outcome.is_valid(),
            "post-Viridian tx below EIP-7623 floor must be accepted: {outcome:?}"
        );

        // Also ensure pre-Viridian legacy initcode word cost (EIP-3860) is enforced.
        let legacy = TxLegacy {
            chain_id: Some(MORPH_MAINNET.chain().id()),
            nonce: 0,
            gas_price: 1_000_000,
            gas_limit: 21_000,
            to: TxKind::Create,
            value: U256::ZERO,
            input: Bytes::from(vec![0x00; 64]),
        };
        let envelope = MorphTxEnvelope::Legacy(Signed::new_unchecked(
            legacy,
            Signature::test_signature(),
            B256::ZERO,
        ));
        let recovered = Recovered::new_unchecked(envelope, SIGNER);
        let len = recovered.encode_2718_len();
        let pooled = MorphPooledTransaction::new(recovered, len);
        assert!(
            validator
                .validate_one(TransactionOrigin::External, pooled)
                .is_invalid()
        );
    }
}
