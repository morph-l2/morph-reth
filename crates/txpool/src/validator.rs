//! Transaction validator for Morph L2.
//!
//! This module provides Morph-specific transaction validation that extends the standard
//! Ethereum transaction validation with L2 checks:
//! - Rejection of EIP-4844 blob transactions
//! - EIP-3860 max initcode size enforcement
//! - Rejection of L1 message transactions from the pool
//! - L1 data fee validation
//! - MorphTx (0x7F) ERC20 token balance validation

use crate::MorphTxError;
use alloy_consensus::{BlockHeader, Sealable, Transaction};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_primitives::{Address, B256, U256};
use morph_chainspec::hardfork::{MorphHardfork, MorphHardforks};
use morph_primitives::MorphTxEnvelope;
use morph_revm::{L1BlockInfo, MorphBlockEnv, MorphEvmEnv};
use parking_lot::RwLock;
use reth_chainspec::ChainSpecProvider;
use reth_evm::{ConfigureEvm, EvmFactory, EvmFactoryFor};
use reth_primitives_traits::{
    Block, BlockTy, GotExpected, HeaderTy, SealedBlock, transaction::error::InvalidTransactionError,
};
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::{BlockReaderIdExt, StateProviderBox, StateProviderFactory};
use reth_transaction_pool::{
    EthPoolTransaction, EthTransactionValidator, PoolTransaction, TransactionOrigin,
    TransactionValidationOutcome, TransactionValidator, error::InvalidPoolTransactionError,
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
struct MorphValidationHead {
    hash: B256,
    number: u64,
    timestamp: u64,
    base_fee_per_gas: Option<u64>,
    l1_block_info: L1BlockInfo,
    evm_env: MorphEvmEnv,
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
    /// Creates an uninitialized tracker. Validation returns an error until a head is published.
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
    head: Arc<MorphValidationHead>,
    provider: StateProviderBox,
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
    // Pins the cached environment to Morph's, so the fee-token balance query runs in
    // exactly what the execution layer would use.
    EvmFactoryFor<Evm>: EvmFactory<Spec = MorphHardfork, BlockEnv = MorphBlockEnv>,
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
        }
    }

    /// Update the L1 block info for the given header.
    pub fn update_l1_block_info(&self, header: &HeaderTy<Evm::Primitives>) {
        let evm_env = match self.inner.evm_config().evm_env(header) {
            Ok(evm_env) => evm_env,
            Err(err) => {
                tracing::warn!(target: "morph::txpool", %err, "Failed to build EVM env for head block");
                return;
            }
        };

        let provider = match self.client().state_by_block_hash(header.hash_slow()) {
            Ok(provider) => provider,
            Err(err) => {
                tracing::warn!(target: "morph::txpool", %err, "Failed to get state provider for L1 block info update");
                return;
            }
        };

        let mut db = StateProviderDatabase::new(provider);
        let hardfork = self
            .chain_spec()
            .morph_hardfork_at(header.number(), header.timestamp());

        match L1BlockInfo::try_fetch(&mut db, hardfork) {
            Ok(l1_block_info) => {
                self.block_info.update(l1_block_info, header, evm_env);
            }
            Err(err) => {
                tracing::warn!(target: "morph::txpool", ?err, "Failed to fetch L1 block info");
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
    /// When `state` is `None`, validation pins the current complete head and opens its
    /// provider by block hash. Both are reused for subsequent transactions in the batch.
    /// Reset `state` to `None` to select a newer head.
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

        let head = state
            .as_ref()
            .map(|state| state.head.clone())
            .or_else(|| self.block_info.head.read().clone());
        let Some(head) = head else {
            return morph_tx_validation_outcome(
                transaction,
                MorphTxError::TokenInfoFetchFailed {
                    token_id: None,
                    message: "fee-validation head is not initialized".into(),
                },
            );
        };

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
        if state.is_none() {
            let provider = match self.client().state_by_block_hash(head.hash) {
                Ok(provider) => provider,
                Err(err) => {
                    return TransactionValidationOutcome::Error(*transaction.hash(), Box::new(err));
                }
            };
            *state = Some(MorphValidationState {
                head: head.clone(),
                provider,
            });
        }
        let state = state.as_ref().expect("validation state initialized above");
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
        state: &MorphValidationState,
    ) -> Result<crate::MorphTxValidationResult, MorphTxError> {
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
        self.inner.on_new_head_block(new_tip_block);
        self.update_l1_block_info(new_tip_block.header());
    }
}

/// Maps a [`MorphTxError`] onto the right validation outcome.
///
/// [`TransactionValidationOutcome::Invalid`] is a verdict on the transaction: the pool
/// records it as known-bad and the network layer holds the peer that sent it responsible.
/// A failed state read is not such a verdict — the transaction may be perfectly valid and
/// simply could not be checked — so it is reported as
/// [`TransactionValidationOutcome::Error`], which discards this attempt without blaming
/// anyone and leaves the sender free to try again.
fn morph_tx_validation_outcome<Tx: EthPoolTransaction>(
    transaction: Tx,
    err: MorphTxError,
) -> TransactionValidationOutcome<Tx> {
    if matches!(err, MorphTxError::TokenInfoFetchFailed { .. }) {
        return TransactionValidationOutcome::Error(*transaction.hash(), Box::new(err));
    }
    TransactionValidationOutcome::Invalid(transaction, err.into())
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
    use alloy_consensus::{Sealable, Signed, TxEip1559, TxLegacy};
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

    type TokenFeeValidator = MorphTransactionValidator<
        MockEthProvider<MorphPrimitives, MorphChainSpec>,
        crate::MorphPooledTransaction,
        MorphEvmConfig,
    >;

    /// Registered token with a 1:1 price ratio at an Emerald-active head.
    fn token_fee_validator(
        eth_balance: U256,
        token_balance: U256,
        fee_cap: u128,
        local_config: LocalTransactionConfig,
    ) -> TokenFeeValidator {
        let client = new_mock_provider();
        let signer = address!("0000000000000000000000000000000000000001");
        let token = address!("5300000000000000000000000000000000000042");
        let balance_slot = U256::from(7);
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
        client.add_account(
            L2_TOKEN_REGISTRY_ADDRESS,
            token_registry_account(1, token, balance_slot, token_balance),
        );
        client.add_account(
            token,
            ExtendedAccount::new(0, U256::ZERO).extend_storage([(
                storage_key(compute_mapping_slot_for_address(balance_slot, signer)),
                token_balance,
            )]),
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
        let base = compute_mapping_slot(U256::from(151), &token_id_key(1));
        client.add_account(
            L2_TOKEN_REGISTRY_ADDRESS,
            token_registry_account(1, token, U256::from(7), U256::ZERO)
                .extend_storage([(storage_key(base + U256::from(1)), U256::ZERO)]),
        );
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
    fn a_batch_keeps_its_balance_query_environment_across_a_same_height_reorg() {
        let validator = timestamp_sensitive_validator();
        let client = validator.client();
        let tx = token_fee_transaction(0, U256::ZERO);
        let mut state = None;
        let first =
            validator.validate_one_with_state(TransactionOrigin::Local, tx.clone(), &mut state);
        assert!(
            matches!(first, TransactionValidationOutcome::Valid { .. }),
            "{first:?}"
        );

        let replacement = replacement_header();
        client.add_block(
            replacement.hash_slow(),
            morph_primitives::Block {
                header: replacement.clone(),
                body: Default::default(),
            },
        );
        validator.update_l1_block_info(&replacement);

        let in_batch =
            validator.validate_one_with_state(TransactionOrigin::Local, tx.clone(), &mut state);
        assert!(
            matches!(in_batch, TransactionValidationOutcome::Valid { .. }),
            "a batch cannot mix its old state with the replacement head's environment: {in_batch:?}"
        );
        let fresh = validator.validate_one(TransactionOrigin::Local, tx);
        assert!(
            matches!(fresh, TransactionValidationOutcome::Invalid(..)),
            "a new batch must use the replacement head: {fresh:?}"
        );
    }

    #[test]
    fn a_head_published_during_validation_does_not_change_the_balance_query() {
        let mut validator = timestamp_sensitive_validator();
        let block_info = validator.block_info().clone();
        let replacement = replacement_header();
        let env = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone())
            .evm_env(&replacement)
            .unwrap();
        // This extension runs after the account read and before Morph fee validation,
        // forcing the interleaving of a concurrent canonical-head publication.
        validator
            .inner
            .set_additional_stateful_validation(move |_, _, _| {
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
            MorphTxError::TokenInfoFetchFailed {
                token_id: None,
                message: "provider unavailable".to_string(),
            },
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
            MorphTxError::TokenNotActive { token_id: 1 },
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
}
