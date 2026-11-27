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

use crate::MorphTxValidationError;
use alloy_consensus::{
    BlockHeader, Sealable, Transaction,
    constants::{
        EIP1559_TX_TYPE_ID, EIP2930_TX_TYPE_ID, EIP4844_TX_TYPE_ID, EIP7702_TX_TYPE_ID,
        LEGACY_TX_TYPE_ID,
    },
};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_primitives::{Address, B256, U256};
use morph_chainspec::hardfork::{MorphHardfork, MorphHardforks};
use morph_primitives::MorphTxEnvelope;
use morph_revm::{L1BlockInfo, MorphBlockEnv, MorphEvmEnv};
use parking_lot::RwLock;
use reth_chainspec::ChainSpecProvider;
use reth_evm::{ConfigureEvm, EvmFactory, EvmFactoryFor};
use reth_primitives_traits::{
    BlockTy, GotExpected, HeaderTy, SealedBlock, SealedHeader,
    transaction::error::InvalidTransactionError,
};
use reth_revm::{
    database::StateProviderDatabase,
    revm::{
        context::{Cfg as _, CfgEnv},
        context_interface::cfg::gas_params::Eip2780TxInfo,
        primitives::hardfork::SpecId,
    },
};
use reth_storage_api::{BlockReaderIdExt, StateProviderBox, StateProviderFactory};
use reth_transaction_pool::{
    EthPoolTransaction, EthTransactionValidator, PoolTransaction, TransactionOrigin,
    TransactionValidationOutcome, TransactionValidator,
    error::{Eip7702PoolTransactionError, InvalidPoolTransactionError},
};
use std::sync::Arc;

/// EIP-3860 max initcode size (`2 * MAX_CODE_SIZE = 2 * 24 576 = 49 152` bytes).
///
/// Reuses revm's canonical constant — the same `eip3860::MAX_INITCODE_SIZE` that
/// reth's `EthTransactionValidator` uses — so it stays pinned to the EIP-170
/// code-size base instead of a hand-copied literal. Enforced unconditionally at
/// the txpool layer because Morph has been post-Shanghai since genesis; see
/// `validate_one_with_state` for why we can't rely on reth's Shanghai-gated check.
const MAX_INITCODE_SIZE: usize = reth_revm::revm::primitives::eip3860::MAX_INITCODE_SIZE;

/// A complete set of fee-validation inputs for one block.
#[derive(Debug)]
pub(crate) struct MorphValidationHead {
    hash: B256,
    number: u64,
    timestamp: u64,
    base_fee_per_gas: Option<u64>,
    pub(crate) l1_block_info: L1BlockInfo,
    pub(crate) evm_env: MorphEvmEnv,
}

/// Tracks L1 fee parameters and the matching block environment.
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
/// Created lazily by [`MorphTransactionValidator::validate_one_with_state`]. Reusing it
/// keeps account reads, token reads and the EVM environment on the same block, even if
/// the canonical head advances. Start with `None` to validate a new batch at the new head.
pub struct MorphValidationState {
    pub(crate) head: Arc<MorphValidationHead>,
    pub(crate) provider: StateProviderBox,
}

/// Opens state, EVM environment and L1 parameters for exactly one header.
pub(crate) fn validation_state_for_header<Client, Evm>(
    client: &Client,
    evm_config: &Evm,
    header: &HeaderTy<Evm::Primitives>,
) -> Result<MorphValidationState, Box<dyn std::error::Error + Send + Sync>>
where
    Client: StateProviderFactory,
    Evm: ConfigureEvm,
    EvmFactoryFor<Evm>: EvmFactory<Spec = MorphHardfork, BlockEnv = MorphBlockEnv>,
{
    let evm_env = evm_config
        .evm_env(header)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let provider = client.state_by_block_hash(header.hash_slow())?;
    let l1_block_info = L1BlockInfo::try_fetch(
        &mut StateProviderDatabase::new(&provider),
        *evm_env.cfg_env.spec(),
    )?;
    Ok(MorphValidationState {
        head: Arc::new(MorphValidationHead {
            hash: header.hash_slow(),
            number: header.number(),
            timestamp: header.timestamp(),
            base_fee_per_gas: header.base_fee_per_gas(),
            l1_block_info,
            evm_env,
        }),
        provider,
    })
}

impl std::fmt::Debug for MorphValidationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MorphValidationState")
            .field("head", &self.head)
            .finish_non_exhaustive()
    }
}

/// Validator for Morph L2 transactions.
///
/// This validator extends [`EthTransactionValidator`] with Morph-specific checks:
/// - Rejects EIP-4844 blob transactions (not supported on L2)
/// - Rejects L1 message transactions (only included by sequencer)
/// - Rejects EIP-7702 transactions before Viridian and MorphTx before Emerald
/// - Enforces the EIP-3860 initcode size limit regardless of Shanghai activation
/// - Validates L1 data fee affordability
/// - Validates MorphTx (0x7F) ERC20 token balance and fee_limit
///
/// # MorphTx Validation
///
/// For token-fee MorphTx (type 0x7F, `fee_token_id > 0`), this validator performs
/// additional checks:
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
    /// Maximum gas limit for individual transactions (`--txpool.max-tx-gas`).
    max_tx_gas_limit: Option<u64>,
}

impl<Client, Tx, Evm> MorphTransactionValidator<Client, Tx, Evm> {
    /// Configures the maximum gas limit for individual transactions.
    pub const fn with_max_tx_gas_limit(mut self, max_tx_gas_limit: Option<u64>) -> Self {
        self.max_tx_gas_limit = max_tx_gas_limit;
        self
    }

    /// Returns the configured maximum gas limit for individual transactions.
    pub const fn max_tx_gas_limit(&self) -> Option<u64> {
        self.max_tx_gas_limit
    }

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

/// Preserves `max_tx_gas_limit` when callers construct [`MorphTransactionValidator::new`]
/// from an [`EthTransactionValidator`] without also calling
/// [`MorphTransactionValidator::with_max_tx_gas_limit`] (upstream `reth` keeps the field
/// private on `EthTransactionValidator` while exposing it in `Debug`).
fn inner_max_tx_gas_limit<Client, Tx, Evm>(
    inner: &EthTransactionValidator<Client, Tx, Evm>,
) -> Option<u64> {
    let debug = format!("{inner:?}");
    let rest = debug.split("max_tx_gas_limit: Some(").nth(1)?;
    let digits = rest.split(')').next()?;
    digits.parse().ok()
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
    // Pins the cached environment to Morph's, so the fee-token balance query runs in
    // exactly what the execution layer would use.
    EvmFactoryFor<Evm>: EvmFactory<Spec = MorphHardfork, BlockEnv = MorphBlockEnv>,
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
        let max_tx_gas_limit = inner_max_tx_gas_limit(&inner);
        Self {
            inner,
            block_info: Arc::new(block_info),
            max_tx_gas_limit,
        }
    }

    /// Synchronizes the inner [`EthTransactionValidator`]'s `block_gas_limit` and
    /// `fork_tracker` with `header`.
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
                .is_none_or(|tip| tip.best_hash == head.hash);
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
    /// `on_new_head_block`. Both are reused for subsequent transactions in the batch.
    /// Reset `state` to `None` to pick up a newer head.
    pub fn validate_one_with_state(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: &mut Option<MorphValidationState>,
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

        if state.is_none() {
            match self.current_validation_state() {
                Ok(snapshot) => *state = Some(snapshot),
                Err(err) => return TransactionValidationOutcome::Error(*transaction.hash(), err),
            }
        }
        let state = state.as_ref().expect("validation state initialized above");
        let head = &state.head;

        // Reject EIP-7702 transactions before Viridian hardfork (PRAGUE)
        if transaction.is_eip7702()
            && !self
                .chain_spec()
                .is_viridian_active_at_timestamp(head.timestamp)
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
                .is_emerald_active_at_timestamp(head.timestamp)
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

        if let Err(err) = self.validate_stateless(origin, &transaction, &head.evm_env.cfg_env) {
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
            let hardfork = *head.evm_env.cfg_env.spec();

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
                if let Err(err) = self.validate_morph_tx_balance(
                    &consensus_tx,
                    sender,
                    balance,
                    l1_data_fee,
                    state,
                ) {
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

    /// Validates MorphTx (0x7F) ERC20 token balance and fee_limit.
    ///
    /// Accepts `&Recovered<MorphTxEnvelope>` directly (already cloned by the caller)
    /// to avoid a redundant second `clone_into_consensus()`.
    ///
    /// This method performs the following checks (reference: go-ethereum tx_pool.go:727-791):
    /// 1. Structural MorphTx rules (`version`, `fee_limit`, memo length, fee ordering)
    /// 2. ETH balance must be >= transaction value (value is still in ETH)
    /// 3. `fee_token_id == 0`: ETH-fee path, require ETH affordability for `cost + l1_fee`
    /// 4. `fee_token_id > 0`: token must be registered and active in L2TokenRegistry
    /// 5. Token price ratio must be valid (non-zero)
    /// 6. Effective token limit must cover required token amount
    fn validate_morph_tx_balance(
        &self,
        consensus_tx: &reth_primitives_traits::Recovered<MorphTxEnvelope>,
        sender: Address,
        eth_balance: U256,
        l1_data_fee: U256,
        state: &MorphValidationState,
    ) -> Result<crate::MorphTxValidationResult, MorphTxValidationError<reth_provider::ProviderError>>
    {
        let mut db = StateProviderDatabase::new(&state.provider);
        let input = crate::MorphTxValidationInput {
            consensus_tx,
            sender,
            eth_balance,
            l1_data_fee,
            hardfork: *state.head.evm_env.cfg_env.spec(),
            evm_env: &state.head.evm_env,
        };

        let result = crate::validate_morph_tx(&mut db, &input)?;
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

    /// Validates all given transactions, reusing a single state provider across the batch.
    ///
    /// Returns all outcomes for the given transactions in the same order.
    ///
    /// See also [`Self::validate_one`]
    pub fn validate_all(
        &self,
        transactions: Vec<(TransactionOrigin, Tx)>,
    ) -> Vec<TransactionValidationOutcome<Tx>> {
        let mut state = None;
        transactions
            .into_iter()
            .map(|(origin, tx)| self.validate_one_with_state(origin, tx, &mut state))
            .collect()
    }

    /// Performs Morph-aware stateless validation directly without delegating to
    /// `self.inner.validate_stateless`.
    ///
    /// Upstream `EthTransactionValidator::validate_stateless` evaluates intrinsic gas via
    /// `ForkTracker` (ignoring `cfg_env.disable_eip7623` post-Viridian and omitting EIP-3860
    /// initcode word cost pre-Viridian) and, in newer upstream versions, increments
    /// `rejected_intrinsic_gas_too_low` before returning `Err(IntrinsicGasTooLow)`. Running
    /// the stateless checks here against `self.inner`'s configuration and `cfg_env` preserves
    /// exact error precedence without false EIP-7623 rejections or metric pollution.
    fn validate_stateless(
        &self,
        origin: TransactionOrigin,
        transaction: &Tx,
        cfg_env: &CfgEnv<MorphHardfork>,
    ) -> Result<(), InvalidPoolTransactionError> {
        match transaction.ty() {
            EIP2930_TX_TYPE_ID if !self.inner.eip2718() => {
                return Err(InvalidTransactionError::Eip2930Disabled.into());
            }
            EIP1559_TX_TYPE_ID if !self.inner.eip1559() => {
                return Err(InvalidTransactionError::Eip1559Disabled.into());
            }
            EIP4844_TX_TYPE_ID => {
                return Err(InvalidTransactionError::Eip4844Disabled.into());
            }
            EIP7702_TX_TYPE_ID if !self.inner.eip7702() => {
                return Err(InvalidTransactionError::Eip7702Disabled.into());
            }
            LEGACY_TX_TYPE_ID
            | EIP2930_TX_TYPE_ID
            | EIP1559_TX_TYPE_ID
            | EIP7702_TX_TYPE_ID
            | morph_primitives::MORPH_TX_TYPE_ID => {}
            _ => {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
        }

        // Reject transactions with a nonce equal to u64::MAX according to EIP-2681.
        if transaction.nonce() == u64::MAX {
            return Err(InvalidPoolTransactionError::Eip2681);
        }

        // Reject non-blob transactions over the configured maximum encoded byte size.
        let tx_size = transaction.encoded_length();
        let max_tx_input_bytes = self.inner.max_tx_input_bytes();
        if tx_size > max_tx_input_bytes {
            return Err(InvalidPoolTransactionError::OversizedData {
                size: tx_size,
                limit: max_tx_input_bytes,
            });
        }

        // Respect any stricter `cfg_env` initcode cap in addition to `MAX_INITCODE_SIZE`.
        transaction.ensure_max_init_code_size(cfg_env.max_initcode_size())?;

        // Enforce block gas limit and optional per-transaction gas limit (`--txpool.max-tx-gas`).
        let transaction_gas_limit = transaction.gas_limit();
        let block_gas_limit = self.inner.block_gas_limit();
        if transaction_gas_limit > block_gas_limit {
            return Err(InvalidPoolTransactionError::ExceedsGasLimit(
                transaction_gas_limit,
                block_gas_limit,
            ));
        }

        if let Some(max_tx_gas_limit) = self.max_tx_gas_limit
            && transaction_gas_limit > max_tx_gas_limit
        {
            return Err(InvalidPoolTransactionError::MaxTxGasLimitExceeded(
                transaction_gas_limit,
                max_tx_gas_limit,
            ));
        }

        // Ensure max_priority_fee_per_gas (if dynamic fee) does not exceed max_fee_per_gas.
        if transaction.max_priority_fee_per_gas() > Some(transaction.max_fee_per_gas()) {
            return Err(InvalidTransactionError::TipAboveFeeCap.into());
        }

        let is_local = self
            .inner
            .local_transactions_config()
            .is_local(origin, transaction.sender_ref());

        // Enforce configured local transaction fee cap. Token-fee MorphTx reports only its
        // ETH value through `cost()`, so compute the gas budget directly for MorphTx.
        if is_local && let Some(tx_fee_cap_wei) = (*self.inner.tx_fee_cap()).filter(|cap| *cap != 0)
        {
            let max_tx_fee_wei = if is_morph_tx(transaction) {
                U256::from(transaction_gas_limit)
                    .saturating_mul(U256::from(transaction.max_fee_per_gas()))
            } else {
                transaction.cost().saturating_sub(transaction.value())
            };
            if max_tx_fee_wei > U256::from(tx_fee_cap_wei) {
                return Err(InvalidPoolTransactionError::ExceedsFeeCap {
                    max_tx_fee_wei: max_tx_fee_wei.saturating_to(),
                    tx_fee_cap_wei,
                });
            }
        }

        // Drop non-local dynamic-fee transactions below the configured minimum priority fee.
        if !is_local
            && transaction.is_dynamic_fee()
            && let Some(minimum_priority_fee) = *self.inner.minimum_priority_fee()
            && transaction.max_priority_fee_per_gas() < Some(minimum_priority_fee)
        {
            return Err(InvalidPoolTransactionError::PriorityFeeBelowMinimum {
                minimum_priority_fee,
            });
        }

        // Enforce chain ID when present (allowing pre-EIP-155 legacy transactions).
        if let Some(chain_id) = transaction.chain_id()
            && chain_id != self.inner.chain_id()
        {
            return Err(InvalidTransactionError::ChainIdMismatch.into());
        }

        if transaction.is_eip7702() {
            if !self.inner.fork_tracker().is_prague_activated() {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }

            if transaction
                .authorization_list()
                .is_none_or(|l| l.is_empty())
            {
                return Err(Eip7702PoolTransactionError::MissingEip7702AuthorizationList.into());
            }
        }

        ensure_morph_intrinsic_gas(transaction, cfg_env)?;

        let tx_gas_limit_cap = self
            .inner
            .fork_tracker()
            .tx_gas_limit_cap
            .load(std::sync::atomic::Ordering::Relaxed);
        if tx_gas_limit_cap > 0 && transaction_gas_limit > tx_gas_limit_cap {
            return Err(InvalidTransactionError::GasLimitTooHigh.into());
        }

        Ok(())
    }
}

impl<Client, Tx, Evm> TransactionValidator for MorphTransactionValidator<Client, Tx, Evm>
where
    Client: ChainSpecProvider<ChainSpec: MorphHardforks>
        + StateProviderFactory
        + BlockReaderIdExt<Header = HeaderTy<Evm::Primitives>>,
    Tx: EthPoolTransaction<Consensus = MorphTxEnvelope>,
    Evm: ConfigureEvm,
    // Pins the cached environment to Morph's, so the fee-token balance query runs in
    // exactly what the execution layer would use.
    EvmFactoryFor<Evm>: EvmFactory<Spec = MorphHardfork, BlockEnv = MorphBlockEnv>,
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

/// Validates transaction intrinsic gas against Morph's actual execution `CfgEnv`,
/// honoring `is_eip7623_disabled()` (EIP-7623 calldata floor gas is disabled on Morph)
/// while preserving EIP-3860 initcode word cost (from genesis) and EIP-7702
/// authorization-list cost.
fn ensure_morph_intrinsic_gas<Tx: EthPoolTransaction>(
    transaction: &Tx,
    cfg: &CfgEnv<MorphHardfork>,
) -> Result<(), InvalidPoolTransactionError> {
    let spec: SpecId = (*cfg.spec()).into();
    let eip2780 = cfg.is_amsterdam_eip2780_enabled().then(|| Eip2780TxInfo {
        value: transaction.value(),
        is_self_transfer: transaction.kind().to() == Some(&transaction.sender()),
    });
    let mut gas = cfg.gas_params().initial_tx_gas(
        transaction.input(),
        transaction.is_create(),
        transaction
            .access_list()
            .map(|l| l.len())
            .unwrap_or_default() as u64,
        transaction
            .access_list()
            .map(|l| l.iter().map(|i| i.storage_keys.len()).sum::<usize>())
            .unwrap_or_default() as u64,
        transaction
            .authorization_list()
            .map(|l| l.len())
            .unwrap_or_default() as u64,
        eip2780,
    );
    if cfg.is_eip7623_disabled() {
        gas.set_floor_gas(0);
    }
    if !cfg.is_amsterdam_eip8037_enabled() {
        gas.set_initial_state_gas(0);
    }

    let gas_limit = transaction.gas_limit();
    if gas_limit < gas.initial_total_gas()
        || (spec.is_enabled_in(SpecId::PRAGUE) && gas_limit < gas.floor_gas())
    {
        return Err(InvalidPoolTransactionError::IntrinsicGasTooLow);
    }
    Ok(())
}

/// Maps a [`MorphTxValidationError`] onto the right validation outcome.
///
/// [`TransactionValidationOutcome::Invalid`] is a verdict on the transaction: the pool
/// can record it as known-bad. Peer penalties are decided separately by the error type.
/// A failed state read is not such a verdict — the transaction may be perfectly valid and
/// simply could not be checked — so it is reported as
/// [`TransactionValidationOutcome::Error`], which discards this attempt without blaming
/// anyone and leaves the sender free to try again.
fn morph_tx_validation_outcome<
    Tx: EthPoolTransaction,
    E: std::error::Error + Send + Sync + 'static,
>(
    transaction: Tx,
    err: MorphTxValidationError<E>,
) -> TransactionValidationOutcome<Tx> {
    match err {
        MorphTxValidationError::State(err) => {
            TransactionValidationOutcome::Error(*transaction.hash(), Box::new(err))
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

/// Helper function to check if a transaction is a MorphTx (0x7F).
fn is_morph_tx(tx: &impl Typed2718) -> bool {
    tx.ty() == morph_primitives::MORPH_TX_TYPE_ID
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MorphTxError;
    use crate::morph_tx_validation::tests::{
        BALANCE_OF_RUNTIME, FAILING_BALANCE_OF, call_mode_registry_storage, token_balance_key,
    };
    use alloy_consensus::{Sealable, Signed, TxEip1559, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{B256, Bytes, Signature, TxKind, address};
    use morph_chainspec::{MORPH_MAINNET, MorphChainSpec};
    use morph_evm::MorphEvmConfig;
    use morph_primitives::{MorphPrimitives, TxL1Msg, TxMorph};
    use morph_revm::L2_TOKEN_REGISTRY_ADDRESS;
    use reth_primitives_traits::Recovered;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::{
        CoinbaseTipOrdering, LocalTransactionConfig, Pool, TransactionPool,
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

    /// The registry account with `token` registered as call-mode fee token `token_id`.
    fn token_registry_account(token_id: u16, token: alloy_primitives::Address) -> ExtendedAccount {
        ExtendedAccount::new(0, U256::ZERO).extend_storage(
            call_mode_registry_storage(token_id, token)
                .map(|(slot, value)| (storage_key(slot), value)),
        )
    }

    /// A fee token whose `balanceOf` runs `code`, with `balance` recorded for `holder`.
    fn fee_token_account(
        code: &'static [u8],
        holder: alloy_primitives::Address,
        balance: U256,
    ) -> ExtendedAccount {
        ExtendedAccount::new(0, U256::ZERO)
            .with_bytecode(Bytes::from_static(code))
            .extend_storage([(storage_key(token_balance_key(holder)), balance)])
    }

    type TokenFeeValidator = MorphTransactionValidator<
        MockEthProvider<MorphPrimitives, MorphChainSpec>,
        crate::MorphPooledTransaction,
        MorphEvmConfig,
    >;

    /// Registered call-mode token with a 1:1 price ratio at an Emerald-active head.
    fn token_fee_validator(
        eth_balance: U256,
        token_balance: U256,
        fee_cap: u128,
        local_config: LocalTransactionConfig,
    ) -> TokenFeeValidator {
        let client = new_mock_provider();
        let signer = address!("0000000000000000000000000000000000000001");
        let token = address!("5300000000000000000000000000000000000042");
        let header = morph_primitives::MorphHeader::from(alloy_consensus::Header {
            number: 1,
            timestamp: 1_767_765_600,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(10),
            ..Default::default()
        });
        client.add_block(
            header.hash_slow(),
            morph_primitives::Block {
                header,
                body: Default::default(),
            },
        );
        client.add_account(signer, ExtendedAccount::new(0, eth_balance));
        client.add_account(L2_TOKEN_REGISTRY_ADDRESS, token_registry_account(1, token));
        client.add_account(
            token,
            fee_token_account(BALANCE_OF_RUNTIME, signer, token_balance),
        );
        let inner = EthTransactionValidatorBuilder::new(
            client,
            MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
        )
        .disable_balance_check()
        .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
        .set_tx_fee_cap(fee_cap)
        .with_local_transactions_config(local_config)
        .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default());
        MorphTransactionValidator::new(inner)
    }

    /// Maximum gas fee is 2,100,000 wei; value remains denominated in ETH.
    fn token_fee_transaction(tx_nonce: u64, value: U256) -> crate::MorphPooledTransaction {
        let tx = TxMorph {
            chain_id: 2818,
            nonce: tx_nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value,
            fee_token_id: 1,
            fee_limit: U256::ZERO,
            ..Default::default()
        };
        let recovered = Recovered::new_unchecked(
            MorphTxEnvelope::Morph(Signed::new_unhashed(tx, Signature::test_signature())),
            address!("0000000000000000000000000000000000000001"),
        );
        let len = recovered.encode_2718_len();
        crate::MorphPooledTransaction::new(recovered, len)
    }

    fn timestamp_sensitive_validator() -> TokenFeeValidator {
        let validator =
            token_fee_validator(U256::ZERO, U256::from(10_000_000), 0, Default::default());
        let client = validator.client();
        let token = address!("5300000000000000000000000000000000000042");
        // Return 10,000,000 only at the old head's timestamp. The token state stays
        // unchanged so both reads of the cached provider must use the old environment.
        let old_timestamp = 1_767_765_600u32;
        let mut code = vec![0x42, 0x63]; // TIMESTAMP PUSH4
        code.extend_from_slice(&old_timestamp.to_be_bytes());
        code.extend_from_slice(&[
            0x14, 0x62, 0x98, 0x96, 0x80, 0x02, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
        ]);
        client.add_account(
            token,
            ExtendedAccount::new(0, U256::ZERO).with_bytecode(code.into()),
        );

        validator
    }

    fn replacement_header() -> morph_primitives::MorphHeader {
        morph_primitives::MorphHeader::from(alloy_consensus::Header {
            number: 1,
            timestamp: 1_767_765_601,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(10),
            ..Default::default()
        })
    }

    #[test]
    fn a_head_published_during_validation_does_not_change_the_balance_query() {
        let mut validator = timestamp_sensitive_validator();
        let block_info = validator.block_info().clone();
        let client = validator.client().clone();
        let replacement = replacement_header();
        let env = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone())
            .evm_env(&replacement)
            .unwrap();
        // This extension runs after the account read and before Morph fee validation,
        // forcing the interleaving of a concurrent canonical-head publication.
        validator
            .inner
            .set_additional_stateful_validation(move |_, _, _| {
                client
                    .blocks
                    .lock()
                    .retain(|_, block| block.header.number() != replacement.number());
                client
                    .headers
                    .lock()
                    .retain(|_, header| header.number() != replacement.number());
                client.add_block(
                    replacement.hash_slow(),
                    morph_primitives::Block {
                        header: replacement.clone(),
                        body: Default::default(),
                    },
                );
                block_info.update(L1BlockInfo::default(), &replacement, env.clone());
                Ok(())
            });
        let tx = token_fee_transaction(0, U256::ZERO);
        let in_flight = validator.validate_one(TransactionOrigin::Local, tx.clone());
        assert!(
            matches!(in_flight, TransactionValidationOutcome::Valid { .. }),
            "an in-flight validation must retain its original head: {in_flight:?}"
        );
        let fresh = validator.validate_one(TransactionOrigin::Local, tx);
        assert!(
            matches!(fresh, TransactionValidationOutcome::Invalid(..)),
            "a fresh validation must see the published head: {fresh:?}"
        );
    }

    /// A `balanceOf` that yields no balance rejects the transaction without blaming the
    /// peer that relayed it: the fault lies with the token, not the sender.
    #[test]
    fn a_failing_balance_query_is_rejected_without_penalizing_the_peer() {
        for code in FAILING_BALANCE_OF {
            let validator =
                token_fee_validator(U256::ZERO, U256::from(10_000_000), 0, Default::default());
            validator.client().add_account(
                address!("5300000000000000000000000000000000000042"),
                fee_token_account(
                    code,
                    address!("0000000000000000000000000000000000000001"),
                    U256::from(10_000_000),
                ),
            );
            let outcome = validator.validate_one(
                TransactionOrigin::External,
                token_fee_transaction(0, U256::ZERO),
            );
            let err = match outcome {
                TransactionValidationOutcome::Invalid(_, err) => err,
                other => panic!("balanceOf code {code:02x?}: {other:?}"),
            };
            assert_eq!(
                err.downcast_other_ref::<MorphTxError>(),
                Some(&MorphTxError::TokenBalanceQueryFailed { token_id: 1 }),
                "balanceOf code {code:02x?}"
            );
            assert_eq!(
                err.as_other().map(|other| other.is_bad_transaction()),
                Some(false),
                "balanceOf code {code:02x?}"
            );
        }
    }

    #[test]
    fn token_fee_transaction_above_local_fee_cap_is_rejected() {
        // The effective gas price is 20, so only the maximum fee budget exceeds this cap.
        let validator = token_fee_validator(
            U256::ZERO,
            U256::from(10_000_000),
            500_000,
            Default::default(),
        );
        let outcome = validator.validate_one(
            TransactionOrigin::Local,
            token_fee_transaction(0, U256::ZERO),
        );
        assert!(
            matches!(
                outcome,
                TransactionValidationOutcome::Invalid(
                    _,
                    InvalidPoolTransactionError::ExceedsFeeCap {
                        max_tx_fee_wei: 2_100_000,
                        tx_fee_cap_wei: 500_000,
                    }
                )
            ),
            "{outcome:?}"
        );
    }

    #[test]
    fn token_fee_cap_accepts_zero_or_sufficient_cap_without_counting_value() {
        for cap in [0, 2_100_000, 2_100_001] {
            let validator = token_fee_validator(
                U256::from(7),
                U256::from(10_000_000),
                cap,
                Default::default(),
            );
            let outcome = validator.validate_one(
                TransactionOrigin::Local,
                token_fee_transaction(0, U256::from(7)),
            );
            assert!(
                matches!(outcome, TransactionValidationOutcome::Valid { .. }),
                "cap={cap}: {outcome:?}"
            );
        }
    }

    #[test]
    fn token_fee_cap_respects_local_transaction_configuration() {
        let local_sender = LocalTransactionConfig {
            local_addresses: [address!("0000000000000000000000000000000000000001")]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        for (origin, config, should_reject) in [
            (
                TransactionOrigin::External,
                LocalTransactionConfig::default(),
                false,
            ),
            (TransactionOrigin::External, local_sender.clone(), true),
            (
                TransactionOrigin::Local,
                LocalTransactionConfig {
                    no_exemptions: true,
                    ..Default::default()
                },
                false,
            ),
            (
                TransactionOrigin::External,
                LocalTransactionConfig {
                    no_exemptions: true,
                    ..local_sender
                },
                false,
            ),
        ] {
            let validator = token_fee_validator(U256::ZERO, U256::from(10_000_000), 100, config);
            let outcome = validator.validate_one(origin, token_fee_transaction(0, U256::ZERO));
            if should_reject {
                assert!(
                    matches!(
                        outcome,
                        TransactionValidationOutcome::Invalid(
                            _,
                            InvalidPoolTransactionError::ExceedsFeeCap { .. }
                        )
                    ),
                    "{outcome:?}"
                );
            } else {
                assert!(
                    matches!(outcome, TransactionValidationOutcome::Valid { .. }),
                    "{outcome:?}"
                );
            }
        }
    }

    #[test]
    fn token_fee_transaction_with_zero_eth_is_pending_and_selectable() {
        let validator = token_fee_validator(
            U256::ZERO,
            U256::from(10_000_000),
            2_100_000,
            Default::default(),
        );
        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        );
        let added = futures::executor::block_on(pool.add_transaction(
            TransactionOrigin::Local,
            token_fee_transaction(0, U256::ZERO),
        ))
        .unwrap();
        let all = pool.all_transactions();
        assert_eq!(all.pending.len(), 1);
        assert!(all.queued.is_empty());
        let best: Vec<_> = pool.best_transactions().map(|tx| *tx.hash()).collect();
        assert_eq!(best, [added.hash]);
    }

    #[test]
    fn token_fee_transactions_still_reserve_cumulative_eth_value() {
        let validator = token_fee_validator(
            U256::from(10),
            U256::from(10_000_000),
            2_100_000,
            Default::default(),
        );
        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        );
        for nonce in [0, 1] {
            futures::executor::block_on(pool.add_transaction(
                TransactionOrigin::Local,
                token_fee_transaction(nonce, U256::from(7)),
            ))
            .unwrap();
        }
        let all = pool.all_transactions();
        assert_eq!(all.pending.len(), 1);
        assert_eq!(all.pending[0].nonce(), 0);
        assert_eq!(all.queued.len(), 1);
        assert_eq!(all.queued[0].nonce(), 1);
        let best: Vec<_> = pool.best_transactions().map(|tx| tx.nonce()).collect();
        assert_eq!(best, [0]);
    }

    #[test]
    fn an_unreadable_fee_token_state_is_an_error_not_an_invalid_transaction() {
        let tx = token_fee_transaction(0, U256::ZERO);
        let hash = *tx.hash();

        let outcome = morph_tx_validation_outcome(
            tx,
            MorphTxValidationError::State(std::io::Error::other("provider unavailable")),
        );
        assert!(
            matches!(outcome, TransactionValidationOutcome::Error(reported, _) if reported == hash),
            "a failed state read must not mark the transaction known-bad: {outcome:?}"
        );
    }

    #[test]
    fn a_real_fee_token_failure_is_still_an_invalid_transaction() {
        let outcome = morph_tx_validation_outcome(
            token_fee_transaction(0, U256::ZERO),
            MorphTxValidationError::<std::io::Error>::Invalid(MorphTxError::TokenNotActive {
                token_id: 1,
            }),
        );
        assert!(
            matches!(outcome, TransactionValidationOutcome::Invalid(..)),
            "{outcome:?}"
        );
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
        let header = morph_primitives::MorphHeader::from(alloy_consensus::Header {
            timestamp: 1234,
            number: 100,
            base_fee_per_gas: Some(42),
            ..Default::default()
        });
        let evm_env = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone())
            .evm_env(&header)
            .unwrap();
        info.update(l1_info, &header, evm_env);

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
    /// Shanghai-gated EIP-3860 check (`EthTransactionValidator::validate_stateless`) is
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
        let validator = token_fee_validator(U256::ZERO, U256::from(300_000), 0, Default::default());
        let signer = address!("0000000000000000000000000000000000000001");
        // Effective execution price is 11, but admission must reserve the maximum 100.
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
        let len = recovered.encode_2718_len();
        let outcome = validator.validate_one(
            TransactionOrigin::Local,
            crate::MorphPooledTransaction::new(recovered, len),
        );
        assert!(matches!(outcome,
            TransactionValidationOutcome::Invalid(_, InvalidPoolTransactionError::Overdraft { cost, balance })
                if cost == U256::from(2_100_000) && balance == U256::from(300_000)
        ));
    }

    // -------------------------------------------------------------------------
    // Audit fixes: M-H02, M-H05, M-M08
    // -------------------------------------------------------------------------

    /// M-H02: Post-Viridian calldata-heavy transactions (both standard EIP-1559 and
    /// MorphTx 0x7F) where `standard_intrinsic <= gas_limit < eip7623_floor_gas` must
    /// be accepted by `MorphTransactionValidator` without penalizing honest peers,
    /// matching `MorphEvmConfig`'s `disable_eip7623 = true`.
    #[test]
    fn post_viridian_calldata_heavy_tx_accepted_without_eip7623_floor_gas() {
        let signer = address!("0000000000000000000000000000000000000001");
        let validator = token_fee_validator(
            U256::from(10u128.pow(18)),
            U256::from(100_000_000u64),
            0,
            Default::default(),
        );
        // Post-Emerald head (timestamp 1_767_765_600 >= viridianTime 1_762_149_600)
        assert!(
            validator.inner.fork_tracker().is_prague_activated(),
            "Prague must remain active in fork_tracker for EIP-7702"
        );

        // 1,024 non-zero calldata bytes:
        // - standard intrinsic gas: 21_000 + 16 * 1_024 = 37_384
        // - EIP-7623 floor gas:     21_000 + 40 * 1_024 = 61_960
        let calldata = Bytes::from(vec![0xab; 1_024]);
        let gas_limit = 40_000u64;

        // 1. Standard EIP-1559 transaction
        let eip1559 = TxEip1559 {
            chain_id: 2818,
            nonce: 0,
            gas_limit,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            input: calldata.clone(),
        };
        let recovered = Recovered::new_unchecked(
            MorphTxEnvelope::Eip1559(Signed::new_unhashed(eip1559, Signature::test_signature())),
            signer,
        );
        let len = recovered.encode_2718_len();
        let pooled_eip1559 = crate::MorphPooledTransaction::new(recovered, len);

        // Confirm upstream `validate_stateless` would reject this tx under EIP-7623.
        assert!(matches!(
            validator
                .inner
                .validate_stateless(TransactionOrigin::External, &pooled_eip1559),
            Err(InvalidPoolTransactionError::IntrinsicGasTooLow)
        ));
        let outcome = validator.validate_one(TransactionOrigin::External, pooled_eip1559);
        assert!(
            matches!(outcome, TransactionValidationOutcome::Valid { .. }),
            "post-Viridian calldata-heavy EIP-1559 tx must be accepted: {outcome:?}"
        );

        // 2. MorphTx (0x7F) paying fees in ERC20 token
        let morph_tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::ZERO,
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            input: calldata,
        };
        let recovered = Recovered::new_unchecked(
            MorphTxEnvelope::Morph(Signed::new_unhashed(morph_tx, Signature::test_signature())),
            signer,
        );
        let len = recovered.encode_2718_len();
        let pooled_morph = crate::MorphPooledTransaction::new(recovered, len);

        assert!(matches!(
            validator
                .inner
                .validate_stateless(TransactionOrigin::External, &pooled_morph),
            Err(InvalidPoolTransactionError::IntrinsicGasTooLow)
        ));
        let outcome = validator.validate_one(TransactionOrigin::External, pooled_morph);
        assert!(
            matches!(outcome, TransactionValidationOutcome::Valid { .. }),
            "post-Viridian calldata-heavy MorphTx must be accepted: {outcome:?}"
        );
    }

    /// M-H02: Transactions genuinely below Morph's execution intrinsic gas — including
    /// standard calldata gas, EIP-3860 initcode word cost (both pre- and post-Viridian),
    /// and EIP-7702 authorization-list cost (on both type 0x04 and MorphTx V2) — are
    /// still rejected with `IntrinsicGasTooLow`, while other stateless errors keep
    /// their exact classification.
    #[test]
    fn intrinsic_gas_enforces_standard_eip3860_and_eip7702_costs() {
        use alloy_consensus::TxEip7702;
        use alloy_eips::eip7702::{Authorization, SignedAuthorization};

        let signer = address!("0000000000000000000000000000000000000001");
        let make_eip1559 = |gas_limit: u64, to: TxKind, input: Bytes, chain_id: u64| {
            let tx = TxEip1559 {
                chain_id,
                nonce: 0,
                gas_limit,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 10,
                to,
                value: U256::ZERO,
                access_list: Default::default(),
                input,
            };
            let recovered = Recovered::new_unchecked(
                MorphTxEnvelope::Eip1559(Signed::new_unhashed(tx, Signature::test_signature())),
                signer,
            );
            let len = recovered.encode_2718_len();
            crate::MorphPooledTransaction::new(recovered, len)
        };

        // 1. Pre-Viridian EIP-3860 initcode word cost (genesis timestamp = 0, where
        //    `fork_tracker.is_shanghai_activated()` is false because Morph genesis uses
        //    `shanghaiBlock: 0`).
        //    1,024 zero bytes initcode:
        //    base(53_000) + calldata(4 * 1_024 = 4_096) + initcode_words(2 * 32 = 64) = 57_160.
        let pre_client = new_mock_provider();
        pre_client.add_account(signer, ExtendedAccount::new(0, U256::from(10u128.pow(18))));
        let pre_validator = MorphTransactionValidator::new(
            EthTransactionValidatorBuilder::new(
                pre_client,
                MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
            )
            .disable_balance_check()
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default()),
        );
        assert!(!pre_validator.inner.fork_tracker().is_shanghai_activated());

        let initcode = Bytes::from(vec![0u8; 1_024]);
        let outcome = pre_validator.validate_one(
            TransactionOrigin::External,
            make_eip1559(57_159, TxKind::Create, initcode.clone(), 2818),
        );
        assert!(
            matches!(
                outcome,
                TransactionValidationOutcome::Invalid(
                    _,
                    InvalidPoolTransactionError::IntrinsicGasTooLow
                )
            ),
            "pre-Viridian initcode missing EIP-3860 word cost must be rejected: {outcome:?}"
        );
        let outcome = pre_validator.validate_one(
            TransactionOrigin::External,
            make_eip1559(57_160, TxKind::Create, initcode.clone(), 2818),
        );
        assert!(
            matches!(outcome, TransactionValidationOutcome::Valid { .. }),
            "pre-Viridian initcode meeting EIP-3860 intrinsic gas must pass: {outcome:?}"
        );

        // 2. Post-Celadon validator (Viridian + Emerald + Jade + Celadon active)
        let post_client = new_mock_provider();
        let celadon_header = morph_primitives::MorphHeader::from(alloy_consensus::Header {
            number: 1,
            timestamp: 1_793_772_000, // celadonTime
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(10),
            ..Default::default()
        });
        post_client.add_block(
            celadon_header.hash_slow(),
            morph_primitives::Block {
                header: celadon_header,
                body: Default::default(),
            },
        );
        post_client.add_account(signer, ExtendedAccount::new(0, U256::from(10u128.pow(18))));
        let post_validator = MorphTransactionValidator::new(
            EthTransactionValidatorBuilder::new(
                post_client,
                MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
            )
            .disable_balance_check()
            .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default()),
        );
        assert!(post_validator.inner.fork_tracker().is_prague_activated());

        // 2a. Below standard calldata intrinsic gas (21_000 + 16*1_024 = 37_384)
        let calldata = Bytes::from(vec![0xff; 1_024]);
        let outcome = post_validator.validate_one(
            TransactionOrigin::External,
            make_eip1559(
                37_383,
                TxKind::Call(address!("0000000000000000000000000000000000000002")),
                calldata.clone(),
                2818,
            ),
        );
        assert!(matches!(
            outcome,
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::IntrinsicGasTooLow
            )
        ));

        // 2b. Post-Viridian EIP-3860 initcode word cost
        let outcome = post_validator.validate_one(
            TransactionOrigin::External,
            make_eip1559(57_159, TxKind::Create, initcode.clone(), 2818),
        );
        assert!(matches!(
            outcome,
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::IntrinsicGasTooLow
            )
        ));
        let outcome = post_validator.validate_one(
            TransactionOrigin::External,
            make_eip1559(57_160, TxKind::Create, initcode, 2818),
        );
        assert!(matches!(
            outcome,
            TransactionValidationOutcome::Valid { .. }
        ));

        // 2c. EIP-7702 (type 0x04) authorization-list intrinsic gas:
        //     base(21_000) + 2 * 25_000 = 71_000
        let dummy_auth = SignedAuthorization::new_unchecked(
            Authorization {
                chain_id: U256::from(2818u64),
                address: address!("0000000000000000000000000000000000000099"),
                nonce: 0,
            },
            0,
            U256::from(1u64),
            U256::from(1u64),
        );
        let make_7702 = |gas_limit: u64, input: Bytes| {
            let tx = TxEip7702 {
                chain_id: 2818,
                nonce: 0,
                gas_limit,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 10,
                to: address!("0000000000000000000000000000000000000002"),
                value: U256::ZERO,
                access_list: Default::default(),
                authorization_list: vec![dummy_auth.clone(), dummy_auth.clone()],
                input,
            };
            let recovered = Recovered::new_unchecked(
                MorphTxEnvelope::Eip7702(Signed::new_unhashed(tx, Signature::test_signature())),
                signer,
            );
            let len = recovered.encode_2718_len();
            crate::MorphPooledTransaction::new(recovered, len)
        };
        assert!(matches!(
            post_validator
                .validate_one(TransactionOrigin::External, make_7702(70_999, Bytes::new())),
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::IntrinsicGasTooLow
            )
        ));
        assert!(matches!(
            post_validator
                .validate_one(TransactionOrigin::External, make_7702(71_000, Bytes::new())),
            TransactionValidationOutcome::Valid { .. }
        ));

        // 2d. Post-Celadon MorphTx V2 with 1 auth + 2,048 non-zero bytes calldata:
        //     standard = 21_000 + 25_000 + 16 * 2_048 = 78_768
        //     EIP-7623 floor = 21_000 + 40 * 2_048 = 102_920
        let make_morph_v2 = |gas_limit: u64| {
            let tx = TxMorph {
                chain_id: 2818,
                nonce: 0,
                gas_limit,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 10,
                to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
                value: U256::ZERO,
                access_list: Default::default(),
                version: 2,
                fee_token_id: 0,
                fee_limit: U256::ZERO,
                reference: None,
                memo: None,
                authorization_list: vec![dummy_auth.clone()],
                input: Bytes::from(vec![0xab; 2_048]),
            };
            let recovered = Recovered::new_unchecked(
                MorphTxEnvelope::Morph(Signed::new_unhashed(tx, Signature::test_signature())),
                signer,
            );
            let len = recovered.encode_2718_len();
            crate::MorphPooledTransaction::new(recovered, len)
        };
        assert!(matches!(
            post_validator.validate_one(TransactionOrigin::External, make_morph_v2(78_767)),
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::IntrinsicGasTooLow
            )
        ));
        assert!(matches!(
            post_validator.validate_one(TransactionOrigin::External, make_morph_v2(80_000)),
            TransactionValidationOutcome::Valid { .. }
        ));

        // 2e. Other stateless errors (e.g., wrong chain ID) keep their exact error variant.
        let outcome = post_validator.validate_one(
            TransactionOrigin::External,
            make_eip1559(
                40_000,
                TxKind::Call(address!("0000000000000000000000000000000000000002")),
                calldata,
                999_999,
            ),
        );
        assert!(matches!(
            outcome,
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::Consensus(InvalidTransactionError::ChainIdMismatch)
            )
        ));
    }

    /// Test provider wrapping `MockEthProvider` with configurable pruned/unreadable block hashes
    /// so M-H05 tests can simulate failed `state_by_block_hash` lookups on orphaned/pruned heads.
    #[derive(Clone, Debug)]
    struct PrunableMockProvider {
        inner: MockEthProvider<MorphPrimitives, MorphChainSpec>,
        pruned_hashes: Arc<parking_lot::Mutex<std::collections::HashSet<B256>>>,
    }

    impl PrunableMockProvider {
        fn new(inner: MockEthProvider<MorphPrimitives, MorphChainSpec>) -> Self {
            Self {
                inner,
                pruned_hashes: Arc::new(parking_lot::Mutex::new(Default::default())),
            }
        }

        fn prune_hash(&self, hash: B256) {
            self.pruned_hashes.lock().insert(hash);
        }
    }

    impl ChainSpecProvider for PrunableMockProvider {
        type ChainSpec = MorphChainSpec;

        fn chain_spec(&self) -> Arc<Self::ChainSpec> {
            self.inner.chain_spec()
        }
    }

    impl reth_storage_api::BlockHashReader for PrunableMockProvider {
        fn block_hash(&self, number: u64) -> reth_provider::ProviderResult<Option<B256>> {
            self.inner.block_hash(number)
        }

        fn canonical_hashes_range(
            &self,
            start: alloy_primitives::BlockNumber,
            end: alloy_primitives::BlockNumber,
        ) -> reth_provider::ProviderResult<Vec<B256>> {
            self.inner.canonical_hashes_range(start, end)
        }
    }

    impl reth_storage_api::BlockNumReader for PrunableMockProvider {
        fn chain_info(&self) -> reth_provider::ProviderResult<reth_chainspec::ChainInfo> {
            self.inner.chain_info()
        }

        fn best_block_number(
            &self,
        ) -> reth_provider::ProviderResult<alloy_primitives::BlockNumber> {
            self.inner.best_block_number()
        }

        fn last_block_number(
            &self,
        ) -> reth_provider::ProviderResult<alloy_primitives::BlockNumber> {
            self.inner.last_block_number()
        }

        fn block_number(
            &self,
            hash: B256,
        ) -> reth_provider::ProviderResult<Option<alloy_primitives::BlockNumber>> {
            self.inner.block_number(hash)
        }
    }

    impl reth_storage_api::BlockIdReader for PrunableMockProvider {
        fn pending_block_num_hash(
            &self,
        ) -> reth_provider::ProviderResult<Option<alloy_eips::BlockNumHash>> {
            self.inner.pending_block_num_hash()
        }

        fn safe_block_num_hash(
            &self,
        ) -> reth_provider::ProviderResult<Option<alloy_eips::BlockNumHash>> {
            self.inner.safe_block_num_hash()
        }

        fn finalized_block_num_hash(
            &self,
        ) -> reth_provider::ProviderResult<Option<alloy_eips::BlockNumHash>> {
            self.inner.finalized_block_num_hash()
        }
    }

    impl reth_storage_api::HeaderProvider for PrunableMockProvider {
        type Header = morph_primitives::MorphHeader;

        fn header(
            &self,
            block_hash: alloy_primitives::BlockHash,
        ) -> reth_provider::ProviderResult<Option<Self::Header>> {
            self.inner.header(block_hash)
        }

        fn header_by_number(
            &self,
            num: u64,
        ) -> reth_provider::ProviderResult<Option<Self::Header>> {
            self.inner.header_by_number(num)
        }

        fn headers_range(
            &self,
            range: impl std::ops::RangeBounds<alloy_primitives::BlockNumber>,
        ) -> reth_provider::ProviderResult<Vec<Self::Header>> {
            self.inner.headers_range(range)
        }

        fn sealed_header(
            &self,
            number: alloy_primitives::BlockNumber,
        ) -> reth_provider::ProviderResult<Option<SealedHeader<Self::Header>>> {
            self.inner.sealed_header(number)
        }

        fn sealed_headers_while(
            &self,
            range: impl std::ops::RangeBounds<alloy_primitives::BlockNumber>,
            predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
        ) -> reth_provider::ProviderResult<Vec<SealedHeader<Self::Header>>> {
            self.inner.sealed_headers_while(range, predicate)
        }
    }

    impl reth_storage_api::BlockBodyIndicesProvider for PrunableMockProvider {
        fn block_body_indices(
            &self,
            num: u64,
        ) -> reth_provider::ProviderResult<Option<reth_db_api::models::StoredBlockBodyIndices>>
        {
            self.inner.block_body_indices(num)
        }

        fn block_body_indices_range(
            &self,
            range: std::ops::RangeInclusive<alloy_primitives::BlockNumber>,
        ) -> reth_provider::ProviderResult<Vec<reth_db_api::models::StoredBlockBodyIndices>>
        {
            self.inner.block_body_indices_range(range)
        }
    }

    impl reth_storage_api::TransactionsProvider for PrunableMockProvider {
        type Transaction = MorphTxEnvelope;

        fn transaction_id(
            &self,
            tx_hash: alloy_primitives::TxHash,
        ) -> reth_provider::ProviderResult<Option<alloy_primitives::TxNumber>> {
            self.inner.transaction_id(tx_hash)
        }

        fn transaction_by_id(
            &self,
            id: alloy_primitives::TxNumber,
        ) -> reth_provider::ProviderResult<Option<Self::Transaction>> {
            self.inner.transaction_by_id(id)
        }

        fn transaction_by_id_unhashed(
            &self,
            id: alloy_primitives::TxNumber,
        ) -> reth_provider::ProviderResult<Option<Self::Transaction>> {
            self.inner.transaction_by_id_unhashed(id)
        }

        fn transaction_by_hash(
            &self,
            hash: alloy_primitives::TxHash,
        ) -> reth_provider::ProviderResult<Option<Self::Transaction>> {
            self.inner.transaction_by_hash(hash)
        }

        fn transaction_by_hash_with_meta(
            &self,
            hash: alloy_primitives::TxHash,
        ) -> reth_provider::ProviderResult<
            Option<(Self::Transaction, reth_primitives_traits::TransactionMeta)>,
        > {
            self.inner.transaction_by_hash_with_meta(hash)
        }

        fn transactions_by_block(
            &self,
            id: alloy_eips::BlockHashOrNumber,
        ) -> reth_provider::ProviderResult<Option<Vec<Self::Transaction>>> {
            self.inner.transactions_by_block(id)
        }

        fn transactions_by_block_range(
            &self,
            range: impl std::ops::RangeBounds<alloy_primitives::BlockNumber>,
        ) -> reth_provider::ProviderResult<Vec<Vec<Self::Transaction>>> {
            self.inner.transactions_by_block_range(range)
        }

        fn transactions_by_tx_range(
            &self,
            range: impl std::ops::RangeBounds<alloy_primitives::TxNumber>,
        ) -> reth_provider::ProviderResult<Vec<Self::Transaction>> {
            self.inner.transactions_by_tx_range(range)
        }

        fn senders_by_tx_range(
            &self,
            range: impl std::ops::RangeBounds<alloy_primitives::TxNumber>,
        ) -> reth_provider::ProviderResult<Vec<Address>> {
            self.inner.senders_by_tx_range(range)
        }

        fn transaction_sender(
            &self,
            id: alloy_primitives::TxNumber,
        ) -> reth_provider::ProviderResult<Option<Address>> {
            self.inner.transaction_sender(id)
        }
    }

    impl reth_storage_api::ReceiptProvider for PrunableMockProvider {
        type Receipt = morph_primitives::MorphReceipt;

        fn receipt(
            &self,
            id: alloy_primitives::TxNumber,
        ) -> reth_provider::ProviderResult<Option<Self::Receipt>> {
            self.inner.receipt(id)
        }

        fn receipt_by_hash(
            &self,
            hash: alloy_primitives::TxHash,
        ) -> reth_provider::ProviderResult<Option<Self::Receipt>> {
            self.inner.receipt_by_hash(hash)
        }

        fn receipts_by_block(
            &self,
            block: alloy_eips::BlockHashOrNumber,
        ) -> reth_provider::ProviderResult<Option<Vec<Self::Receipt>>> {
            self.inner.receipts_by_block(block)
        }

        fn receipts_by_tx_range(
            &self,
            range: impl std::ops::RangeBounds<alloy_primitives::TxNumber>,
        ) -> reth_provider::ProviderResult<Vec<Self::Receipt>> {
            self.inner.receipts_by_tx_range(range)
        }

        fn receipts_by_block_range(
            &self,
            block_range: std::ops::RangeInclusive<alloy_primitives::BlockNumber>,
        ) -> reth_provider::ProviderResult<Vec<Vec<Self::Receipt>>> {
            self.inner.receipts_by_block_range(block_range)
        }
    }

    impl reth_storage_api::ReceiptProviderIdExt for PrunableMockProvider {}

    impl reth_storage_api::BlockReader for PrunableMockProvider {
        type Block = morph_primitives::Block;

        fn find_block_by_hash(
            &self,
            hash: B256,
            source: reth_provider::BlockSource,
        ) -> reth_provider::ProviderResult<Option<Self::Block>> {
            self.inner.find_block_by_hash(hash, source)
        }

        fn block(
            &self,
            id: alloy_eips::BlockHashOrNumber,
        ) -> reth_provider::ProviderResult<Option<Self::Block>> {
            self.inner.block(id)
        }

        fn pending_block(
            &self,
        ) -> reth_provider::ProviderResult<
            Option<reth_primitives_traits::RecoveredBlock<Self::Block>>,
        > {
            self.inner.pending_block()
        }

        fn pending_block_and_receipts(
            &self,
        ) -> reth_provider::ProviderResult<
            Option<(
                reth_primitives_traits::RecoveredBlock<Self::Block>,
                Vec<Self::Receipt>,
            )>,
        > {
            self.inner.pending_block_and_receipts()
        }

        fn recovered_block(
            &self,
            id: alloy_eips::BlockHashOrNumber,
            transaction_kind: reth_provider::TransactionVariant,
        ) -> reth_provider::ProviderResult<
            Option<reth_primitives_traits::RecoveredBlock<Self::Block>>,
        > {
            self.inner.recovered_block(id, transaction_kind)
        }

        fn sealed_block_with_senders(
            &self,
            id: alloy_eips::BlockHashOrNumber,
            transaction_kind: reth_provider::TransactionVariant,
        ) -> reth_provider::ProviderResult<
            Option<reth_primitives_traits::RecoveredBlock<Self::Block>>,
        > {
            self.inner.sealed_block_with_senders(id, transaction_kind)
        }

        fn block_range(
            &self,
            range: std::ops::RangeInclusive<alloy_primitives::BlockNumber>,
        ) -> reth_provider::ProviderResult<Vec<Self::Block>> {
            self.inner.block_range(range)
        }

        fn block_with_senders_range(
            &self,
            range: std::ops::RangeInclusive<alloy_primitives::BlockNumber>,
        ) -> reth_provider::ProviderResult<Vec<reth_primitives_traits::RecoveredBlock<Self::Block>>>
        {
            self.inner.block_with_senders_range(range)
        }

        fn recovered_block_range(
            &self,
            range: std::ops::RangeInclusive<alloy_primitives::BlockNumber>,
        ) -> reth_provider::ProviderResult<Vec<reth_primitives_traits::RecoveredBlock<Self::Block>>>
        {
            self.inner.recovered_block_range(range)
        }

        fn block_by_transaction_id(
            &self,
            id: alloy_primitives::TxNumber,
        ) -> reth_provider::ProviderResult<Option<alloy_primitives::BlockNumber>> {
            self.inner.block_by_transaction_id(id)
        }
    }

    impl BlockReaderIdExt for PrunableMockProvider {
        fn block_by_id(
            &self,
            id: alloy_eips::BlockId,
        ) -> reth_provider::ProviderResult<Option<Self::Block>> {
            self.inner.block_by_id(id)
        }

        fn header_by_number_or_tag(
            &self,
            id: alloy_eips::BlockNumberOrTag,
        ) -> reth_provider::ProviderResult<Option<Self::Header>> {
            self.inner.header_by_number_or_tag(id)
        }

        fn sealed_header_by_number_or_tag(
            &self,
            id: alloy_eips::BlockNumberOrTag,
        ) -> reth_provider::ProviderResult<Option<SealedHeader<Self::Header>>> {
            self.inner.sealed_header_by_number_or_tag(id)
        }

        fn sealed_header_by_id(
            &self,
            id: alloy_eips::BlockId,
        ) -> reth_provider::ProviderResult<Option<SealedHeader<Self::Header>>> {
            self.inner.sealed_header_by_id(id)
        }

        fn header_by_id(
            &self,
            id: alloy_eips::BlockId,
        ) -> reth_provider::ProviderResult<Option<Self::Header>> {
            self.inner.header_by_id(id)
        }
    }

    impl StateProviderFactory for PrunableMockProvider {
        fn latest(&self) -> reth_provider::ProviderResult<StateProviderBox> {
            self.inner.latest()
        }

        fn state_by_block_number_or_tag(
            &self,
            number_or_tag: alloy_eips::BlockNumberOrTag,
        ) -> reth_provider::ProviderResult<StateProviderBox> {
            self.inner.state_by_block_number_or_tag(number_or_tag)
        }

        fn history_by_block_number(
            &self,
            block: alloy_primitives::BlockNumber,
        ) -> reth_provider::ProviderResult<StateProviderBox> {
            self.inner.history_by_block_number(block)
        }

        fn history_by_block_hash(
            &self,
            block: alloy_primitives::BlockHash,
        ) -> reth_provider::ProviderResult<StateProviderBox> {
            if self.pruned_hashes.lock().contains(&block) {
                return Err(reth_provider::ProviderError::BlockHashNotFound(block));
            }
            self.inner.history_by_block_hash(block)
        }

        fn state_by_block_hash(
            &self,
            block: alloy_primitives::BlockHash,
        ) -> reth_provider::ProviderResult<StateProviderBox> {
            if self.pruned_hashes.lock().contains(&block) {
                return Err(reth_provider::ProviderError::BlockHashNotFound(block));
            }
            self.inner.state_by_block_hash(block)
        }

        fn pending(&self) -> reth_provider::ProviderResult<StateProviderBox> {
            self.inner.pending()
        }

        fn pending_state_by_hash(
            &self,
            block_hash: B256,
        ) -> reth_provider::ProviderResult<Option<StateProviderBox>> {
            self.inner.pending_state_by_hash(block_hash)
        }

        fn maybe_pending(&self) -> reth_provider::ProviderResult<Option<StateProviderBox>> {
            self.inner.maybe_pending()
        }
    }

    /// M-H05: A failed `update_l1_block_info` (e.g. transient/orphaned state lookup failure)
    /// must clear the cached head rather than retaining a stale/orphaned block hash, and
    /// subsequent validation must seamlessly fall back to `latest_header()` and refresh.
    #[test]
    fn failed_update_l1_block_info_clears_cached_head_and_falls_back_to_latest() {
        let signer = address!("0000000000000000000000000000000000000001");
        let base = new_mock_provider();
        let header1 = morph_primitives::MorphHeader::from(alloy_consensus::Header {
            number: 1,
            timestamp: 1_767_765_600,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(10),
            ..Default::default()
        });
        base.add_block(
            header1.hash_slow(),
            morph_primitives::Block {
                header: header1.clone(),
                body: Default::default(),
            },
        );
        base.add_account(signer, ExtendedAccount::new(0, U256::from(10u128.pow(18))));

        let client = PrunableMockProvider::new(base.clone());
        let validator = MorphTransactionValidator::new(
            EthTransactionValidatorBuilder::new(
                client.clone(),
                MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
            )
            .disable_balance_check()
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default()),
        );
        assert_eq!(validator.block_info().number(), 1);

        // Simulate a transient/reorg failure while refreshing to an unreadable block 2.
        let orphan_header = morph_primitives::MorphHeader::from(alloy_consensus::Header {
            number: 2,
            timestamp: 1_767_765_602,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(10),
            ..Default::default()
        });
        client.prune_hash(orphan_header.hash_slow());
        // Also prune old block 1 so staying pinned to block 1 would have caused total outage.
        client.prune_hash(header1.hash_slow());

        validator.update_l1_block_info(&orphan_header);
        assert_eq!(
            validator.block_info().number(),
            0,
            "failed update_l1_block_info must clear cached head to None"
        );

        // Publish canonical block 3 in provider without calling on_new_head_block.
        let header3 = morph_primitives::MorphHeader::from(alloy_consensus::Header {
            number: 3,
            timestamp: 1_767_765_604,
            gas_limit: 45_000_000,
            base_fee_per_gas: Some(10),
            ..Default::default()
        });
        base.add_block(
            header3.hash_slow(),
            morph_primitives::Block {
                header: header3,
                body: Default::default(),
            },
        );

        let tx = TxEip1559 {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 40_000_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let recovered = Recovered::new_unchecked(
            MorphTxEnvelope::Eip1559(Signed::new_unhashed(tx, Signature::test_signature())),
            signer,
        );
        let len = recovered.encode_2718_len();
        let outcome = validator.validate_one(
            TransactionOrigin::External,
            crate::MorphPooledTransaction::new(recovered, len),
        );
        assert!(
            matches!(outcome, TransactionValidationOutcome::Valid { .. }),
            "validator must fall back to latest header (block 3, 45M gas): {outcome:?}"
        );
        assert_eq!(validator.block_info().number(), 3);
        assert_eq!(validator.inner.block_gas_limit(), 45_000_000);
    }

    /// M-H05: When cached `head` lags behind canonical tip (e.g. `depth > 64` jump where
    /// upstream maintenance skips `on_new_head_block`) OR points to a pruned block hash,
    /// `current_validation_state` seamlessly refreshes both `MorphL1BlockInfo` and the
    /// inner validator's `block_gas_limit` / `fork_tracker` to the latest canonical head.
    #[test]
    fn stale_or_pruned_cached_head_refreshes_to_latest_canonical_tip() {
        let signer = address!("0000000000000000000000000000000000000001");

        for prune_old_head in [false, true] {
            let base = new_mock_provider();
            // Start at pre-Viridian block 1 (timestamp 0, gas_limit 30M).
            let header1 = morph_primitives::MorphHeader::from(alloy_consensus::Header {
                number: 1,
                timestamp: 0,
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(10),
                ..Default::default()
            });
            base.add_block(
                header1.hash_slow(),
                morph_primitives::Block {
                    header: header1.clone(),
                    body: Default::default(),
                },
            );
            base.add_account(signer, ExtendedAccount::new(0, U256::from(10u128.pow(18))));

            let client = PrunableMockProvider::new(base.clone());
            let validator = MorphTransactionValidator::new(
                EthTransactionValidatorBuilder::new(
                    client.clone(),
                    MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
                )
                .disable_balance_check()
                .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
                .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default()),
            );
            assert_eq!(validator.block_info().number(), 1);
            assert_eq!(validator.inner.block_gas_limit(), 30_000_000);
            assert!(!validator.inner.fork_tracker().is_prague_activated());

            // Simulate a depth > 64 canonical jump to block 100 (post-Celadon, 60M gas limit)
            // WITHOUT invoking `on_new_head_block`.
            let header100 = morph_primitives::MorphHeader::from(alloy_consensus::Header {
                number: 100,
                timestamp: 1_793_772_000, // celadonTime
                gas_limit: 60_000_000,
                base_fee_per_gas: Some(10),
                ..Default::default()
            });
            base.add_block(
                header100.hash_slow(),
                morph_primitives::Block {
                    header: header100,
                    body: Default::default(),
                },
            );
            if prune_old_head {
                client.prune_hash(header1.hash_slow());
            }

            // A post-Celadon, 45M-gas MorphTx V2 (only valid after refreshing BOTH
            // `block_info.head` to block 100 AND `inner.block_gas_limit` to 60M!).
            let tx = TxMorph {
                chain_id: 2818,
                nonce: 0,
                gas_limit: 45_000_000,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 10,
                to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
                value: U256::ZERO,
                access_list: Default::default(),
                version: 2,
                fee_token_id: 0,
                fee_limit: U256::ZERO,
                reference: None,
                memo: None,
                authorization_list: Vec::new(),
                input: Bytes::new(),
            };
            let recovered = Recovered::new_unchecked(
                MorphTxEnvelope::Morph(Signed::new_unhashed(tx, Signature::test_signature())),
                signer,
            );
            let len = recovered.encode_2718_len();
            let outcome = validator.validate_one(
                TransactionOrigin::External,
                crate::MorphPooledTransaction::new(recovered, len),
            );
            assert!(
                matches!(outcome, TransactionValidationOutcome::Valid { .. }),
                "prune_old_head={prune_old_head}: stale head must refresh to block 100: {outcome:?}"
            );
            assert_eq!(validator.block_info().number(), 100);
            assert_eq!(validator.inner.block_gas_limit(), 60_000_000);
            assert!(validator.inner.fork_tracker().is_prague_activated());
        }
    }

    /// M-M08: A validator initialized when the latest canonical header has a higher
    /// `gas_limit` than genesis (e.g. 60M vs 30M) must accept transactions with
    /// `30M < gas_limit <= 60M` immediately on startup without waiting for `on_new_head_block`.
    #[test]
    fn validator_initializes_block_gas_limit_from_latest_header_above_genesis() {
        let signer = address!("0000000000000000000000000000000000000001");
        let client = new_mock_provider();
        assert_eq!(MORPH_MAINNET.inner.genesis().gas_limit, 30_000_000);

        let tip_header = morph_primitives::MorphHeader::from(alloy_consensus::Header {
            number: 10,
            timestamp: 1_767_765_600,
            gas_limit: 60_000_000,
            base_fee_per_gas: Some(10),
            ..Default::default()
        });
        client.add_block(
            tip_header.hash_slow(),
            morph_primitives::Block {
                header: tip_header,
                body: Default::default(),
            },
        );
        client.add_account(signer, ExtendedAccount::new(0, U256::from(10u128.pow(18))));

        // Even when the inner builder starts with default/genesis 30M gas limit,
        // `MorphTransactionValidator::new` syncs to the latest canonical header's 60M limit.
        let validator = MorphTransactionValidator::new(
            EthTransactionValidatorBuilder::new(
                client,
                MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
            )
            .set_block_gas_limit(MORPH_MAINNET.inner.genesis().gas_limit)
            .disable_balance_check()
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default()),
        );
        assert_eq!(validator.inner.block_gas_limit(), 60_000_000);

        let make_tx = |gas_limit: u64| {
            let tx = TxEip1559 {
                chain_id: 2818,
                nonce: 0,
                gas_limit,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 10,
                to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
                value: U256::ZERO,
                access_list: Default::default(),
                input: Bytes::new(),
            };
            let recovered = Recovered::new_unchecked(
                MorphTxEnvelope::Eip1559(Signed::new_unhashed(tx, Signature::test_signature())),
                signer,
            );
            let len = recovered.encode_2718_len();
            crate::MorphPooledTransaction::new(recovered, len)
        };

        // 45M is above genesis 30M and within tip 60M -> must be accepted immediately.
        let outcome = validator.validate_one(TransactionOrigin::External, make_tx(45_000_000));
        assert!(
            matches!(outcome, TransactionValidationOutcome::Valid { .. }),
            "45M gas tx must be accepted immediately at 60M tip: {outcome:?}"
        );

        // 60M + 1 exceeds tip 60M -> must be rejected with ExceedsGasLimit.
        let outcome = validator.validate_one(TransactionOrigin::External, make_tx(60_000_001));
        assert!(
            matches!(
                outcome,
                TransactionValidationOutcome::Invalid(
                    _,
                    InvalidPoolTransactionError::ExceedsGasLimit(60_000_001, 60_000_000)
                )
            ),
            "tx above 60M tip must fail with ExceedsGasLimit: {outcome:?}"
        );
    }

    /// M-M08: End-to-end `Pool::add_external_transaction` test verifying that initializing
    /// `PoolConfig.gas_limit` alongside the validator's `block_gas_limit` allows `30M < gas <= 60M`
    /// transactions to enter `AllTransactions` immediately on startup before any `set_block_info`
    /// / new canonical block arrives (whereas `PoolConfig::default()`'s hardcoded 30M rejects them
    /// at `AllTransactions::insert_tx`).
    #[tokio::test]
    async fn pool_insertion_with_initialized_pool_config_gas_limit_accepts_tx_above_30m() {
        use reth_transaction_pool::{PoolConfig, error::PoolErrorKind};

        let signer = address!("0000000000000000000000000000000000000001");
        let client = new_mock_provider();
        let tip_header = morph_primitives::MorphHeader::from(alloy_consensus::Header {
            number: 10,
            timestamp: 1_767_765_600,
            gas_limit: 60_000_000,
            base_fee_per_gas: Some(10),
            ..Default::default()
        });
        client.add_block(
            tip_header.hash_slow(),
            morph_primitives::Block {
                header: tip_header,
                body: Default::default(),
            },
        );
        client.add_account(signer, ExtendedAccount::new(0, U256::from(10u128.pow(18))));

        let make_validator = || {
            MorphTransactionValidator::new(
                EthTransactionValidatorBuilder::new(
                    client.clone(),
                    MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
                )
                .set_block_gas_limit(60_000_000)
                .disable_balance_check()
                .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default()),
            )
        };
        let make_tx = |gas_limit: u64, nonce: u64| {
            let tx = TxEip1559 {
                chain_id: 2818,
                nonce,
                gas_limit,
                max_fee_per_gas: 1_000_000_000,
                max_priority_fee_per_gas: 1_000_000_000,
                to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
                value: U256::ZERO,
                access_list: Default::default(),
                input: Bytes::new(),
            };
            let recovered = Recovered::new_unchecked(
                MorphTxEnvelope::Eip1559(Signed::new_unhashed(tx, Signature::test_signature())),
                signer,
            );
            let len = recovered.encode_2718_len();
            crate::MorphPooledTransaction::new(recovered, len)
        };

        // Control: with unpatched `PoolConfig::default()` (`gas_limit == 30_000_000`),
        // `AllTransactions::insert_tx` rejects a 45M-gas tx even though `validator` allows 60M.
        let unpatched_pool = Pool::new(
            make_validator(),
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            PoolConfig::default(),
        );
        let err = unpatched_pool
            .add_external_transaction(make_tx(45_000_000, 0))
            .await
            .expect_err("unpatched PoolConfig::default() has 30M gas_limit");
        assert!(
            matches!(
                err.kind,
                PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::ExceedsGasLimit(
                    45_000_000, 30_000_000
                ))
            ),
            "expected AllTransactions 30M rejection, got: {err:?}"
        );

        // Fix: with `pool_config.gas_limit` initialized from latest canonical tip (60M),
        // `AllTransactions::insert_tx` accepts 45M immediately before any block arrives.
        let pool_config = PoolConfig {
            gas_limit: 60_000_000,
            ..PoolConfig::default()
        };
        let pool = Pool::new(
            make_validator(),
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            pool_config,
        );
        assert_eq!(pool.block_info().block_gas_limit, 60_000_000);

        pool.add_external_transaction(make_tx(45_000_000, 0))
            .await
            .expect("45M gas tx must be admitted into pool immediately at 60M tip");
        assert_eq!(pool.pending_transactions().len(), 1);

        let err = pool
            .add_external_transaction(make_tx(60_000_001, 1))
            .await
            .expect_err("tx above 60M tip must still be rejected");
        assert!(matches!(
            err.kind,
            PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::ExceedsGasLimit(
                60_000_001, 60_000_000
            ))
        ));
    }

    /// Verifies that `MorphTransactionValidator`'s direct stateless pipeline preserves every
    /// stateless constraint (`Eip2681`, `OversizedData`, `MaxTxGasLimitExceeded` from both
    /// inner builder and wrapper, `TipAboveFeeCap`, `ExceedsFeeCap`, `PriorityFeeBelowMinimum`,
    /// `MissingEip7702AuthorizationList`, `GasLimitTooHigh`, and pre-EIP-155 legacy chain-ID
    /// handling) without delegating to `inner.validate_stateless`.
    #[test]
    fn morph_stateless_validation_preserves_all_limits_and_error_variants() {
        use alloy_consensus::TxEip7702;

        let signer = address!("0000000000000000000000000000000000000001");
        let client = new_mock_provider();
        let header = morph_primitives::MorphHeader::from(alloy_consensus::Header {
            number: 1,
            timestamp: 1_793_772_000, // post-Celadon
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(10),
            ..Default::default()
        });
        client.add_block(
            header.hash_slow(),
            morph_primitives::Block {
                header,
                body: Default::default(),
            },
        );
        client.add_account(signer, ExtendedAccount::new(0, U256::from(10u128.pow(18))));

        // Build via inner `with_max_tx_gas_limit` alone to verify `inner_max_tx_gas_limit`
        // preserves builder-configured per-tx gas caps even without calling
        // `MorphTransactionValidator::with_max_tx_gas_limit`.
        let validator = MorphTransactionValidator::new(
            EthTransactionValidatorBuilder::new(
                client,
                MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
            )
            .with_max_tx_input_bytes(4_096)
            .with_max_tx_gas_limit(Some(500_000))
            .set_tx_fee_cap(1_000_000)
            .with_minimum_priority_fee(Some(25))
            .disable_balance_check()
            .build::<crate::MorphPooledTransaction, _>(InMemoryBlobStore::default()),
        );
        assert_eq!(validator.max_tx_gas_limit(), Some(500_000));

        // 1,024 non-zero calldata bytes (standard intrinsic 37,384 < gas_limit 40,000 < EIP-7623 floor 61,960)
        let calldata = Bytes::from(vec![0xab; 1_024]);
        let make_1559 = |nonce: u64,
                         gas_limit: u64,
                         max_fee: u128,
                         max_prio: u128,
                         encoded_len: Option<usize>| {
            let tx = TxEip1559 {
                chain_id: 2818,
                nonce,
                gas_limit,
                max_fee_per_gas: max_fee,
                max_priority_fee_per_gas: max_prio,
                to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
                value: U256::ZERO,
                access_list: Default::default(),
                input: calldata.clone(),
            };
            let recovered = Recovered::new_unchecked(
                MorphTxEnvelope::Eip1559(Signed::new_unhashed(tx, Signature::test_signature())),
                signer,
            );
            let len = encoded_len.unwrap_or_else(|| recovered.encode_2718_len());
            crate::MorphPooledTransaction::new(recovered, len)
        };

        // 1. Valid calldata-heavy tx meeting minimum_priority_fee passes externally.
        assert!(matches!(
            validator.validate_one(
                TransactionOrigin::External,
                make_1559(0, 40_000, 100, 25, None)
            ),
            TransactionValidationOutcome::Valid { .. }
        ));

        // 2. EIP-2681 nonce == u64::MAX rejected.
        assert!(matches!(
            validator.validate_one(
                TransactionOrigin::External,
                make_1559(u64::MAX, 40_000, 100, 25, None)
            ),
            TransactionValidationOutcome::Invalid(_, InvalidPoolTransactionError::Eip2681)
        ));

        // 3. OversizedData when encoded_length > max_tx_input_bytes (4,096).
        assert!(matches!(
            validator.validate_one(
                TransactionOrigin::External,
                make_1559(0, 40_000, 100, 25, Some(4_097))
            ),
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::OversizedData {
                    size: 4_097,
                    limit: 4_096
                }
            )
        ));

        // 4. MaxTxGasLimitExceeded when gas_limit > max_tx_gas_limit (500,000).
        assert!(matches!(
            validator.validate_one(
                TransactionOrigin::External,
                make_1559(0, 500_001, 100, 25, None)
            ),
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::MaxTxGasLimitExceeded(500_001, 500_000)
            )
        ));

        // 5. TipAboveFeeCap when max_priority_fee_per_gas > max_fee_per_gas.
        assert!(matches!(
            validator.validate_one(
                TransactionOrigin::External,
                make_1559(0, 40_000, 20, 25, None)
            ),
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::Consensus(InvalidTransactionError::TipAboveFeeCap)
            )
        ));

        // 6. Local ETH tx exceeding tx_fee_cap (40_000 * 30 = 1_200_000 > 1_000_000).
        assert!(matches!(
            validator.validate_one(TransactionOrigin::Local, make_1559(0, 40_000, 30, 10, None)),
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::ExceedsFeeCap {
                    max_tx_fee_wei: 1_200_000,
                    tx_fee_cap_wei: 1_000_000
                }
            )
        ));

        // 7. PriorityFeeBelowMinimum for external dynamic-fee tx (24 < 25), while local is exempt.
        assert!(matches!(
            validator.validate_one(
                TransactionOrigin::External,
                make_1559(0, 40_000, 25, 24, None)
            ),
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::PriorityFeeBelowMinimum {
                    minimum_priority_fee: 25
                }
            )
        ));
        assert!(matches!(
            validator.validate_one(TransactionOrigin::Local, make_1559(0, 40_000, 25, 24, None)),
            TransactionValidationOutcome::Valid { .. }
        ));

        // 8. Pre-EIP-155 Legacy tx (`chain_id: None`) is allowed.
        let legacy = TxLegacy {
            chain_id: None,
            nonce: 0,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            input: Bytes::new(),
            gas_price: 25,
        };
        let legacy_recovered = Recovered::new_unchecked(
            MorphTxEnvelope::Legacy(Signed::new_unhashed(legacy, Signature::test_signature())),
            signer,
        );
        let legacy_len = legacy_recovered.encode_2718_len();
        assert!(matches!(
            validator.validate_one(
                TransactionOrigin::External,
                crate::MorphPooledTransaction::new(legacy_recovered, legacy_len)
            ),
            TransactionValidationOutcome::Valid { .. }
        ));

        // 9. EIP-7702 with empty authorization list -> MissingEip7702AuthorizationList.
        let empty_7702 = TxEip7702 {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 40_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 25,
            to: address!("0000000000000000000000000000000000000002"),
            value: U256::ZERO,
            access_list: Default::default(),
            authorization_list: Vec::new(),
            input: Bytes::new(),
        };
        let empty_7702_recovered = Recovered::new_unchecked(
            MorphTxEnvelope::Eip7702(Signed::new_unhashed(
                empty_7702,
                Signature::test_signature(),
            )),
            signer,
        );
        let empty_7702_len = empty_7702_recovered.encode_2718_len();
        assert!(matches!(
            validator.validate_one(
                TransactionOrigin::External,
                crate::MorphPooledTransaction::new(empty_7702_recovered, empty_7702_len)
            ),
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::Eip7702(
                    Eip7702PoolTransactionError::MissingEip7702AuthorizationList
                )
            )
        ));

        // 10. `tx_gas_limit_cap` enforced on calldata-heavy tx.
        validator
            .inner
            .fork_tracker()
            .tx_gas_limit_cap
            .store(39_999, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            validator.validate_one(
                TransactionOrigin::External,
                make_1559(0, 40_000, 100, 25, None)
            ),
            TransactionValidationOutcome::Invalid(
                _,
                InvalidPoolTransactionError::Consensus(InvalidTransactionError::GasLimitTooHigh)
            )
        ));
    }
}
