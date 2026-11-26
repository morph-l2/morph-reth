//! Transaction pool maintenance tasks for Morph L2.
//!
//! This module provides maintenance tasks for the Morph transaction pool,
//! revalidating L1 fee affordability and MorphTx (0x7F) token balances when the
//! chain state changes.
//!
//! # Background
//!
//! MorphTx allows users to pay gas fees using ERC20 tokens. Since reth's txpool
//! only tracks ETH balance changes (via `SenderInfo`), it cannot automatically
//! demote MorphTx transactions when the token balance decreases. Its transaction
//! cost also excludes L1 data fees, so every sender needs L1 fee revalidation.
//!
//! This maintenance task solves this by:
//! 1. Listening to canonical state changes (new blocks)
//! 2. Re-validating every pooled transaction, including those parked behind a nonce gap,
//!    against the sender's current account balances
//! 3. Removing every transaction with a block-gas-limit, L1 fee, or token-fee violation
//!    that reth cannot handle, letting the pool park surviving descendants
//!
//! # Relationship with reth's own maintenance task
//!
//! This task runs *alongside* [`reth_transaction_pool::maintain::maintain_transaction_pool`],
//! and both subscribe to the canonical state stream independently — there is no ordering
//! guarantee between them. Everything reth's task already understands (ETH balance, nonces,
//! base fee, mined transactions) stays its responsibility. This task also checks the
//! sender's **ERC20 token** balance and the **L1 data fees** missing from reth's cost.
//! Every transaction of a sender at or above the on-chain nonce is revalidated in one pass,
//! matching go-ethereum's full-list filter; removing offending transactions lets reth park
//! any surviving descendants.
//!
//! Because the ordering is not guaranteed, this task must tolerate seeing a pool snapshot
//! that still contains transactions the new block already executed. It does so by reading
//! the sender's on-chain nonce and skipping everything below it, mirroring
//! `AllTransactions::update`, which discards those transactions before any affordability
//! check, and go-ethereum's `demoteUnexecutables`, which calls `list.Forward(nonce)` first.
//!
//! # Reference
//!
//! This is similar to how go-ethereum handles MorphTx in `promoteExecutables`
//! and `demoteUnexecutables` (tx_pool.go), but implemented as a separate
//! maintenance task since we cannot modify reth's internal pool logic.

use crate::{
    MorphPooledTransaction, MorphTxValidationError, MorphValidationState,
    morph_tx_validation::validate_morph_tx_with,
    state_cache::{StateCache, TokenFeeValidator},
};
use alloy_consensus::Transaction;
use alloy_consensus::Typed2718;
use alloy_primitives::{Address, B256, TxHash};
use futures::{FutureExt, StreamExt};
use morph_chainspec::hardfork::MorphHardforks;
use morph_revm::{L1BlockInfo, MorphEvmEnv};
use reth_chainspec::ChainSpecProvider;
use reth_evm::{ConfigureEvm, Database, EvmEnvFor, database::StateProviderDatabase};
use reth_primitives_traits::{AlloyBlockHeader, HeaderTy, NodePrimitives, SealedHeader};
use reth_provider::CanonStateSubscriptions;
use reth_storage_api::{BlockReaderIdExt, StateProvider as _, StateProviderFactory};
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use std::collections::{HashMap, HashSet};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Chain access for the maintenance loop.
///
/// The node reads everything from its provider ([`ProviderFeeState`]). Tests supply a
/// separate state per block, which the mock provider cannot do.
trait FeeStateSource<N: NodePrimitives> {
    /// Opens the state, L1 fee parameters and EVM environment of `header`.
    fn state_for(&self, header: &HeaderTy<N>) -> Result<MorphValidationState, BoxError>;

    /// Returns the current canonical head.
    fn canonical_head(&self) -> Result<Option<SealedHeader<HeaderTy<N>>>, BoxError>;
}

/// [`FeeStateSource`] backed by the node's provider.
struct ProviderFeeState<Client, Evm> {
    client: Client,
    evm_config: Evm,
}

impl<Client, Evm> FeeStateSource<Evm::Primitives> for ProviderFeeState<Client, Evm>
where
    Client: StateProviderFactory + BlockReaderIdExt<Header = HeaderTy<Evm::Primitives>>,
    Evm: ConfigureEvm,
    EvmEnvFor<Evm>: Into<MorphEvmEnv>,
{
    fn state_for(
        &self,
        header: &HeaderTy<Evm::Primitives>,
    ) -> Result<MorphValidationState, BoxError> {
        crate::validator::validation_state_for_header(&self.client, &self.evm_config, header)
    }

    fn canonical_head(&self) -> Result<Option<SealedHeader<HeaderTy<Evm::Primitives>>>, BoxError> {
        Ok(self.client.latest_header()?)
    }
}

fn exceeds_block_gas_limit(tx_gas_limit: u64, block_gas_limit: u64) -> bool {
    tx_gas_limit > block_gas_limit
}

/// Determines which transactions to remove while revalidating all senders' fee affordability.
///
/// Scans every transaction of each sender at or above the sender's on-chain nonce and returns
/// every offending hash in one pass, matching go-ethereum's full-sender filter (`txList.FilterF`).
/// Continuing past ETH-parked or invalid predecessors prevents an earlier parked transaction from
/// shielding invalid higher-nonce transactions that reth would otherwise promote unvalidated once
/// the predecessor is replaced or funded. Surviving descendants of removed transactions are parked
/// automatically by reth's `remove_transactions` (see [`maintain_morph_pool`]).
fn collect_removable_transactions<DB: Database>(
    db: &mut DB,
    l1_block_info: &L1BlockInfo,
    evm_env: &MorphEvmEnv,
    block_gas_limit: u64,
    pool_txs: Vec<&MorphPooledTransaction>,
) -> Vec<TxHash> {
    let hardfork = evm_env.spec;

    // Group by sender and process in nonce order for deterministic removal ordering.
    let mut txs_by_sender: HashMap<Address, Vec<&MorphPooledTransaction>> = HashMap::new();
    for tx in pool_txs {
        txs_by_sender.entry(tx.sender()).or_default().push(tx);
    }

    // Read each sender's on-chain account once before constructing the reusable `TokenFeeValidator`
    // over `&mut *db`. If a sender's account cannot be read due to a transient DB error, skip
    // that sender instead of evicting their transactions.
    let mut sender_accounts: HashMap<Address, evm2::evm::AccountInfo> =
        HashMap::with_capacity(txs_by_sender.len());
    for sender in txs_by_sender.keys() {
        match db.get_account(sender) {
            Ok(account) => {
                sender_accounts.insert(*sender, account.unwrap_or_default());
            }
            Err(err) => {
                tracing::warn!(
                    target: "morph::txpool::maintain",
                    ?sender,
                    ?err,
                    "Failed to get account info; leaving sender's transactions in the pool"
                );
            }
        }
    }

    // Cache registry entries, storage reads, and call-mode `balanceOf` results across all
    // transactions in this round only; a later block builds a fresh cache.
    let cache = StateCache::default();
    let mut token_validator = TokenFeeValidator::new(&cache, db, evm_env.clone());

    let mut to_remove: Vec<TxHash> = Vec::new();

    for (sender, mut sender_txs) in txs_by_sender {
        let Some(account) = sender_accounts.get(&sender) else {
            continue;
        };
        sender_txs.sort_by_key(|tx| Transaction::nonce(*tx));

        for tx in sender_txs {
            let consensus_tx = tx.transaction();

            // Standard maintenance removes mined transactions concurrently; skip any the
            // new block already executed rather than judging them against post-block balances.
            if tx.nonce() < account.nonce {
                continue;
            }

            // A nonce gap or earlier invalid/ETH-parked transaction does not stop the walk.
            // Every check below is per transaction against the sender's current balances,
            // which is what admission applies and what go-ethereum's `promoteExecutables`
            // applies to its whole queue (`FilterF`) before `Ready` promotes anything.
            // Nothing here accumulates the costs of missing predecessors, so every transaction
            // can be judged on its own. Stopping early would leave invalid higher-nonce
            // transactions in the pool for reth to promote unvalidated the moment a
            // predecessor is replaced or funded: `AllTransactions::insert_tx` only recomputes
            // the ETH cost bits, which for a token-fee MorphTx cover its value alone.

            // Reth only sets its block-gas-limit flag at insertion, so a later
            // limit reduction needs the same explicit removal for both tx types.
            if exceeds_block_gas_limit(consensus_tx.gas_limit(), block_gas_limit) {
                to_remove.push(*tx.hash());
                continue;
            }

            let uses_token_fee = consensus_tx.fee_token_id().is_some_and(|id| id > 0);

            // For ETH-paying transactions (ordinary txs and `fee_token_id == 0` MorphTxs),
            // reth knows `tx.cost()` (`gas_limit * max_fee_per_gas + value`) and parks the
            // transaction until a balance update makes it affordable again. Do not remove it
            // here, but keep scanning higher nonces so an ETH-parked predecessor cannot shield
            // invalid descendants.
            //
            // For token-fee MorphTxs (`fee_token_id > 0`), `tx.cost()` is `value` alone: even
            // when reth parks the transaction for an ETH `value` shortfall, we must still
            // validate its token usability and token-fee affordability below so an invalid or
            // unaffordable token transaction is not left parked and later promoted unvalidated
            // when ETH arrives.
            if *tx.cost() > account.balance && !uses_token_fee {
                continue;
            }

            let l1_data_fee = l1_block_info.calculate_tx_l1_cost(tx.encoded_2718(), hardfork);
            if consensus_tx.ty() != morph_primitives::MORPH_TX_TYPE_ID {
                if tx.cost().saturating_add(l1_data_fee) > account.balance {
                    to_remove.push(*tx.hash());
                }
                continue;
            }

            // Validate each MorphTx against the same chain balance used at admission.
            // When a token-fee MorphTx is parked purely due to an ETH `value` shortfall
            // (`tx.value() > account.balance`), raise `eth_balance` to `tx.value()` so
            // `validate_morph_tx_with` does not treat that ETH shortfall as a removal reason
            // (standard maintenance owns ETH `value` parking) while still enforcing hardfork,
            // format, token usability, and token balance checks.
            let eth_balance = if uses_token_fee {
                account.balance.max(consensus_tx.value())
            } else {
                account.balance
            };
            let input = crate::MorphTxValidationInput {
                consensus_tx,
                sender,
                eth_balance,
                l1_data_fee,
                hardfork,
                evm_env,
            };

            match validate_morph_tx_with(&input, |token_id, caller| {
                token_validator.resolve_token_fee_info(token_id, caller)
            }) {
                Ok(_) => {}
                Err(MorphTxValidationError::State(err)) => {
                    tracing::warn!(
                        target: "morph::txpool::maintain",
                        tx_hash = ?tx.hash(),
                        ?sender,
                        ?err,
                        "Could not read token state; leaving MorphTx in the pool"
                    );
                }
                Err(MorphTxValidationError::Invalid(err)) => {
                    tracing::debug!(
                        target: "morph::txpool::maintain",
                        tx_hash = ?tx.hash(),
                        ?sender,
                        ?err,
                        "Removing MorphTx: validation failed"
                    );
                    to_remove.push(*tx.hash());
                }
            }
        }
    }

    to_remove
}

/// Keeps the removal candidates that are still removable at the canonical head.
///
/// A round judges the pool at the block its notification named. By the time it ends, the
/// canonical head can be newer: blocks keep arriving while it runs, and the skip-ahead can
/// stop short of the newest notification. A verdict about an older block should not remove a
/// transaction the head can pay for, so the candidates' senders are judged again at the head
/// and only candidates that fail there too are kept. Anything only the head would remove is
/// left for the round of that head. Nothing is removed if the head cannot be read.
///
/// This narrows the window to the re-check itself; it does not close it. The head can move
/// again during the few milliseconds the re-check takes, and a candidate that block makes
/// payable is still removed. That is the exposure any pool has between reading a state and
/// acting on it, and the sender resolves it by resubmitting.
fn recheck_at_canonical_head<Pool, N, Source>(
    pool: &Pool,
    source: &Source,
    judged_at: B256,
    candidates: Vec<TxHash>,
) -> Vec<TxHash>
where
    Pool: TransactionPool<Transaction = MorphPooledTransaction>,
    N: NodePrimitives,
    Source: FeeStateSource<N>,
{
    if candidates.is_empty() {
        return candidates;
    }
    let head = match source.canonical_head() {
        Ok(Some(head)) => head,
        Ok(None) => {
            tracing::warn!(target: "morph::txpool::maintain", "No canonical head; skipping removals");
            return Vec::new();
        }
        Err(err) => {
            tracing::warn!(target: "morph::txpool::maintain", %err, "Failed to read the canonical head; skipping removals");
            return Vec::new();
        }
    };
    if head.hash() == judged_at {
        return candidates;
    }
    let state = match source.state_for(head.header()) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(target: "morph::txpool::maintain", %err, "Failed to open the canonical head; skipping removals");
            return Vec::new();
        }
    };

    let senders: HashSet<Address> = candidates
        .iter()
        .filter_map(|hash| pool.get(hash))
        .map(|tx| tx.sender())
        .collect();
    let sender_txs: Vec<_> = senders
        .into_iter()
        .flat_map(|sender| pool.get_transactions_by_sender(sender))
        .collect();
    let evm_state_provider = (&state.provider).into_evm_state_provider();
    let mut db = StateProviderDatabase::new(&evm_state_provider);
    let still_removable: HashSet<TxHash> = collect_removable_transactions(
        &mut db,
        &state.head.l1_block_info,
        &state.head.evm_env,
        head.gas_limit(),
        sender_txs.iter().map(|tx| &tx.transaction).collect(),
    )
    .into_iter()
    .collect();

    candidates
        .into_iter()
        .filter(|hash| still_removable.contains(hash))
        .collect()
}

/// Maintains the Morph transaction pool by revalidating L1 fees and token balances.
///
/// This task runs continuously and:
/// - Listens for new canonical blocks
/// - Re-validates MorphTx (0x7F) transactions in the pool
/// - Removes transactions that no longer have sufficient token balance
/// - Re-validates L1 fee affordability for every sender, including ordinary-only senders
/// - Removes every transaction whose block gas limit, L1 fee, or token-fee validation fails,
///   letting the pool park surviving descendants
/// - Judges each transaction on its own across each sender's full nonce sequence at or above
///   the on-chain nonce, including transactions parked behind a nonce gap or ETH shortfall
/// - Re-checks every removal at the canonical head right before applying it
pub async fn maintain_morph_pool<Pool, Client, Evm>(pool: Pool, client: Client, evm_config: Evm)
where
    Pool: TransactionPool<Transaction = MorphPooledTransaction> + Clone,
    Client: ChainSpecProvider<ChainSpec: MorphHardforks>
        + StateProviderFactory
        + BlockReaderIdExt<Header = HeaderTy<Evm::Primitives>>
        + CanonStateSubscriptions
        + Clone
        + 'static,
    Evm: ConfigureEvm<Primitives = <Client as CanonStateSubscriptions>::Primitives>,
    EvmEnvFor<Evm>: Into<MorphEvmEnv>,
{
    let chain_events = client.canonical_state_stream();

    tracing::info!(target: "morph::txpool::maintain", "Starting Morph fee maintenance task");

    maintain_morph_pool_with(pool, ProviderFeeState { client, evm_config }, chain_events).await;
}

/// [`maintain_morph_pool`] with an explicit state source and canonical event stream.
async fn maintain_morph_pool_with<Pool, N, Source, Events>(
    pool: Pool,
    source: Source,
    mut chain_events: Events,
) where
    Pool: TransactionPool<Transaction = MorphPooledTransaction> + Clone,
    N: NodePrimitives,
    Source: FeeStateSource<N>,
    Events: futures::Stream<Item = reth_provider::CanonStateNotification<N>> + Unpin,
{
    loop {
        let Some(mut event) = chain_events.next().await else {
            tracing::debug!(target: "morph::txpool::maintain", "Chain event stream ended");
            break;
        };

        // Skip ahead to the newest queued notification. A round reads each sender's account
        // and any fee-token state, so the chain can advance while we are working; the verdicts
        // this task produces are a pure function of the latest state, which makes every
        // intermediate block wasted work against a stale view of the pool. This is only an
        // optimisation: under tokio's cooperative budget `now_or_never` reports an empty
        // stream after 128 items, so removals are re-checked at the canonical head below.
        while let Some(next) = chain_events.next().now_or_never().flatten() {
            event = next;
        }

        let new_tip = event.tip();
        let block_number = new_tip.number();
        let block_gas_limit = new_tip.gas_limit();

        tracing::trace!(
            target: "morph::txpool::maintain",
            block_number,
            "Processing new block for pool fee validation"
        );

        // Every transaction type is revalidated (ordinary ones for their L1 fee), scanning each
        // sender's full nonce sequence at or above the on-chain nonce.
        let all_txs = pool.all_transactions();
        let pool_txs: Vec<&MorphPooledTransaction> = all_txs
            .pending
            .iter()
            .chain(all_txs.queued.iter())
            .map(|tx| &tx.transaction)
            .collect();

        if pool_txs.is_empty() {
            continue;
        }

        let state = match source.state_for(new_tip.header()) {
            Ok(state) => state,
            Err(err) => {
                tracing::warn!(target: "morph::txpool::maintain", %err, "Failed to prepare fee revalidation state");
                continue;
            }
        };
        let evm_state_provider = (&state.provider).into_evm_state_provider();
        let mut db = StateProviderDatabase::new(&evm_state_provider);
        let l1_block_info = state.head.l1_block_info;
        let evm_env = &state.head.evm_env;

        tracing::trace!(
            target: "morph::txpool::maintain",
            count = pool_txs.len(),
            "Revalidating pooled transaction fees"
        );

        let to_remove = collect_removable_transactions(
            &mut db,
            &l1_block_info,
            evm_env,
            block_gas_limit,
            pool_txs,
        );

        let to_remove = recheck_at_canonical_head(&pool, &source, new_tip.hash(), to_remove);

        // Remove the offending transactions. `remove_transactions` *parks* each removed
        // transaction's descendants instead of deleting them (upstream
        // `remove_transaction_by_hash` calls `park_descendant_transactions`), so a
        // higher-nonce transaction that is still affordable on its own — a plain ETH-fee
        // transaction, say — survives in the queued sub-pool and becomes executable again
        // once a replacement for the removed nonce arrives. go-ethereum's
        // `demoteUnexecutables` does the same thing by re-enqueueing its `invalids`
        // (core/tx_pool.go:1888) rather than dropping them.
        if !to_remove.is_empty() {
            let count = to_remove.len();
            pool.remove_transactions(to_remove);
            tracing::info!(
                target: "morph::txpool::maintain",
                count,
                block_number,
                "Removed transactions during pool fee revalidation"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::morph_tx_validation::tests::{
        BALANCE_OF_RUNTIME, FAILING_BALANCE_OF, TokenRead, UnreadableTokenDb,
        call_mode_registry_storage, call_mode_token_state, test_evm_env, token_balance_key,
    };
    use alloy_consensus::{Signed, TxLegacy, transaction::Recovered};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Bytes, Sealable, Signature, TxKind, U256, address};
    use evm2::bytecode::Bytecode;
    use evm2::evm::{AccountInfo, CacheDB, DynDatabase, EmptyDB, InMemoryDB};
    use morph_chainspec::{MORPH_MAINNET, MorphHardfork};
    use morph_evm::MorphEvmConfig;
    use morph_primitives::{MorphHeader, MorphPrimitives, MorphTxEnvelope, TxMorph};
    use morph_revm::L2_TOKEN_REGISTRY_ADDRESS;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::{
        CoinbaseTipOrdering, Pool, blobstore::InMemoryBlobStore,
        validate::EthTransactionValidatorBuilder,
    };
    use std::sync::Arc;

    const SIGNER: Address = address!("1000000000000000000000000000000000000001");
    const FEE_TOKEN: Address = address!("5300000000000000000000000000000000000042");
    const SECOND_TOKEN: Address = address!("5300000000000000000000000000000000000043");
    const TOKEN_ID: u16 = 1;
    const SECOND_TOKEN_ID: u16 = 2;
    const TX_TOKEN_BUDGET: u64 = 21_000 * 100;

    fn l1_block_info_with_fee(l1_fee: u64) -> L1BlockInfo {
        let scaled = U256::from(l1_fee) * U256::from(1_000_000_000);
        L1BlockInfo {
            l1_base_fee: U256::from(1),
            l1_commit_scalar: scaled,
            calldata_gas: scaled,
            ..Default::default()
        }
    }

    fn token_fee_tx(nonce: u64, token_id: u16, value: U256, gas_limit: u64) -> MorphPooledTransaction {
        let tx = TxMorph {
            chain_id: 2818,
            nonce,
            gas_limit,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value,
            fee_token_id: token_id,
            fee_limit: U256::ZERO,
            ..Default::default()
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unhashed(tx, Signature::test_signature()));
        let recovered = Recovered::new_unchecked(envelope, SIGNER);
        let len = recovered.encode_2718_len();
        MorphPooledTransaction::new(recovered, len)
    }

    fn legacy_tx(nonce: u64, gas_limit: u64, gas_price: u128, value: U256) -> MorphPooledTransaction {
        let tx = TxLegacy {
            chain_id: Some(2818),
            nonce,
            gas_limit,
            gas_price,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value,
            input: Bytes::new(),
        };
        let envelope = MorphTxEnvelope::Legacy(Signed::new_unhashed(tx, Signature::test_signature()));
        let recovered = Recovered::new_unchecked(envelope, SIGNER);
        let len = recovered.encode_2718_len();
        MorphPooledTransaction::new(recovered, len)
    }

    fn register_call_mode_token(
        db: &mut InMemoryDB,
        token_id: u16,
        token: Address,
        token_balance: u64,
        active: bool,
    ) {
        let mut entries = call_mode_registry_storage(token_id, token);
        if !active {
            let mut inactive = [0u8; 32];
            inactive[30] = 18;
            inactive[31] = 0;
            entries[2].1 = U256::from_be_bytes(inactive);
        }
        for (slot, value) in entries {
            db.insert_account_storage(&L2_TOKEN_REGISTRY_ADDRESS, &slot, &value);
        }
        db.insert_account_info(
            &token,
            AccountInfo::default().with_code(Bytecode::new_legacy(Bytes::from_static(
                BALANCE_OF_RUNTIME,
            ))),
        );
        db.insert_account_storage(&token, &token_balance_key(SIGNER), &U256::from(token_balance));
    }

    #[test]
    fn gas_limit_check_rejects_transactions_above_block_limit() {
        assert!(exceeds_block_gas_limit(30_000_001, 30_000_000));
        assert!(!exceeds_block_gas_limit(30_000_000, 30_000_000));
    }

    #[test]
    fn skips_mined_nonces_and_revalidates_ordinary_tx_l1_fee() {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            &SIGNER,
            AccountInfo {
                nonce: 1,
                balance: U256::from(2_100_000u64), // covers tx1 cost (2.1M) alone, NOT + 1M L1 fee
                ..Default::default()
            },
        );
        let tx0_mined = legacy_tx(0, 21_000, 100, U256::ZERO);
        let tx1_unpayable_l1 = legacy_tx(1, 21_000, 100, U256::ZERO);
        let tx2_payable = legacy_tx(2, 21_000, 10, U256::ZERO); // 210k + 1M <= 2.1M

        let to_remove = collect_removable_transactions(
            &mut db,
            &l1_block_info_with_fee(1_000_000),
            &test_evm_env(MorphHardfork::Celadon),
            30_000_000,
            vec![&tx0_mined, &tx1_unpayable_l1, &tx2_payable],
        );

        assert_eq!(to_remove, vec![*tx1_unpayable_l1.hash()]);
    }

    #[test]
    fn eth_parked_and_transient_db_errors_do_not_shield_higher_nonces() {
        for fail_on in [
            TokenRead::Registry,
            TokenRead::TokenAccount,
            TokenRead::TokenStorage,
        ] {
            let mut inner = call_mode_token_state(
                TOKEN_ID,
                FEE_TOKEN,
                SIGNER,
                U256::from(TX_TOKEN_BUDGET),
                BALANCE_OF_RUNTIME,
            );
            // Second token is inactive (only detectable if registry read works).
            register_call_mode_token(&mut inner, SECOND_TOKEN_ID, SECOND_TOKEN, 0, false);
            inner.insert_account_info(
                &SIGNER,
                AccountInfo {
                    nonce: 0,
                    balance: U256::from(2_100_000u64),
                    ..Default::default()
                },
            );

            let mut db = UnreadableTokenDb {
                inner,
                token: FEE_TOKEN,
                fail_on,
            };

            // Nonce 0 triggers transient DB error on FEE_TOKEN -> must NOT be removed.
            let tx0_db_err = token_fee_tx(0, TOKEN_ID, U256::ZERO, 21_000);
            // Nonce 1 is ETH-parked (cost > 2.1M) -> left to reth, must NOT shield nonce 2.
            let tx1_eth_parked = legacy_tx(1, 21_000, 200, U256::ZERO);
            // Nonce 2 fails L1 fee check (2.1M + 1M > 2.1M) -> must be removed!
            let tx2_l1_shortfall = legacy_tx(2, 21_000, 100, U256::ZERO);
            // Nonce 3 is affordable (210k + 1M <= 2.1M) -> stays.
            let tx3_valid = legacy_tx(3, 21_000, 10, U256::ZERO);

            let to_remove = collect_removable_transactions(
                &mut db,
                &l1_block_info_with_fee(1_000_000),
                &test_evm_env(MorphHardfork::Celadon),
                30_000_000,
                vec![&tx0_db_err, &tx1_eth_parked, &tx2_l1_shortfall, &tx3_valid],
            );

            assert_eq!(
                to_remove,
                vec![*tx2_l1_shortfall.hash()],
                "transient {fail_on:?} error or ETH-parked tx must not remove tx0 or shield tx2"
            );
        }
    }

    #[test]
    fn failing_balance_of_removes_morph_tx_without_removing_valid_descendants() {
        for (label, runtime) in FAILING_BALANCE_OF {
            let mut db = call_mode_token_state(
                TOKEN_ID,
                FEE_TOKEN,
                SIGNER,
                U256::from(TX_TOKEN_BUDGET),
                runtime,
            );
            db.insert_account_info(
                &SIGNER,
                AccountInfo {
                    nonce: 0,
                    balance: U256::from(10_000_000u64),
                    ..Default::default()
                },
            );
            let tx0_bad_token = token_fee_tx(0, TOKEN_ID, U256::ZERO, 21_000);
            let tx1_valid_eth = legacy_tx(1, 21_000, 100, U256::ZERO);

            let to_remove = collect_removable_transactions(
                &mut db,
                &L1BlockInfo::default(),
                &test_evm_env(MorphHardfork::Celadon),
                30_000_000,
                vec![&tx0_bad_token, &tx1_valid_eth],
            );
            assert_eq!(to_remove, vec![*tx0_bad_token.hash()], "case {label}");
        }
    }

    #[test]
    fn remove_transactions_parks_surviving_descendant_and_rechecks_canonical_head() {
        type TestPool = Pool<
            crate::MorphTransactionValidator<
                MockEthProvider<MorphPrimitives, morph_chainspec::MorphChainSpec>,
                MorphPooledTransaction,
                MorphEvmConfig,
            >,
            CoinbaseTipOrdering<MorphPooledTransaction>,
            InMemoryBlobStore,
        >;

        let head0 = MorphHeader {
            inner: alloy_consensus::Header {
                number: 1,
                timestamp: 2_000_000_000,
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(1),
                ..Default::default()
            },
            ..Default::default()
        };
        let head1 = MorphHeader {
            inner: alloy_consensus::Header {
                number: 2,
                timestamp: 2_000_000_002,
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(1),
                ..Default::default()
            },
            ..Default::default()
        };

        let make_provider = |token_balance: u64, header: &MorphHeader| {
            let p = MockEthProvider::<MorphPrimitives, _>::new()
                .with_chain_spec((**MORPH_MAINNET).clone())
                .with_genesis_block();
            p.add_block(
                header.hash_slow(),
                morph_primitives::Block::new(header.clone(), Default::default()),
            );
            let registry = ExtendedAccount::new(0, U256::ZERO).extend_storage(
                call_mode_registry_storage(TOKEN_ID, FEE_TOKEN)
                    .into_iter()
                    .map(|(k, v)| (B256::from(k.to_be_bytes::<32>()), v)),
            );
            let token = ExtendedAccount::new(0, U256::ZERO)
                .with_bytecode(Bytes::from_static(BALANCE_OF_RUNTIME))
                .extend_storage([(
                    B256::from(token_balance_key(SIGNER).to_be_bytes::<32>()),
                    U256::from(token_balance),
                )]);
            p.add_account(L2_TOKEN_REGISTRY_ADDRESS, registry);
            p.add_account(FEE_TOKEN, token);
            p.add_account(SIGNER, ExtendedAccount::new(0, U256::from(10_000_000u64)));
            p
        };

        // Start with enough tokens for both txs.
        let p_initial = make_provider(3 * TX_TOKEN_BUDGET, &head0);
        let validator = crate::MorphTransactionValidator::new(
            EthTransactionValidatorBuilder::new(
                p_initial.clone(),
                MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
            )
            .disable_balance_check()
            .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
            .no_shanghai()
            .no_cancun()
            .build::<MorphPooledTransaction, _>(InMemoryBlobStore::default()),
        );
        let pool: TestPool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        );

        // tx0 needs 2*budget in tokens; tx1 needs 1*budget in tokens.
        let tx0 = futures::executor::block_on(pool.add_transaction(
            reth_transaction_pool::TransactionOrigin::Local,
            token_fee_tx(0, TOKEN_ID, U256::ZERO, 42_000),
        ))
        .unwrap()
        .hash;
        let tx1 = futures::executor::block_on(pool.add_transaction(
            reth_transaction_pool::TransactionOrigin::Local,
            token_fee_tx(1, TOKEN_ID, U256::ZERO, 21_000),
        ))
        .unwrap()
        .hash;
        assert_eq!(pool.all_transactions().pending.len(), 2);

        // At judged block (head0) balance dropped to 0, but at canonical head (head1) balance
        // recovered to 1*budget: tx0 must be removed, tx1 must survive parked in queued!
        struct TwoBlockSource {
            states: HashMap<B256, MockEthProvider<MorphPrimitives, morph_chainspec::MorphChainSpec>>,
            head: SealedHeader<MorphHeader>,
        }
        impl FeeStateSource<MorphPrimitives> for TwoBlockSource {
            fn state_for(&self, header: &MorphHeader) -> Result<MorphValidationState, BoxError> {
                let provider = self
                    .states
                    .get(&header.hash_slow())
                    .expect("state for header");
                crate::validator::validation_state_for_header(
                    provider,
                    &MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
                    header,
                )
            }
            fn canonical_head(&self) -> Result<Option<SealedHeader<MorphHeader>>, BoxError> {
                Ok(Some(self.head.clone()))
            }
        }

        let source = TwoBlockSource {
            states: HashMap::from([
                (head0.hash_slow(), make_provider(0, &head0)),
                (head1.hash_slow(), make_provider(TX_TOKEN_BUDGET, &head1)),
            ]),
            head: SealedHeader::seal_slow(head1),
        };

        let sealed_block0 = reth_primitives_traits::RecoveredBlock::new_unhashed(
            morph_primitives::Block::new(head0.clone(), Default::default()),
            Vec::new(),
        );
        let chain = Arc::new(reth_provider::Chain::new(
            vec![sealed_block0],
            Default::default(),
            Default::default(),
        ));
        let event = reth_provider::CanonStateNotification::Commit { new: chain };

        futures::executor::block_on(maintain_morph_pool_with(
            pool.clone(),
            source,
            futures::stream::iter([event]),
        ));

        assert!(pool.get(&tx0).is_none(), "unaffordable tx0 must be removed");
        assert!(
            pool.get(&tx1).is_some(),
            "tx1 payable at canonical head must survive parked in queued"
        );
        assert_eq!(pool.all_transactions().pending.len(), 0);
        assert_eq!(pool.all_transactions().queued.len(), 1);
    }

    // Ensure `DynDatabase` import is exercised even when tests are trimmed.
    #[test]
    fn counting_db_reuses_cache_per_round() {
        struct CountingDb {
            inner: InMemoryDB,
            reads: HashMap<Address, usize>,
        }
        impl Database for CountingDb {
            type Error = std::convert::Infallible;
            fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
                Ok(DynDatabase::get_account(&mut self.inner, address).unwrap())
            }
            fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
                Ok(DynDatabase::get_code_by_hash(&mut self.inner, code_hash).unwrap())
            }
            fn get_storage(&mut self, address: &Address, index: &U256) -> Result<U256, Self::Error> {
                *self.reads.entry(*address).or_default() += 1;
                Ok(DynDatabase::get_storage(&mut self.inner, address, index).unwrap())
            }
            fn get_block_hash(&mut self, number: &U256) -> Result<B256, Self::Error> {
                Ok(DynDatabase::get_block_hash(&mut self.inner, number).unwrap())
            }
        }

        let mut inner = call_mode_token_state(
            TOKEN_ID,
            FEE_TOKEN,
            SIGNER,
            U256::from(TX_TOKEN_BUDGET),
            BALANCE_OF_RUNTIME,
        );
        inner.insert_account_info(&SIGNER, AccountInfo::default());
        let mut db = CountingDb {
            inner,
            reads: HashMap::new(),
        };
        let txs = [
            token_fee_tx(0, TOKEN_ID, U256::ZERO, 21_000),
            token_fee_tx(1, TOKEN_ID, U256::ZERO, 21_000),
            token_fee_tx(2, TOKEN_ID, U256::ZERO, 21_000),
        ];

        assert!(
            collect_removable_transactions(
                &mut db,
                &L1BlockInfo::default(),
                &test_evm_env(MorphHardfork::Celadon),
                30_000_000,
                txs.iter().collect(),
            )
            .is_empty()
        );
        assert_eq!(db.reads[&L2_TOKEN_REGISTRY_ADDRESS], 5);
        assert_eq!(db.reads[&FEE_TOKEN], 1);
    }
}
