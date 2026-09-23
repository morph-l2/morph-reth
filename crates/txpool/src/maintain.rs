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
//! 2. Re-validating each sender's contiguous nonce sequence against current account balances
//! 3. Removing the first transaction with an L1 fee or token shortfall that reth cannot
//!    handle, letting the pool park its descendants
//!
//! # Relationship with reth's own maintenance task
//!
//! This task runs *alongside* [`reth_transaction_pool::maintain::maintain_transaction_pool`],
//! and both subscribe to the canonical state stream independently — there is no ordering
//! guarantee between them. Everything reth's task already understands (ETH balance, nonces,
//! base fee, mined transactions) stays its responsibility. This task also checks the
//! sender's **ERC20 token** balance and the **L1 data fees** missing from reth's cost.
//! An ordinary transaction that cannot cover L1 fees is
//! removed so its descendants are parked instead of remaining unchecked in pending.
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

use crate::{MorphPooledTransaction, MorphTxValidationError, MorphValidationState};
use alloy_consensus::Transaction;
use alloy_consensus::Typed2718;
use alloy_primitives::{Address, B256, TxHash};
use futures::{FutureExt, StreamExt};
use morph_chainspec::hardfork::{MorphHardfork, MorphHardforks};
use morph_revm::{L1BlockInfo, MorphBlockEnv, MorphEvmEnv};
use reth_chainspec::ChainSpecProvider;
use reth_evm::{ConfigureEvm, EvmFactory, EvmFactoryFor};
use reth_primitives_traits::{AlloyBlockHeader, HeaderTy, NodePrimitives, SealedHeader};
use reth_provider::CanonStateSubscriptions;
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};
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
    EvmFactoryFor<Evm>: EvmFactory<Spec = MorphHardfork, BlockEnv = MorphBlockEnv>,
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
/// Returns the hashes to remove from the pool. Only the first offending transaction of a
/// sender is returned: the pool parks the rest of that sender's transactions on its own when
/// the returned hash is removed (see [`maintain_morph_pool`]).
fn collect_removable_transactions<DB: alloy_evm::Database>(
    db: &mut DB,
    l1_block_info: &L1BlockInfo,
    evm_env: &MorphEvmEnv,
    block_gas_limit: u64,
    pool_txs: Vec<&MorphPooledTransaction>,
) -> Vec<TxHash> {
    let hardfork = *evm_env.cfg_env.spec();
    // These caches live for exactly this provider/environment snapshot. Cache successful
    // reads only; a later block must re-read changed registry parameters and balances.
    let mut token_entries = HashMap::new();
    let mut token_balances = HashMap::new();
    // Group by sender and process in nonce order so removing a transaction parks its descendants.
    let mut txs_by_sender: HashMap<Address, Vec<&MorphPooledTransaction>> = HashMap::new();
    for tx in pool_txs {
        txs_by_sender.entry(tx.sender()).or_default().push(tx);
    }

    let mut to_remove: Vec<TxHash> = Vec::new();

    for (sender, mut sender_txs) in txs_by_sender {
        sender_txs.sort_by_key(|tx| tx.transaction().nonce());

        // Read one account per sender. Affordability is per transaction, matching
        // admission and geth; cumulative ETH parking remains owned by reth.
        let account = match db.basic(sender) {
            Ok(account) => account.unwrap_or_default(),
            Err(err) => {
                tracing::warn!(
                    target: "morph::txpool::maintain",
                    ?sender,
                    ?err,
                    "Failed to get account info; skipping sender"
                );
                continue;
            }
        };

        // The nonce the next executable transaction of this sender must carry.
        let mut next_nonce_in_line = account.nonce;

        for tx in sender_txs {
            // Access the consensus tx by reference (via Deref chain) instead of
            // cloning. Use the pool tx's cached EIP-2718 encoding for L1 fee.
            let consensus_tx = tx.transaction();

            // Already executed by the new block. reth's own maintenance task removes these
            // when it applies the same canonical update; both tasks subscribe to the
            // canonical stream independently, so this one can still observe them here.
            // Charging them would consume a budget the sender no longer owes and strand the
            // sender's next, genuinely affordable transaction.
            if consensus_tx.nonce() < account.nonce {
                continue;
            }

            // Nonce gap: the transactions filling it are not in the pool, so how much of
            // this sender's balance is still owed by the time this one executes is unknown,
            // and nothing from here on is executable anyway. Upstream's
            // `AllTransactions::update` short-circuits the sender on a gap for the same
            // reason, and go-ethereum only ever applies a per-transaction cost check to its
            // queue, never a cumulative one. Anything left behind the gap sits in the queued
            // sub-pool, where reth's own stale eviction reaps it.
            if consensus_tx.nonce() != next_nonce_in_line {
                break;
            }
            next_nonce_in_line = next_nonce_in_line.saturating_add(1);

            // Reth only sets its block-gas-limit flag at insertion, so a later
            // limit reduction needs the same explicit removal for both tx types.
            if exceeds_block_gas_limit(consensus_tx.gas_limit(), block_gas_limit) {
                to_remove.push(*tx.hash());
                break;
            }

            // Reth knows this ETH cost (only value for token-fee MorphTx) and can
            // park the transaction until a balance update makes it affordable again.
            if *tx.cost() > account.balance {
                break;
            }

            let l1_data_fee = l1_block_info.calculate_tx_l1_cost(tx.encoded_2718(), hardfork);
            if consensus_tx.ty() != morph_primitives::MORPH_TX_TYPE_ID {
                if tx.cost().saturating_add(l1_data_fee) > account.balance {
                    to_remove.push(*tx.hash());
                    break;
                }
                continue;
            }

            // Validate each transaction against the same chain balance used at admission.
            let input = crate::MorphTxValidationInput {
                consensus_tx,
                sender,
                eth_balance: account.balance,
                l1_data_fee,
                hardfork,
                evm_env,
            };

            match crate::morph_tx_validation::validate_morph_tx_with_token_info(
                &input,
                |token_id| {
                    use morph_revm::TokenRegistryEntry;
                    let entry = match token_entries.entry(token_id) {
                        std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            *entry.insert(TokenRegistryEntry::load(db, token_id)?)
                        }
                    };
                    let Some(entry) = entry else {
                        return Ok(None);
                    };
                    let info = match token_balances.entry((sender, token_id)) {
                        std::collections::hash_map::Entry::Occupied(info) => *info.get(),
                        std::collections::hash_map::Entry::Vacant(info) => {
                            *info.insert(entry.load_for_caller(db, sender, evm_env)?)
                        }
                    };
                    Ok(Some(info))
                },
            ) {
                Ok(_) => {}
                Err(MorphTxValidationError::State(err)) => {
                    tracing::warn!(
                        target: "morph::txpool::maintain",
                        tx_hash = ?tx.hash(),
                        ?sender,
                        ?err,
                        "Could not read token state; leaving sender's MorphTx in the pool"
                    );
                    break;
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
                    break;
                }
            };
        }
    }

    to_remove
}

/// Keeps the removal candidates that are still removable at the canonical head.
///
/// A round judges the pool at the block its notification named. By the time it ends, the
/// canonical head can be newer: blocks keep arriving while it runs, and the skip-ahead can
/// stop short of the newest notification. A verdict about an older block must not remove a
/// transaction the head can pay for, so the candidates' senders are judged again at the head
/// and only candidates that fail there too are kept. Anything only the head would remove is
/// left for the round of that head. Nothing is removed if the head cannot be read.
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
    let mut db = StateProviderDatabase::new(state.provider);
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
/// - Removes ordinary transactions whose L1 fees make them individually unaffordable,
///   parking their descendants
/// - Re-checks every removal at the canonical head right before applying it
///
pub async fn maintain_morph_pool<Pool, Client, Evm>(pool: Pool, client: Client, evm_config: Evm)
where
    Pool: TransactionPool<Transaction = MorphPooledTransaction> + Clone,
    Client: ChainSpecProvider<ChainSpec: MorphHardforks>
        + StateProviderFactory
        + BlockReaderIdExt<Header = HeaderTy<Evm::Primitives>>
        + CanonStateSubscriptions
        + Clone
        + 'static,
    Evm: ConfigureEvm<Primitives = <Client as reth_provider::NodePrimitivesProvider>::Primitives>,
    EvmFactoryFor<Evm>: EvmFactory<Spec = MorphHardfork, BlockEnv = MorphBlockEnv>,
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

        // Preserve each sender's complete nonce sequence, including ordinary ETH-fee
        // transactions between MorphTx. Filtering first would create false nonce gaps.
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
        let mut db = StateProviderDatabase::new(state.provider);
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
    use alloy_primitives::U256;

    #[test]
    fn gas_limit_check_rejects_transactions_above_block_limit() {
        assert!(exceeds_block_gas_limit(30_000_001, 30_000_000));
        assert!(!exceeds_block_gas_limit(30_000_000, 30_000_000));
    }

    // ---------------------------------------------------------------------------------
    // Revalidation round tests
    //
    // These drive `collect_removable_transactions` against a hand-built state so the
    // removal verdict can be asserted without a pool, and one pool-level test covers the
    // descendant handling that only the pool can show.
    // ---------------------------------------------------------------------------------

    use crate::morph_tx_validation::tests::{
        BALANCE_OF_RUNTIME, FAILING_BALANCE_OF, TokenRead, UnreadableTokenDb,
        call_mode_registry_storage, call_mode_token_state, token_balance_key,
    };
    use alloy_consensus::{Signed, transaction::Recovered};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Signature, TxKind, address};
    use morph_primitives::{MorphTxEnvelope, TxMorph};
    use morph_revm::L2_TOKEN_REGISTRY_ADDRESS;
    use reth_revm::revm;
    use reth_revm::revm::database::{CacheDB, EmptyDB};
    use reth_revm::revm::state::AccountInfo;

    const SIGNER: Address = address!("0000000000000000000000000000000000000001");
    const FEE_TOKEN: Address = address!("5300000000000000000000000000000000000042");
    const TOKEN_ID: u16 = 1;
    /// `gas_limit * max_fee_per_gas` of [`token_fee_tx`]; at a 1:1 price ratio this is also
    /// the per-transaction token requirement at admission and revalidation.
    const TX_TOKEN_BUDGET: u64 = 21_000 * 100;

    /// State with [`TOKEN_ID`] registered as an active call-mode fee token at a 1:1 price
    /// ratio, whose `balanceOf` reports `token_balance` for [`SIGNER`].
    fn test_state(account_nonce: u64, eth_balance: u64, token_balance: u64) -> CacheDB<EmptyDB> {
        test_state_with_token_code(
            account_nonce,
            eth_balance,
            token_balance,
            BALANCE_OF_RUNTIME,
        )
    }

    /// [`test_state`] with the fee token's `balanceOf` replaced by `code`.
    fn test_state_with_token_code(
        account_nonce: u64,
        eth_balance: u64,
        token_balance: u64,
        code: &'static [u8],
    ) -> CacheDB<EmptyDB> {
        let mut db =
            call_mode_token_state(TOKEN_ID, FEE_TOKEN, code, SIGNER, U256::from(token_balance));
        db.insert_account_info(
            SIGNER,
            AccountInfo {
                nonce: account_nonce,
                balance: U256::from(eth_balance),
                ..Default::default()
            },
        );
        db
    }

    /// A token-fee MorphTx requiring [`TX_TOKEN_BUDGET`] tokens and no ETH.
    fn token_fee_tx(tx_nonce: u64) -> MorphPooledTransaction {
        token_fee_tx_with_value(tx_nonce, U256::ZERO)
    }

    fn token_fee_tx_with_value(tx_nonce: u64, value: U256) -> MorphPooledTransaction {
        let tx = TxMorph {
            chain_id: 2818,
            nonce: tx_nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value,
            fee_token_id: TOKEN_ID,
            fee_limit: U256::ZERO,
            ..Default::default()
        };
        let recovered = Recovered::new_unchecked(
            MorphTxEnvelope::Morph(Signed::new_unhashed(tx, Signature::test_signature())),
            SIGNER,
        );
        let encoded_len = recovered.encode_2718_len();
        MorphPooledTransaction::new(recovered, encoded_len)
    }

    /// The environment the revalidation round is evaluated in.
    fn test_evm_env() -> MorphEvmEnv {
        MorphEvmEnv::new(
            reth_revm::revm::context::CfgEnv::new_with_spec(MorphHardfork::Emerald),
            MorphBlockEnv::default(),
        )
    }

    fn removable(db: &mut CacheDB<EmptyDB>, txs: Vec<&MorphPooledTransaction>) -> Vec<TxHash> {
        collect_removable_transactions(
            db,
            &L1BlockInfo::default(),
            &test_evm_env(),
            30_000_000,
            txs,
        )
    }

    #[test]
    fn transactions_already_executed_by_the_block_are_skipped_not_taken_for_a_gap() {
        // The new block executed nonce 0, but this task can still see it: reth's own maintenance
        // removes it on the same notification, in no guaranteed order. Reading it as a nonce gap
        // would end the walk before nonce 1, which the post-state can no longer pay for.
        let mut db = test_state(1, 0, TX_TOKEN_BUDGET - 1);
        let (tx0, tx1) = (token_fee_tx(0), token_fee_tx(1));

        assert_eq!(removable(&mut db, vec![&tx0, &tx1]), vec![*tx1.hash()]);
    }

    #[test]
    fn individually_affordable_token_transactions_are_retained() {
        // Each transaction passes admission against the same account balance.
        // Maintenance must not evict one merely because their maximum costs add up.
        let mut db = test_state(0, 0, TX_TOKEN_BUDGET + TX_TOKEN_BUDGET / 2);
        let (tx0, tx1) = (token_fee_tx(0), token_fee_tx(1));

        assert!(removable(&mut db, vec![&tx0, &tx1]).is_empty());
    }

    #[test]
    fn an_ordinary_transaction_between_morph_txs_does_not_hide_the_successor() {
        let mut db = test_state(0, 10_000_000, TX_TOKEN_BUDGET - 1);
        let (first, middle, last) = (legacy_tx(0), legacy_tx(1), token_fee_tx(2));
        assert_eq!(
            removable(&mut db, vec![&first, &middle, &last]),
            vec![*last.hash()]
        );
    }

    #[test]
    fn ordinary_predecessors_do_not_cause_a_morph_tx_value_to_be_evicted() {
        let mut db = test_state(0, TX_TOKEN_BUDGET + 6, 10 * TX_TOKEN_BUDGET);
        let (first, last) = (legacy_tx(0), token_fee_tx_with_value(1, U256::from(7)));
        assert!(removable(&mut db, vec![&first, &last]).is_empty());
    }

    #[test]
    fn morph_eth_shortfalls_remain_owned_by_standard_maintenance() {
        let mut db = test_state(0, 6, 10 * TX_TOKEN_BUDGET);
        let tx = token_fee_tx_with_value(0, U256::from(7));
        assert!(removable(&mut db, vec![&tx]).is_empty());
    }

    #[test]
    fn block_gas_limit_decreases_remove_both_transaction_types() {
        for tx in [legacy_tx(0), token_fee_tx(0)] {
            let mut db = test_state(0, 10_000_000, 10 * TX_TOKEN_BUDGET);
            assert_eq!(
                collect_removable_transactions(
                    &mut db,
                    &L1BlockInfo::default(),
                    &test_evm_env(),
                    20_000,
                    vec![&tx]
                ),
                vec![*tx.hash()]
            );
        }
    }

    #[test]
    fn unaffordable_ordinary_predecessors_remain_owned_by_standard_maintenance() {
        let mut db = test_state(0, 0, 10 * TX_TOKEN_BUDGET);
        let (first, last) = (legacy_tx(0), token_fee_tx(1));
        assert!(removable(&mut db, vec![&first, &last]).is_empty());
    }

    #[test]
    fn transactions_behind_nonce_gaps_are_left_queued() {
        // Future nonces stay queued; missing predecessors may alter fee balances.
        let mut db = test_state(0, 0, TX_TOKEN_BUDGET);
        let (tx0, gapped) = (token_fee_tx(0), token_fee_tx(10));

        assert!(
            removable(&mut db, vec![&tx0, &gapped]).is_empty(),
            "transactions behind a gap are left to queued-pool maintenance"
        );
    }

    #[test]
    fn a_sender_holding_only_future_nonces_is_left_alone() {
        // Nothing this sender holds is executable at the current state nonce, so there is no
        // executable front to evaluate — not even for a sender that now holds no tokens.
        let mut db = test_state(0, 0, 0);
        let gapped = token_fee_tx(5);

        assert!(removable(&mut db, vec![&gapped]).is_empty());
    }

    #[derive(Debug)]
    struct CountingDb {
        inner: CacheDB<EmptyDB>,
        reads: HashMap<Address, usize>,
    }

    impl revm::Database for CountingDb {
        type Error = core::convert::Infallible;
        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            self.inner.basic(address)
        }
        fn code_by_hash(
            &mut self,
            hash: alloy_primitives::B256,
        ) -> Result<revm::state::Bytecode, Self::Error> {
            self.inner.code_by_hash(hash)
        }
        fn storage(&mut self, address: Address, slot: U256) -> Result<U256, Self::Error> {
            *self.reads.entry(address).or_default() += 1;
            self.inner.storage(address, slot)
        }
        fn block_hash(&mut self, number: u64) -> Result<alloy_primitives::B256, Self::Error> {
            self.inner.block_hash(number)
        }
    }

    #[test]
    fn token_cache_is_shared_within_a_round_and_refreshed_next_round() {
        let mut db = CountingDb {
            inner: test_state(0, 0, TX_TOKEN_BUDGET),
            reads: HashMap::new(),
        };
        let txs: Vec<_> = (0..3).map(token_fee_tx).collect();
        assert!(
            collect_removable_transactions(
                &mut db,
                &L1BlockInfo::default(),
                &test_evm_env(),
                30_000_000,
                txs.iter().collect()
            )
            .is_empty()
        );
        // Three transactions share one registry entry (five words) and one `balanceOf` SLOAD.
        assert_eq!(db.reads[&L2_TOKEN_REGISTRY_ADDRESS], 5);
        assert_eq!(db.reads[&FEE_TOKEN], 1);

        // The next round reads both again, so it sees the balance spent in between.
        db.inner
            .insert_account_storage(FEE_TOKEN, token_balance_key(SIGNER), U256::ZERO)
            .unwrap();
        assert_eq!(
            collect_removable_transactions(
                &mut db,
                &L1BlockInfo::default(),
                &test_evm_env(),
                30_000_000,
                txs.iter().collect()
            ),
            vec![*txs[0].hash()]
        );
        assert_eq!(db.reads[&L2_TOKEN_REGISTRY_ADDRESS], 10);
        assert_eq!(db.reads[&FEE_TOKEN], 2);
    }

    #[test]
    fn unreadable_token_state_does_not_remove_transactions() {
        let tx = token_fee_tx(0);

        // Sanity check: the same transaction against readable state is kept as well, so the
        // assertions below are about the read failure and not about affordability.
        assert!(
            removable(&mut test_state(0, 0, 10 * TX_TOKEN_BUDGET), vec![&tx]).is_empty(),
            "transaction is affordable when the token balance can be read"
        );

        for failing in [TokenRead::Storage, TokenRead::Code] {
            let mut db = UnreadableTokenDb {
                inner: test_state(0, 0, 10 * TX_TOKEN_BUDGET),
                token: FEE_TOKEN,
                failing,
            };
            let to_remove = collect_removable_transactions(
                &mut db,
                &L1BlockInfo::default(),
                &test_evm_env(),
                30_000_000,
                vec![&tx],
            );
            assert!(
                to_remove.is_empty(),
                "a {failing:?} read failure inside balanceOf is not an invalid transaction"
            );
        }
    }

    #[test]
    fn a_balance_query_without_a_balance_removes_the_transaction() {
        let tx = token_fee_tx(0);
        for code in FAILING_BALANCE_OF {
            let mut db = test_state_with_token_code(0, 0, 10 * TX_TOKEN_BUDGET, code);
            assert_eq!(
                removable(&mut db, vec![&tx]),
                vec![*tx.hash()],
                "balanceOf code {code:02x?}"
            );
        }
    }

    // ---------------------------------------------------------------------------------
    // Pool-level test: only the pool can show what happens to a removed transaction's
    // descendants, so this one drives the maintenance loop against a real pool.
    // ---------------------------------------------------------------------------------

    use alloy_consensus::TxLegacy;
    use alloy_primitives::Sealable;
    use morph_chainspec::{MORPH_MAINNET, MorphChainSpec};
    use morph_evm::MorphEvmConfig;
    use morph_primitives::MorphPrimitives;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::{
        CoinbaseTipOrdering, Pool, blobstore::InMemoryBlobStore,
        validate::EthTransactionValidatorBuilder,
    };

    type TestProvider = MockEthProvider<MorphPrimitives, MorphChainSpec>;

    fn storage_key(slot: U256) -> alloy_primitives::B256 {
        alloy_primitives::B256::from(slot.to_be_bytes::<32>())
    }

    /// The chain head the pool validates against: an empty Emerald-active block.
    fn head_block() -> morph_primitives::Block {
        morph_primitives::Block {
            header: morph_primitives::MorphHeader::from(alloy_consensus::Header {
                number: 1,
                timestamp: 1_767_765_600,
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(10),
                ..Default::default()
            }),
            body: Default::default(),
        }
    }

    /// Mirrors [`test_state`] for [`MockEthProvider`], which the pool's validator needs.
    fn mock_provider(eth_balance: u64, token_balance: u64) -> TestProvider {
        let client = MockEthProvider::<MorphPrimitives, _>::new()
            .with_chain_spec((**MORPH_MAINNET).clone())
            .with_genesis_block();

        // MorphTx is only accepted from Emerald onwards, so the head must be past it.
        let head = head_block();
        client.add_block(head.header.hash_slow(), head);

        client.add_account(SIGNER, ExtendedAccount::new(0, U256::from(eth_balance)));
        client.add_account(
            L2_TOKEN_REGISTRY_ADDRESS,
            ExtendedAccount::new(0, U256::ZERO).extend_storage(
                call_mode_registry_storage(TOKEN_ID, FEE_TOKEN)
                    .map(|(slot, value)| (storage_key(slot), value)),
            ),
        );
        set_token_balance(&client, token_balance);
        client
    }

    /// Deploys the call-mode fee token with `token_balance` recorded for [`SIGNER`].
    fn set_token_balance(client: &TestProvider, token_balance: u64) {
        client.add_account(
            FEE_TOKEN,
            ExtendedAccount::new(0, U256::ZERO)
                .with_bytecode(alloy_primitives::Bytes::from_static(BALANCE_OF_RUNTIME))
                .extend_storage([(
                    storage_key(token_balance_key(SIGNER)),
                    U256::from(token_balance),
                )]),
        );
    }

    /// A canonical commit of [`head_block`].
    fn commit_event() -> reth_provider::CanonStateNotification<MorphPrimitives> {
        commit_event_of(head_block())
    }

    /// A canonical commit of `block`.
    fn commit_event_of(
        block: morph_primitives::Block,
    ) -> reth_provider::CanonStateNotification<MorphPrimitives> {
        reth_provider::CanonStateNotification::Commit {
            new: std::sync::Arc::new(reth_provider::Chain::new(
                [reth_primitives_traits::RecoveredBlock::new_unhashed(
                    block,
                    Vec::new(),
                )],
                Default::default(),
                Default::default(),
            )),
        }
    }

    /// A plain ETH-fee transaction, affordable on its own.
    fn legacy_tx(tx_nonce: u64) -> MorphPooledTransaction {
        legacy_tx_for_sender(tx_nonce, SIGNER)
    }

    fn legacy_tx_for_sender(tx_nonce: u64, sender: Address) -> MorphPooledTransaction {
        let tx = TxLegacy {
            chain_id: Some(2818),
            nonce: tx_nonce,
            gas_limit: 21_000,
            gas_price: 100,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            ..Default::default()
        };
        let recovered = Recovered::new_unchecked(
            MorphTxEnvelope::Legacy(Signed::new_unhashed(tx, Signature::test_signature())),
            sender,
        );
        let encoded_len = recovered.encode_2718_len();
        MorphPooledTransaction::new(recovered, encoded_len)
    }

    /// Runs the maintenance loop against the provider, as the node does.
    fn provider_state(client: TestProvider) -> ProviderFeeState<TestProvider, MorphEvmConfig> {
        ProviderFeeState {
            client,
            evm_config: MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
        }
    }

    /// A [`FeeStateSource`] with a separate state per block, unlike [`MockEthProvider`],
    /// whose state ignores the block hash. `head` is the canonical head.
    struct BlockStates {
        states: HashMap<B256, TestProvider>,
        head: SealedHeader<morph_primitives::MorphHeader>,
    }

    impl FeeStateSource<MorphPrimitives> for BlockStates {
        fn state_for(
            &self,
            header: &morph_primitives::MorphHeader,
        ) -> Result<MorphValidationState, BoxError> {
            let client = self
                .states
                .get(&header.hash_slow())
                .ok_or("no state for this block")?;
            crate::validator::validation_state_for_header(
                client,
                &MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
                header,
            )
        }

        fn canonical_head(
            &self,
        ) -> Result<Option<SealedHeader<morph_primitives::MorphHeader>>, BoxError> {
            Ok(Some(self.head.clone()))
        }
    }

    /// The block after [`head_block`].
    fn next_block() -> morph_primitives::Block {
        let mut block = head_block();
        block.header.inner.number = 2;
        block.header.inner.timestamp += 1;
        block
    }

    /// Admits [`token_fee_tx`] 0 while [`SIGNER`] can pay, then runs maintenance on `events`
    /// with the sender's tokens drained at [`head_block`] and `head_token_balance` at the
    /// canonical head, [`next_block`]. Returns whether the transaction is still pooled.
    ///
    /// The events go through a tokio broadcast channel, like reth's canonical stream, and the
    /// loop runs the way `spawn_critical_blocking_task` runs it: `Handle::block_on` on a
    /// blocking thread, where tokio's cooperative budget applies.
    fn survives_a_stale_round(
        head_token_balance: u64,
        events: Vec<reth_provider::CanonStateNotification<MorphPrimitives>>,
    ) -> bool {
        let judged = mock_provider(0, TX_TOKEN_BUDGET);
        let validator = crate::MorphTransactionValidator::new(
            EthTransactionValidatorBuilder::new(
                judged.clone(),
                MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
            )
            .disable_balance_check()
            .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
            .build::<MorphPooledTransaction, _>(InMemoryBlobStore::default()),
        );
        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        );
        let hash = futures::executor::block_on(pool.add_transaction(
            reth_transaction_pool::TransactionOrigin::Local,
            token_fee_tx(0),
        ))
        .unwrap()
        .hash;
        set_token_balance(&judged, 0);
        let head = next_block().header;
        let source = BlockStates {
            states: HashMap::from([
                (head_block().header.hash_slow(), judged),
                (head.hash_slow(), mock_provider(0, head_token_balance)),
            ]),
            head: SealedHeader::seal_slow(head),
        };

        let (sender, receiver) = tokio::sync::broadcast::channel(events.len());
        for event in events {
            sender.send(event).unwrap();
        }
        drop(sender);
        let events = tokio_stream::wrappers::BroadcastStream::new(receiver)
            .map(|event| event.expect("the channel holds every event"));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let handle = runtime.handle().clone();
        let loop_pool = pool.clone();
        let task = runtime.spawn_blocking(move || {
            handle.block_on(maintain_morph_pool_with(loop_pool, source, events))
        });
        runtime.block_on(task).unwrap();
        pool.get(&hash).is_some()
    }

    #[test]
    fn removals_judged_at_an_older_block_are_rechecked_at_the_canonical_head() {
        for backlog in [false, true] {
            // Without a backlog, the head moved on before its notification arrived. With one,
            // 128 notifications exhaust tokio's cooperative budget within a single poll, so the
            // loop cannot see the head's notification behind them before it removes anything.
            let events = || {
                let mut events: Vec<_> = std::iter::repeat_with(commit_event)
                    .take(if backlog { 128 } else { 1 })
                    .collect();
                if backlog {
                    events.push(commit_event_of(next_block()));
                }
                events
            };
            assert!(
                survives_a_stale_round(TX_TOKEN_BUDGET, events()),
                "backlog={backlog}: payable at the canonical head, so it must stay"
            );
            assert!(
                !survives_a_stale_round(0, events()),
                "backlog={backlog}: still unpayable at the canonical head, so it must go"
            );
        }
    }

    #[test]
    fn unchanged_head_does_not_evict_newly_admitted_transactions() {
        let client = mock_provider(6_300_000, 10_000_000);
        client.add_account(
            morph_revm::L1_GAS_PRICE_ORACLE_ADDRESS,
            ExtendedAccount::new(0, U256::ZERO).extend_storage([
                (storage_key(U256::from(1)), U256::from(1)),
                (
                    storage_key(U256::from(7)),
                    U256::from(2_000_000_000_000_000u64),
                ),
            ]),
        );
        let validator = crate::MorphTransactionValidator::new(
            EthTransactionValidatorBuilder::new(
                client.clone(),
                MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
            )
            .disable_balance_check()
            .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
            .build::<MorphPooledTransaction, _>(InMemoryBlobStore::default()),
        );
        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        );
        let hashes: Vec<_> = (0..3)
            .map(|nonce| {
                futures::executor::block_on(pool.add_transaction(
                    reth_transaction_pool::TransactionOrigin::Local,
                    legacy_tx(nonce),
                ))
                .unwrap()
                .hash
            })
            .collect();
        assert_eq!(pool.all_transactions().pending.len(), 3);
        futures::executor::block_on(maintain_morph_pool_with(
            pool.clone(),
            provider_state(client),
            futures::stream::iter([commit_event()]),
        ));
        assert!(hashes.iter().all(|hash| pool.get(hash).is_some()));
        assert_eq!(pool.all_transactions().pending.len(), 3);
    }

    #[test]
    fn ordinary_only_senders_are_revalidated_when_l1_fees_rise() {
        use reth_transaction_pool::TransactionPoolExt;

        let sender = address!("0000000000000000000000000000000000000009");
        // Each ordinary transaction costs 2,100,000 wei before L1 fees.
        // A 2,000,000 L1 fee still fits individually at 6,300,000;
        // a 5,000,000 L1 fee does not.
        for unrelated_morph in [false, true] {
            for (l1_fee, eth_balance, first_unaffordable) in [
                (0u64, 6_300_000u64, None),
                (2_000_000, 12_300_000, None),
                (2_000_000, 6_300_000, None),
                (5_000_000, 6_300_000, Some(0usize)),
            ] {
                let client = mock_provider(10_000_000, 100_000_000);
                client.add_account(sender, ExtendedAccount::new(0, U256::from(20_000_000)));
                let validator = crate::MorphTransactionValidator::new(
                    EthTransactionValidatorBuilder::new(
                        client.clone(),
                        MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
                    )
                    .disable_balance_check()
                    .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
                    .build::<MorphPooledTransaction, _>(InMemoryBlobStore::default()),
                );
                let pool = Pool::new(
                    validator,
                    CoinbaseTipOrdering::default(),
                    InMemoryBlobStore::default(),
                    Default::default(),
                );
                let ordinary: Vec<_> = (0..3)
                    .map(|nonce| {
                        futures::executor::block_on(pool.add_transaction(
                            reth_transaction_pool::TransactionOrigin::Local,
                            legacy_tx_for_sender(nonce, sender),
                        ))
                        .unwrap()
                        .hash
                    })
                    .collect();
                if unrelated_morph {
                    futures::executor::block_on(pool.add_transaction(
                        reth_transaction_pool::TransactionOrigin::Local,
                        token_fee_tx(0),
                    ))
                    .unwrap();
                }
                assert_eq!(
                    pool.all_transactions().pending.len(),
                    3 + usize::from(unrelated_morph)
                );

                client.add_account(sender, ExtendedAccount::new(0, U256::from(eth_balance)));
                client.add_account(
                    morph_revm::L1_GAS_PRICE_ORACLE_ADDRESS,
                    ExtendedAccount::new(0, U256::ZERO).extend_storage([
                        (storage_key(U256::from(1)), U256::from(1)),
                        (
                            storage_key(U256::from(7)),
                            U256::from(l1_fee) * U256::from(1_000_000_000),
                        ),
                    ]),
                );
                let event = commit_event();
                pool.on_canonical_state_change(reth_transaction_pool::CanonicalStateUpdate {
                    new_tip: event.tip(),
                    pending_block_base_fee: 10,
                    pending_block_blob_fee: None,
                    changed_accounts: vec![reth_provider::ChangedAccount {
                        address: sender,
                        nonce: 0,
                        balance: U256::from(eth_balance),
                    }],
                    mined_transactions: Vec::new(),
                    update_kind: reth_transaction_pool::PoolUpdateKind::Commit,
                });
                assert_eq!(
                    pool.all_transactions().pending.len(),
                    3 + usize::from(unrelated_morph),
                    "standard maintenance cannot see the L1 fee shortfall"
                );

                // Later rounds must retain parked descendants behind the removed nonce.
                for _ in 0..3 {
                    futures::executor::block_on(maintain_morph_pool_with(
                        pool.clone(),
                        provider_state(client.clone()),
                        futures::stream::iter([event.clone()]),
                    ));
                }
                let all = pool.all_transactions();
                let pending: Vec<_> = all
                    .pending
                    .iter()
                    .filter(|tx| tx.sender() == sender)
                    .map(|tx| *tx.hash())
                    .collect();
                let queued: Vec<_> = all
                    .queued
                    .iter()
                    .filter(|tx| tx.sender() == sender)
                    .map(|tx| *tx.hash())
                    .collect();
                if let Some(index) = first_unaffordable {
                    assert!(
                        pool.get(&ordinary[index]).is_none(),
                        "remove the first L1-unaffordable ordinary transaction; unrelated MorphTx={unrelated_morph}"
                    );
                    assert_eq!(pending, ordinary[..index]);
                    assert_eq!(queued, ordinary[index + 1..]);
                } else {
                    assert_eq!(pending, ordinary);
                    assert!(queued.is_empty());
                }
            }
        }
    }

    #[test]
    fn ordinary_only_senders_keep_nonce_gaps_and_eth_parked_transactions() {
        use reth_transaction_pool::TransactionPoolExt;

        for (nonces, state_nonce, eth_balance, pending_nonces, queued_nonces) in [
            (vec![0, 2], 0, 3_100_000u64, vec![0], vec![2]),
            (vec![5], 0, 2_100_000, vec![], vec![5]),
            (vec![0, 1], 0, 2_000_000, vec![], vec![0, 1]),
            (vec![0, 1], 1, 3_100_000, vec![1], vec![]),
        ] {
            let client = mock_provider(10_000_000, 0);
            let validator = crate::MorphTransactionValidator::new(
                EthTransactionValidatorBuilder::new(
                    client.clone(),
                    MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
                )
                .disable_balance_check()
                .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
                .build::<MorphPooledTransaction, _>(InMemoryBlobStore::default()),
            );
            let pool = Pool::new(
                validator,
                CoinbaseTipOrdering::default(),
                InMemoryBlobStore::default(),
                Default::default(),
            );
            for nonce in nonces {
                futures::executor::block_on(pool.add_transaction(
                    reth_transaction_pool::TransactionOrigin::Local,
                    legacy_tx(nonce),
                ))
                .unwrap();
            }
            client.add_account(
                SIGNER,
                ExtendedAccount::new(state_nonce, U256::from(eth_balance)),
            );
            // Every ordinary transaction now owes 1,000,000 wei in L1 fees.
            client.add_account(
                morph_revm::L1_GAS_PRICE_ORACLE_ADDRESS,
                ExtendedAccount::new(0, U256::ZERO).extend_storage([
                    (storage_key(U256::from(1)), U256::from(1)),
                    (
                        storage_key(U256::from(7)),
                        U256::from(1_000_000_000_000_000u64),
                    ),
                ]),
            );
            let event = commit_event();
            // Run Morph first to cover a pool snapshot that still contains a mined nonce.
            futures::executor::block_on(maintain_morph_pool_with(
                pool.clone(),
                provider_state(client.clone()),
                futures::stream::iter([event.clone()]),
            ));
            pool.on_canonical_state_change(reth_transaction_pool::CanonicalStateUpdate {
                new_tip: event.tip(),
                pending_block_base_fee: 10,
                pending_block_blob_fee: None,
                changed_accounts: vec![reth_provider::ChangedAccount {
                    address: SIGNER,
                    nonce: state_nonce,
                    balance: U256::from(eth_balance),
                }],
                mined_transactions: Vec::new(),
                update_kind: reth_transaction_pool::PoolUpdateKind::Commit,
            });
            // And run after reth parks/removes transactions, covering either task order.
            futures::executor::block_on(maintain_morph_pool_with(
                pool.clone(),
                provider_state(client),
                futures::stream::iter([event]),
            ));
            let all = pool.all_transactions();
            assert_eq!(
                all.pending.iter().map(|tx| tx.nonce()).collect::<Vec<_>>(),
                pending_nonces
            );
            assert_eq!(
                all.queued.iter().map(|tx| tx.nonce()).collect::<Vec<_>>(),
                queued_nonces
            );
        }
    }

    #[test]
    fn morph_after_legacy_nonce_is_revalidated_after_token_balance_drops() {
        let client = mock_provider(10_000_000, 10 * TX_TOKEN_BUDGET);
        let validator = crate::MorphTransactionValidator::new(
            EthTransactionValidatorBuilder::new(
                client.clone(),
                MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
            )
            .disable_balance_check()
            .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
            .build::<MorphPooledTransaction, _>(InMemoryBlobStore::default()),
        );
        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        );

        futures::executor::block_on(pool.add_transaction(
            reth_transaction_pool::TransactionOrigin::Local,
            legacy_tx(0),
        ))
        .unwrap();
        let token_tx = futures::executor::block_on(pool.add_transaction(
            reth_transaction_pool::TransactionOrigin::Local,
            token_fee_tx(1),
        ))
        .unwrap()
        .hash;
        assert_eq!(pool.all_transactions().pending.len(), 2);
        assert!(pool.all_transactions().queued.is_empty());

        set_token_balance(&client, 0);
        futures::executor::block_on(maintain_morph_pool_with(
            pool.clone(),
            provider_state(client),
            futures::stream::iter([commit_event()]),
        ));

        assert!(
            pool.get(&token_tx).is_none(),
            "a pending MorphTx following a legacy nonce must still be checked after its token balance becomes zero"
        );
    }

    #[test]
    fn ordinary_l1_fee_shortfall_parks_the_morph_successor() {
        use reth_transaction_pool::TransactionPoolExt;

        // Two individually affordable predecessors must both survive, even if
        // their combined maximum gas and L1 costs exceed the account balance.
        for (ordinary_count, l1_fee, eth_balance, token_balance) in [
            (1, 0u64, 2_100_000u64, 0u64),
            (1, 1_000_000, 2_100_000, 0),
            (1, 1_000_000, 2_100_000, 21_000_000),
            (2, 1_000_000, 4_200_000, 21_000_000),
        ] {
            let client = mock_provider(10_000_000, 10 * TX_TOKEN_BUDGET);
            let validator = crate::MorphTransactionValidator::new(
                EthTransactionValidatorBuilder::new(
                    client.clone(),
                    MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
                )
                .disable_balance_check()
                .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
                .build::<MorphPooledTransaction, _>(InMemoryBlobStore::default()),
            );
            let pool = Pool::new(
                validator,
                CoinbaseTipOrdering::default(),
                InMemoryBlobStore::default(),
                Default::default(),
            );
            let ordinary: Vec<_> = (0..ordinary_count)
                .map(|nonce| {
                    futures::executor::block_on(pool.add_transaction(
                        reth_transaction_pool::TransactionOrigin::Local,
                        legacy_tx(nonce),
                    ))
                    .unwrap()
                    .hash
                })
                .collect();
            let token_tx = futures::executor::block_on(pool.add_transaction(
                reth_transaction_pool::TransactionOrigin::Local,
                token_fee_tx(ordinary_count),
            ))
            .unwrap()
            .hash;
            assert_eq!(pool.all_transactions().pending.len(), ordinary.len() + 1);

            // All were affordable at admission. The new state still covers reth's
            // ordinary transaction cost, but cannot cover the new L1 fee as well.
            client.add_account(SIGNER, ExtendedAccount::new(0, U256::from(eth_balance)));
            set_token_balance(&client, token_balance);
            client.add_account(
                morph_revm::L1_GAS_PRICE_ORACLE_ADDRESS,
                ExtendedAccount::new(0, U256::ZERO).extend_storage([
                    (storage_key(U256::from(1)), U256::from(1)),
                    (
                        storage_key(U256::from(7)),
                        U256::from(l1_fee) * U256::from(1_000_000_000),
                    ),
                ]),
            );
            let mut block = head_block();
            block.header.inner.number = 2;
            block.header.inner.timestamp += 1;
            client.add_block(block.header.hash_slow(), block.clone());
            let event = reth_provider::CanonStateNotification::Commit {
                new: std::sync::Arc::new(reth_provider::Chain::new(
                    [reth_primitives_traits::RecoveredBlock::new_unhashed(
                        block,
                        Vec::new(),
                    )],
                    Default::default(),
                    Default::default(),
                )),
            };
            // Exercise the same public canonical update used by standard maintenance.
            pool.on_canonical_state_change(reth_transaction_pool::CanonicalStateUpdate {
                new_tip: event.tip(),
                pending_block_base_fee: 10,
                pending_block_blob_fee: None,
                changed_accounts: vec![reth_provider::ChangedAccount {
                    address: SIGNER,
                    nonce: 0,
                    balance: U256::from(eth_balance),
                }],
                mined_transactions: Vec::new(),
                update_kind: reth_transaction_pool::PoolUpdateKind::Commit,
            });
            assert_eq!(
                pool.all_transactions().pending.len(),
                ordinary.len() + 1,
                "standard maintenance does not see L1 costs"
            );

            for _ in 0..3 {
                futures::executor::block_on(maintain_morph_pool_with(
                    pool.clone(),
                    provider_state(client.clone()),
                    futures::stream::iter([event.clone()]),
                ));
            }
            let all = pool.all_transactions();
            if l1_fee == 0 {
                assert!(ordinary.iter().all(|hash| pool.get(hash).is_some()));
                assert!(
                    pool.get(&token_tx).is_none(),
                    "the unfunded MorphTx is removed"
                );
                assert!(all.queued.is_empty());
            } else if ordinary_count == 2 {
                assert!(ordinary.iter().all(|hash| pool.get(hash).is_some()));
                assert!(pool.get(&token_tx).is_some());
                assert_eq!(all.pending.len(), 3);
                assert!(all.queued.is_empty());
            } else {
                let (unaffordable, affordable) = ordinary.split_last().unwrap();
                assert!(
                    pool.get(unaffordable).is_none(),
                    "the first L1-unaffordable predecessor must be removed"
                );
                assert!(affordable.iter().all(|hash| pool.get(hash).is_some()));
                assert_eq!(all.pending.len(), affordable.len());
                assert_eq!(
                    all.queued.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
                    [token_tx],
                    "preserve the successor in queued, including when it still has tokens"
                );
            }
        }
    }

    #[test]
    fn removing_a_morph_tx_parks_its_descendants_instead_of_deleting_them() {
        let client = mock_provider(10_000_000, 10 * TX_TOKEN_BUDGET);
        let validator = crate::MorphTransactionValidator::new(
            EthTransactionValidatorBuilder::new(
                client.clone(),
                MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
            )
            .disable_balance_check()
            .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
            .build::<MorphPooledTransaction, _>(InMemoryBlobStore::default()),
        );
        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        );

        // nonce 0 pays in tokens, nonce 1 is a plain ETH transaction that only depends on
        // nonce 0 through the nonce sequence.
        let unpayable = futures::executor::block_on(pool.add_transaction(
            reth_transaction_pool::TransactionOrigin::Local,
            token_fee_tx(0),
        ))
        .unwrap()
        .hash;
        let descendant = futures::executor::block_on(pool.add_transaction(
            reth_transaction_pool::TransactionOrigin::Local,
            legacy_tx(1),
        ))
        .unwrap()
        .hash;

        // The sender spends its whole token balance elsewhere, so nonce 0 is no longer payable.
        set_token_balance(&client, 0);
        let event = commit_event();
        futures::executor::block_on(maintain_morph_pool_with(
            pool.clone(),
            provider_state(client),
            futures::stream::iter([event]),
        ));

        // Without the removal, every early return of the maintenance round would pass the
        // descendant check below as well.
        assert!(
            pool.get(&unpayable).is_none(),
            "the transaction the sender can no longer pay for must be removed"
        );
        let queued = pool.all_transactions().queued;
        assert!(
            queued.iter().any(|tx| *tx.hash() == descendant),
            "an independently affordable ETH-fee successor must be parked, not deleted"
        );
    }
}
