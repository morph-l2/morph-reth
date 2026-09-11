//! Transaction pool maintenance tasks for Morph L2.
//!
//! This module provides maintenance tasks for the Morph transaction pool,
//! specifically for revalidating MorphTx (0x7F) transactions when the chain
//! state changes.
//!
//! # Background
//!
//! MorphTx allows users to pay gas fees using ERC20 tokens. Since reth's txpool
//! only tracks ETH balance changes (via `SenderInfo`), it cannot automatically
//! demote MorphTx transactions when the token balance decreases.
//!
//! This maintenance task solves this by:
//! 1. Listening to canonical state changes (new blocks)
//! 2. Re-validating all MorphTx (0x7F) transactions in the pool
//! 3. Removing transactions that no longer have sufficient token balance
//!
//! # Relationship with reth's own maintenance task
//!
//! This task runs *alongside* [`reth_transaction_pool::maintain::maintain_transaction_pool`],
//! and both subscribe to the canonical state stream independently — there is no ordering
//! guarantee between them. Everything reth's task already understands (ETH balance, nonces,
//! base fee, mined transactions) stays its responsibility; this task only adds the one
//! dimension reth cannot see, the sender's **ERC20 token** balance.
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

use crate::{MorphPooledTransaction, MorphTxError};
use alloy_consensus::Transaction;
use alloy_consensus::Typed2718;
use alloy_primitives::{Address, TxHash, U256};
use futures::{FutureExt, StreamExt};
use morph_chainspec::hardfork::{MorphHardfork, MorphHardforks};
use morph_revm::{L1BlockInfo, MorphBlockEnv, MorphEvmEnv};
use reth_chainspec::ChainSpecProvider;
use reth_evm::{ConfigureEvm, EvmFactory, EvmFactoryFor};
use reth_primitives_traits::AlloyBlockHeader;
use reth_provider::CanonStateSubscriptions;
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use std::collections::HashMap;

/// Sender-level rolling affordability budget used during maintenance revalidation.
#[derive(Debug, Clone, Default)]
struct SenderBudget {
    /// Remaining ETH budget for this sender.
    eth_balance: U256,
    /// Remaining token budget per `fee_token_id`.
    token_balances: HashMap<u16, U256>,
}

/// Applies cumulative sender-budget check for the ETH-fee path and consumes budget on success.
///
/// Returns `true` if the transaction can be afforded under the current rolling ETH budget.
fn consume_eth_budget(
    budget: &mut SenderBudget,
    tx_value: U256,
    gas_limit: u64,
    max_fee_per_gas: u128,
    l1_data_fee: U256,
) -> bool {
    let gas_fee = U256::from(gas_limit).saturating_mul(U256::from(max_fee_per_gas));
    let total_eth_cost = gas_fee.saturating_add(l1_data_fee).saturating_add(tx_value);
    if total_eth_cost > budget.eth_balance {
        return false;
    }
    budget.eth_balance = budget.eth_balance.saturating_sub(total_eth_cost);
    true
}

/// Applies cumulative sender-budget check for the token-fee path and consumes budget on success.
///
/// Returns `true` if the transaction can be afforded under the current rolling token/ETH budget.
fn consume_token_budget(
    budget: &mut SenderBudget,
    tx_value: U256,
    token_id: Option<u16>,
    fee_limit: Option<U256>,
    required_token_amount: U256,
    state_token_balance: Option<U256>,
) -> bool {
    let (token_id, fee_limit) = match (token_id, fee_limit) {
        (Some(token_id), Some(fee_limit)) => (token_id, fee_limit),
        _ => return false,
    };

    let token_budget = budget
        .token_balances
        .entry(token_id)
        .or_insert(state_token_balance.unwrap_or(U256::ZERO));

    // Match REVM semantics with rolling sender budget:
    // - fee_limit == 0 => use remaining token budget
    // - fee_limit > remaining => cap by remaining token budget
    let effective_limit = if fee_limit.is_zero() || fee_limit > *token_budget {
        *token_budget
    } else {
        fee_limit
    };

    if effective_limit < required_token_amount || tx_value > budget.eth_balance {
        return false;
    }

    *token_budget = (*token_budget).saturating_sub(required_token_amount);
    budget.eth_balance = budget.eth_balance.saturating_sub(tx_value);
    true
}

fn exceeds_block_gas_limit(tx_gas_limit: u64, block_gas_limit: u64) -> bool {
    tx_gas_limit > block_gas_limit
}

/// Classifies a validation failure as "the transaction is bad" vs. "we could not read the state".
///
/// A failed state read says nothing about the transaction: the token registry entry or the
/// caller's balance slot simply could not be resolved at this tip. Removing transactions on
/// that basis loses user transactions to transient I/O, so these are treated as unknown and
/// the sender is left alone until the next canonical event.
///
/// Note that go-ethereum does the opposite — `executableTxFilter` drops the transaction when
/// `getBalanceFunc` errors (core/tx_pool.go:1690) — which is deliberately *not* mirrored here.
/// The rest of this task already skips on a failed state provider, L1 block info fetch or ETH
/// balance read; token state reads follow the same rule.
const fn is_transient(err: &MorphTxError) -> bool {
    matches!(err, MorphTxError::TokenInfoFetchFailed { .. })
}

/// Determines which MorphTx transactions are no longer viable at the given state.
///
/// Returns the hashes to remove from the pool. Only the first offending transaction of a
/// sender is returned: the pool parks the rest of that sender's transactions on its own when
/// the returned hash is removed (see [`maintain_morph_pool`]).
fn collect_removable_transactions<DB: alloy_evm::Database>(
    db: &mut DB,
    l1_block_info: &L1BlockInfo,
    evm_env: &MorphEvmEnv,
    block_gas_limit: u64,
    morph_txs: Vec<&MorphPooledTransaction>,
) -> Vec<TxHash> {
    let hardfork = *evm_env.cfg_env.spec();
    // Group by sender and process in nonce order so affordability is validated cumulatively.
    let mut txs_by_sender: HashMap<Address, Vec<&MorphPooledTransaction>> = HashMap::new();
    for tx in morph_txs {
        txs_by_sender.entry(tx.sender()).or_default().push(tx);
    }

    let mut to_remove: Vec<TxHash> = Vec::new();

    for (sender, mut sender_txs) in txs_by_sender {
        sender_txs.sort_by_key(|tx| tx.transaction().nonce());

        // Read the nonce alongside the balance. The balance seeds the rolling budget; the
        // nonce tells us which pooled transactions the new block already executed and must
        // therefore not be charged against that (already reduced) balance again.
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

        let mut budget = SenderBudget {
            eth_balance: account.balance,
            token_balances: HashMap::new(),
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

            if exceeds_block_gas_limit(consensus_tx.gas_limit(), block_gas_limit) {
                tracing::debug!(
                    target: "morph::txpool::maintain",
                    tx_hash = ?tx.hash(),
                    ?sender,
                    tx_gas_limit = consensus_tx.gas_limit(),
                    block_gas_limit,
                    "Removing MorphTx: gas limit exceeds current block gas limit"
                );
                to_remove.push(*tx.hash());
                break;
            }

            let l1_data_fee = l1_block_info.calculate_tx_l1_cost(tx.encoded_2718(), hardfork);

            // Use shared validation logic first with current sender ETH budget.
            let input = crate::MorphTxValidationInput {
                consensus_tx,
                sender,
                eth_balance: budget.eth_balance,
                l1_data_fee,
                hardfork,
                evm_env,
            };

            let validation = match crate::validate_morph_tx(db, &input) {
                Ok(v) => v,
                Err(err) if is_transient(&err) => {
                    tracing::warn!(
                        target: "morph::txpool::maintain",
                        tx_hash = ?tx.hash(),
                        ?sender,
                        %err,
                        "Could not read token state; leaving sender's MorphTx in the pool"
                    );
                    break;
                }
                Err(err) => {
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

            let fields = consensus_tx.morph_fields();
            let state_token_balance = validation.token_info.as_ref().map(|info| info.balance);
            let token_id = fields.as_ref().map(|f| f.fee_token_id);
            let fee_limit = fields.as_ref().map(|f| f.fee_limit);

            let affordable = if validation.uses_token_fee {
                consume_token_budget(
                    &mut budget,
                    consensus_tx.value(),
                    token_id,
                    fee_limit,
                    validation.required_token_amount,
                    state_token_balance,
                )
            } else {
                consume_eth_budget(
                    &mut budget,
                    consensus_tx.value(),
                    consensus_tx.gas_limit(),
                    consensus_tx.max_fee_per_gas(),
                    l1_data_fee,
                )
            };
            if !affordable {
                tracing::debug!(
                    target: "morph::txpool::maintain",
                    tx_hash = ?tx.hash(),
                    ?sender,
                    uses_token_fee = validation.uses_token_fee,
                    token_id = ?token_id,
                    required_token_amount = ?validation.required_token_amount,
                    "Removing MorphTx: insufficient cumulative sender budget"
                );
                to_remove.push(*tx.hash());
                break;
            }
        }
    }

    to_remove
}

/// Maintains the Morph transaction pool by revalidating MorphTx transactions.
///
/// This task runs continuously and:
/// - Listens for new canonical blocks
/// - Re-validates MorphTx (0x7F) transactions in the pool
/// - Removes transactions that no longer have sufficient token balance
///
pub async fn maintain_morph_pool<Pool, Client, Evm>(pool: Pool, client: Client, evm_config: Evm)
where
    Pool: TransactionPool<Transaction = MorphPooledTransaction> + Clone,
    Client: ChainSpecProvider<ChainSpec: MorphHardforks>
        + StateProviderFactory
        + CanonStateSubscriptions
        + Clone
        + 'static,
    Evm: ConfigureEvm<Primitives = <Client as reth_provider::NodePrimitivesProvider>::Primitives>,
    EvmFactoryFor<Evm>: EvmFactory<Spec = MorphHardfork, BlockEnv = MorphBlockEnv>,
{
    let chain_events = client.canonical_state_stream();

    tracing::info!(target: "morph::txpool::maintain", "Starting MorphTx maintenance task");

    maintain_morph_pool_with(pool, client, evm_config, chain_events).await;
}

/// [`maintain_morph_pool`] with an explicit canonical event stream.
async fn maintain_morph_pool_with<Pool, Client, Evm, Events>(
    pool: Pool,
    client: Client,
    evm_config: Evm,
    mut chain_events: Events,
) where
    Pool: TransactionPool<Transaction = MorphPooledTransaction> + Clone,
    Client: ChainSpecProvider<ChainSpec: MorphHardforks>
        + StateProviderFactory
        + CanonStateSubscriptions
        + Clone
        + 'static,
    Evm: ConfigureEvm<Primitives = <Client as reth_provider::NodePrimitivesProvider>::Primitives>,
    EvmFactoryFor<Evm>: EvmFactory<Spec = MorphHardfork, BlockEnv = MorphBlockEnv>,
    Events:
        futures::Stream<Item = reth_provider::CanonStateNotification<Client::Primitives>> + Unpin,
{
    loop {
        // Wait for the next canonical state change
        let Some(mut event) = chain_events.next().await else {
            tracing::debug!(target: "morph::txpool::maintain", "Chain event stream ended");
            break;
        };

        // Skip ahead to the newest queued notification. A round costs one state read per
        // transaction, so under load the chain can advance while we are working; the verdicts
        // this task produces are a pure function of the latest state, which makes every
        // intermediate block wasted work against a stale view of the pool.
        while let Some(next) = chain_events.next().now_or_never().flatten() {
            event = next;
        }

        let new_tip = event.tip();
        let block_number = new_tip.number();
        let block_gas_limit = new_tip.gas_limit();

        tracing::trace!(
            target: "morph::txpool::maintain",
            block_number,
            "Processing new block for MorphTx validation"
        );

        // Build the environment execution would use for this block, so a call-mode fee
        // token's `balanceOf` resolves to the balance the execution layer would see.
        let evm_env = match evm_config.evm_env(new_tip.header()) {
            Ok(evm_env) => evm_env,
            Err(err) => {
                tracing::warn!(
                    target: "morph::txpool::maintain",
                    %err,
                    "Failed to build EVM env for MorphTx revalidation"
                );
                continue;
            }
        };
        let hardfork = *evm_env.cfg_env.spec();

        // Collect all MorphTx transactions from the pool
        let all_txs = pool.all_transactions();
        let morph_txs: Vec<&MorphPooledTransaction> = all_txs
            .pending
            .iter()
            .chain(all_txs.queued.iter())
            .map(|tx| &tx.transaction)
            .filter(|tx| tx.ty() == morph_primitives::MORPH_TX_TYPE_ID)
            .collect();

        if morph_txs.is_empty() {
            continue;
        }

        // Get state provider for the new tip
        let state_provider = match client.state_by_block_hash(new_tip.hash()) {
            Ok(provider) => provider,
            Err(err) => {
                tracing::warn!(
                    target: "morph::txpool::maintain",
                    %err,
                    "Failed to get state provider for MorphTx revalidation"
                );
                continue;
            }
        };

        let mut db = StateProviderDatabase::new(state_provider);

        // Fetch L1 block info for fee calculation
        let l1_block_info = match L1BlockInfo::try_fetch(&mut db, hardfork) {
            Ok(info) => info,
            Err(err) => {
                tracing::warn!(
                    target: "morph::txpool::maintain",
                    ?err,
                    "Failed to fetch L1 block info for MorphTx revalidation"
                );
                continue;
            }
        };

        tracing::trace!(
            target: "morph::txpool::maintain",
            count = morph_txs.len(),
            "Revalidating MorphTx transactions"
        );

        let to_remove = collect_removable_transactions(
            &mut db,
            &l1_block_info,
            &evm_env,
            block_gas_limit,
            morph_txs,
        );

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
                "Removed invalid MorphTx transactions"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consume_eth_fee_path_updates_budget_and_rejects_when_exhausted() {
        let mut budget = SenderBudget {
            eth_balance: U256::from(100u64),
            token_balances: HashMap::new(),
        };

        let first = consume_eth_budget(&mut budget, U256::from(20u64), 10, 3, U256::from(5u64));
        assert!(first);
        // total cost = value(20) + gas(30) + l1(5) = 55
        assert_eq!(budget.eth_balance, U256::from(45u64));

        let second = consume_eth_budget(&mut budget, U256::from(20u64), 10, 3, U256::from(5u64));
        assert!(!second);
        assert_eq!(budget.eth_balance, U256::from(45u64));
    }

    #[test]
    fn consume_token_fee_path_tracks_cumulative_token_budget() {
        let mut budget = SenderBudget {
            eth_balance: U256::from(10u64),
            token_balances: HashMap::new(),
        };

        let first = consume_token_budget(
            &mut budget,
            U256::ZERO,
            Some(7),
            Some(U256::ZERO), // fee_limit=0 => use full remaining budget
            U256::from(60u64),
            Some(U256::from(100u64)),
        );
        assert!(first);
        assert_eq!(
            budget.token_balances.get(&7).copied(),
            Some(U256::from(40u64))
        );

        let second = consume_token_budget(
            &mut budget,
            U256::ZERO,
            Some(7),
            Some(U256::ZERO),
            U256::from(50u64),
            None,
        );
        assert!(!second);
        assert_eq!(
            budget.token_balances.get(&7).copied(),
            Some(U256::from(40u64))
        );
    }

    #[test]
    fn consume_token_fee_path_honors_fee_limit_and_eth_value() {
        let mut budget = SenderBudget {
            eth_balance: U256::from(5u64),
            token_balances: HashMap::new(),
        };

        // fee_limit caps the payment below required amount => reject
        let limited = consume_token_budget(
            &mut budget,
            U256::ZERO,
            Some(9),
            Some(U256::from(30u64)),
            U256::from(40u64),
            Some(U256::from(100u64)),
        );
        assert!(!limited);

        // Enough token, but ETH value exceeds remaining ETH budget => reject
        let eth_value_fail = consume_token_budget(
            &mut budget,
            U256::from(6u64),
            Some(9),
            Some(U256::from(100u64)),
            U256::from(10u64),
            Some(U256::from(100u64)),
        );
        assert!(!eth_value_fail);
    }

    #[test]
    fn consume_mixed_path_sequence_tracks_eth_and_token_together() {
        let mut budget = SenderBudget {
            eth_balance: U256::from(100u64),
            token_balances: HashMap::new(),
        };

        // Tx1: token-fee path, consumes token only for fee and ETH for value.
        let tx1 = consume_token_budget(
            &mut budget,
            U256::from(10u64), // value in ETH
            Some(3),
            Some(U256::ZERO), // unlimited by tx field => bounded by remaining token budget
            U256::from(70u64),
            Some(U256::from(100u64)),
        );
        assert!(tx1);
        assert_eq!(budget.eth_balance, U256::from(90u64));
        assert_eq!(
            budget.token_balances.get(&3).copied(),
            Some(U256::from(30u64))
        );

        // Tx2: ETH-fee path, consumes full ETH cost.
        let tx2 = consume_eth_budget(
            &mut budget,
            U256::from(20u64), // value
            5,                 // gas_limit
            4,                 // max_fee_per_gas => gas fee = 20
            U256::from(10u64), // l1 fee
        );
        assert!(tx2);
        // total eth cost = 20(value) + 20(gas) + 10(l1) = 50
        assert_eq!(budget.eth_balance, U256::from(40u64));

        // Tx3: token-fee path should now fail because remaining token budget is only 30.
        let tx3 = consume_token_budget(
            &mut budget,
            U256::ZERO,
            Some(3),
            Some(U256::ZERO),
            U256::from(35u64),
            None,
        );
        assert!(!tx3);
        // Budgets stay unchanged on failed consumption.
        assert_eq!(budget.eth_balance, U256::from(40u64));
        assert_eq!(
            budget.token_balances.get(&3).copied(),
            Some(U256::from(30u64))
        );
    }

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

    use alloy_consensus::{Signed, transaction::Recovered};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Signature, TxKind, address};
    use morph_primitives::{MorphTxEnvelope, TxMorph};
    use morph_revm::{
        L2_TOKEN_REGISTRY_ADDRESS, compute_mapping_slot, compute_mapping_slot_for_address,
    };
    use reth_revm::revm::database::{CacheDB, EmptyDB};
    use reth_revm::revm::state::AccountInfo;

    const SIGNER: Address = address!("0000000000000000000000000000000000000001");
    const FEE_TOKEN: Address = address!("5300000000000000000000000000000000000042");
    const TOKEN_ID: u16 = 1;
    const BALANCE_SLOT: u64 = 7;
    /// `gas_limit * max_fee_per_gas` of [`token_fee_tx`]; at a 1:1 price ratio this is also
    /// the token amount one transaction reserves during revalidation.
    const TX_TOKEN_BUDGET: u64 = 21_000 * 100;

    fn token_id_key(token_id: u16) -> [u8; 32] {
        let mut key = [0u8; 32];
        key[30..32].copy_from_slice(&token_id.to_be_bytes());
        key
    }

    /// State with [`TOKEN_ID`] registered as an active slot-mode token at a 1:1 price ratio.
    fn test_state(account_nonce: u64, eth_balance: u64, token_balance: u64) -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            SIGNER,
            AccountInfo {
                nonce: account_nonce,
                balance: U256::from(eth_balance),
                ..Default::default()
            },
        );

        let token_key = token_id_key(TOKEN_ID);
        let base = compute_mapping_slot(U256::from(151), &token_key);
        let mut packed = [0u8; 32];
        packed[30] = 18; // decimals
        packed[31] = 1; // isActive
        for (slot, value) in [
            (base, U256::from_be_bytes(FEE_TOKEN.into_word().0)),
            // `balanceSlot` is stored as the actual slot plus one.
            (base + U256::from(1), U256::from(BALANCE_SLOT + 1)),
            (base + U256::from(2), U256::from_be_bytes(packed)),
            (base + U256::from(3), U256::from(1)), // scale
            (
                compute_mapping_slot(U256::from(153), &token_key),
                U256::from(1), // priceRatio
            ),
        ] {
            db.insert_account_storage(L2_TOKEN_REGISTRY_ADDRESS, slot, value)
                .unwrap();
        }

        db.insert_account_storage(
            FEE_TOKEN,
            compute_mapping_slot_for_address(U256::from(BALANCE_SLOT), SIGNER),
            U256::from(token_balance),
        )
        .unwrap();

        db
    }

    /// A token-fee MorphTx reserving [`TX_TOKEN_BUDGET`] tokens and no ETH.
    fn token_fee_tx(nonce: u64) -> MorphPooledTransaction {
        let tx = TxMorph {
            chain_id: 2818,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
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
    fn transactions_already_executed_by_the_block_do_not_consume_the_budget_again() {
        // The block executed nonce 0, which cost far less than the `TX_TOKEN_BUDGET` it
        // reserved, so the post-state still affords nonce 1 — but not both at max fee.
        let mut db = test_state(1, 0, TX_TOKEN_BUDGET + TX_TOKEN_BUDGET / 2);
        let (tx0, tx1) = (token_fee_tx(0), token_fee_tx(1));

        assert!(
            removable(&mut db, vec![&tx0, &tx1]).is_empty(),
            "nonce 1 is affordable against the post-state and nonce 0 is already mined"
        );
    }

    #[test]
    fn cumulative_budget_still_rejects_an_unaffordable_successor() {
        // Same balances, but the block did not execute nonce 0: both transactions are still
        // owed and the second one genuinely cannot be paid for.
        let mut db = test_state(0, 0, TX_TOKEN_BUDGET + TX_TOKEN_BUDGET / 2);
        let (tx0, tx1) = (token_fee_tx(0), token_fee_tx(1));

        assert_eq!(removable(&mut db, vec![&tx0, &tx1]), vec![*tx1.hash()]);
    }

    #[test]
    fn a_transaction_behind_a_nonce_gap_is_not_charged_to_the_budget() {
        // nonce 0 is executable and reserves the sender's whole token balance. nonce 10 sits
        // behind a gap, so nonces 1..9 — which are not in the pool — decide what is actually
        // left by the time it executes. Judging it against the residue of nonce 0 alone is
        // meaningless, and removing it on that basis destroys a transaction that passed
        // admission on its own.
        let mut db = test_state(0, 0, TX_TOKEN_BUDGET);
        let (tx0, gapped) = (token_fee_tx(0), token_fee_tx(10));

        assert!(
            removable(&mut db, vec![&tx0, &gapped]).is_empty(),
            "a nonce-gapped transaction has no meaningful cumulative budget"
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

    /// Fails every storage read of the fee token, leaving the rest of the state readable.
    #[derive(Debug)]
    struct UnreadableToken(CacheDB<EmptyDB>);

    impl reth_revm::Database for UnreadableToken {
        type Error = reth_provider::ProviderError;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(self.0.basic(address).unwrap())
        }

        fn code_by_hash(
            &mut self,
            code_hash: alloy_primitives::B256,
        ) -> Result<reth_revm::revm::bytecode::Bytecode, Self::Error> {
            Ok(self.0.code_by_hash(code_hash).unwrap())
        }

        fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
            if address == FEE_TOKEN {
                return Err(reth_provider::ProviderError::BestBlockNotFound);
            }
            Ok(self.0.storage(address, index).unwrap())
        }

        fn block_hash(&mut self, number: u64) -> Result<alloy_primitives::B256, Self::Error> {
            Ok(self.0.block_hash(number).unwrap())
        }
    }

    #[test]
    fn unreadable_token_state_does_not_remove_transactions() {
        let tx = token_fee_tx(0);
        let mut db = UnreadableToken(test_state(0, 0, 10 * TX_TOKEN_BUDGET));

        // Sanity check: the same transaction against readable state is kept as well, so the
        // assertion below is about the read failure and not about affordability.
        assert!(
            removable(&mut db.0.clone(), vec![&tx]).is_empty(),
            "transaction is affordable when the token balance can be read"
        );

        let to_remove = collect_removable_transactions(
            &mut db,
            &L1BlockInfo::default(),
            &test_evm_env(),
            30_000_000,
            vec![&tx],
        );
        assert!(
            to_remove.is_empty(),
            "a transient state-read failure must not be treated as an invalid transaction"
        );
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

        let token_key = token_id_key(TOKEN_ID);
        let base = compute_mapping_slot(U256::from(151), &token_key);
        let mut packed = [0u8; 32];
        packed[30] = 18;
        packed[31] = 1;
        client.add_account(
            L2_TOKEN_REGISTRY_ADDRESS,
            ExtendedAccount::new(0, U256::ZERO).extend_storage([
                (
                    storage_key(base),
                    U256::from_be_bytes(FEE_TOKEN.into_word().0),
                ),
                (
                    storage_key(base + U256::from(1)),
                    U256::from(BALANCE_SLOT + 1),
                ),
                (
                    storage_key(base + U256::from(2)),
                    U256::from_be_bytes(packed),
                ),
                (storage_key(base + U256::from(3)), U256::from(1)),
                (
                    storage_key(compute_mapping_slot(U256::from(153), &token_key)),
                    U256::from(1),
                ),
            ]),
        );
        set_token_balance(&client, token_balance);
        client
    }

    fn set_token_balance(client: &TestProvider, token_balance: u64) {
        client.add_account(
            FEE_TOKEN,
            ExtendedAccount::new(0, U256::ZERO).extend_storage([(
                storage_key(compute_mapping_slot_for_address(
                    U256::from(BALANCE_SLOT),
                    SIGNER,
                )),
                U256::from(token_balance),
            )]),
        );
    }

    /// A canonical commit of [`head_block`].
    fn commit_event() -> reth_provider::CanonStateNotification<MorphPrimitives> {
        let block = head_block();
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
    fn legacy_tx(nonce: u64) -> MorphPooledTransaction {
        let tx = TxLegacy {
            chain_id: Some(2818),
            nonce,
            gas_limit: 21_000,
            gas_price: 100,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            ..Default::default()
        };
        let recovered = Recovered::new_unchecked(
            MorphTxEnvelope::Legacy(Signed::new_unhashed(tx, Signature::test_signature())),
            SIGNER,
        );
        let encoded_len = recovered.encode_2718_len();
        MorphPooledTransaction::new(recovered, encoded_len)
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
        futures::executor::block_on(pool.add_transaction(
            reth_transaction_pool::TransactionOrigin::Local,
            token_fee_tx(0),
        ))
        .unwrap();
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
            client,
            MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone()),
            futures::stream::iter([event]),
        ));

        assert!(
            pool.get(&descendant).is_some(),
            "an independently affordable ETH-fee successor must be parked, not deleted"
        );
    }
}
