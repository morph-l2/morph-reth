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
//! 2. Re-validating all MorphTx transactions in the pool
//! 3. Removing transactions that no longer have sufficient token balance
//!
//! # Reference
//!
//! This is similar to how go-ethereum handles MorphTx in `promoteExecutables`
//! and `demoteUnexecutables` (tx_pool.go), but implemented as a separate
//! maintenance task since we cannot modify reth's internal pool logic.

use alloy_consensus::Transaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, TxHash, U256};
use futures::StreamExt;
use morph_chainspec::hardfork::MorphHardforks;
use morph_primitives::MorphTxEnvelope;
use morph_revm::L1BlockInfo;
use reth_chainspec::ChainSpecProvider;
use reth_primitives_traits::AlloyBlockHeader;
use reth_provider::CanonStateSubscriptions;
use reth_revm::Database;
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{EthPoolTransaction, PoolTransaction, TransactionPool};
use std::collections::{HashMap, HashSet};

/// Sender-level rolling affordability budget used during maintenance revalidation.
#[derive(Debug, Clone, Default)]
struct SenderBudget {
    /// Remaining ETH budget for this sender.
    eth_balance: U256,
    /// Remaining token budget per `fee_token_id`.
    token_balances: HashMap<u16, U256>,
}

/// Applies cumulative sender-budget checks and consumes budget on success.
///
/// Returns `true` if the transaction can be afforded under the current rolling budget.
#[allow(clippy::too_many_arguments)]
fn consume_sender_budget(
    budget: &mut SenderBudget,
    uses_token_fee: bool,
    tx_value: U256,
    gas_limit: u64,
    max_fee_per_gas: u128,
    l1_data_fee: U256,
    token_id: Option<u16>,
    fee_limit: Option<U256>,
    required_token_amount: U256,
    state_token_balance: Option<U256>,
) -> bool {
    if uses_token_fee {
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
        return true;
    }

    // ETH-fee path: consume full tx ETH cost (value + gas fee + l1_data_fee).
    let gas_fee = U256::from(gas_limit).saturating_mul(U256::from(max_fee_per_gas));
    let total_eth_cost = gas_fee.saturating_add(l1_data_fee).saturating_add(tx_value);
    if total_eth_cost > budget.eth_balance {
        return false;
    }
    budget.eth_balance = budget.eth_balance.saturating_sub(total_eth_cost);
    true
}

/// Maintains the Morph transaction pool by revalidating MorphTx transactions.
///
/// This task runs continuously and:
/// - Listens for new canonical blocks
/// - Re-validates MorphTx (0x7F) transactions in the pool
/// - Removes transactions that no longer have sufficient token balance
///
pub async fn maintain_morph_pool<Pool, Client>(pool: Pool, client: Client)
where
    Pool: TransactionPool + Clone,
    Pool::Transaction: EthPoolTransaction<Consensus = MorphTxEnvelope>,
    Client: ChainSpecProvider<ChainSpec: MorphHardforks>
        + StateProviderFactory
        + CanonStateSubscriptions
        + Clone
        + 'static,
{
    let mut chain_events = client.canonical_state_stream();

    tracing::info!(target: "morph_txpool::maintain", "Starting MorphTx maintenance task");

    loop {
        // Wait for the next canonical state change
        let Some(event) = chain_events.next().await else {
            tracing::debug!(target: "morph_txpool::maintain", "Chain event stream ended");
            break;
        };

        let new_tip = event.tip();
        let block_number = new_tip.number();
        let block_timestamp = new_tip.timestamp();

        tracing::trace!(
            target: "morph_txpool::maintain",
            block_number,
            "Processing new block for MorphTx validation"
        );

        // Get the hardfork at this block
        let hardfork = client
            .chain_spec()
            .morph_hardfork_at(block_number, block_timestamp);

        // Collect all MorphTx transactions from the pool
        let all_txs = pool.all_transactions();
        let morph_txs: Vec<_> = all_txs
            .pending
            .iter()
            .chain(all_txs.queued.iter())
            .filter(|tx| tx.transaction.clone_into_consensus().is_morph_tx())
            .collect();

        if morph_txs.is_empty() {
            continue;
        }

        // Get state provider for the new tip
        let state_provider = match client.state_by_block_hash(new_tip.hash()) {
            Ok(provider) => provider,
            Err(err) => {
                tracing::warn!(
                    target: "morph_txpool::maintain",
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
                    target: "morph_txpool::maintain",
                    ?err,
                    "Failed to fetch L1 block info for MorphTx revalidation"
                );
                continue;
            }
        };

        tracing::trace!(
            target: "morph_txpool::maintain",
            count = morph_txs.len(),
            "Revalidating MorphTx transactions"
        );

        // Group by sender and process in nonce order so affordability is validated cumulatively.
        let mut txs_by_sender: HashMap<Address, Vec<_>> = HashMap::new();
        for pooled_tx in morph_txs {
            let sender = pooled_tx.transaction.sender();
            txs_by_sender.entry(sender).or_default().push(pooled_tx);
        }

        // Revalidate each sender's MorphTx set and collect invalid ones
        let mut to_remove: HashSet<TxHash> = HashSet::new();

        for (sender, mut sender_txs) in txs_by_sender {
            sender_txs
                .sort_by_key(|pooled_tx| pooled_tx.transaction.clone_into_consensus().nonce());

            // Initialize sender ETH budget once.
            let eth_balance = match db.basic(sender) {
                Ok(Some(account)) => account.balance,
                Ok(None) => U256::ZERO,
                Err(err) => {
                    tracing::warn!(
                        target: "morph_txpool::maintain",
                        ?sender,
                        ?err,
                        "Failed to get account balance"
                    );
                    continue;
                }
            };

            let mut budget = SenderBudget {
                eth_balance,
                token_balances: HashMap::new(),
            };

            for pooled_tx in sender_txs {
                let tx = &pooled_tx.transaction;
                let consensus_tx = tx.clone_into_consensus();

                // Calculate L1 data fee for this transaction.
                let mut encoded = Vec::with_capacity(consensus_tx.encode_2718_len());
                consensus_tx.encode_2718(&mut encoded);
                let l1_data_fee = l1_block_info.calculate_tx_l1_cost(&encoded, hardfork);

                // Use shared validation logic first with current sender ETH budget.
                let input = crate::MorphTxValidationInput {
                    consensus_tx: &consensus_tx,
                    sender,
                    eth_balance: budget.eth_balance,
                    l1_data_fee,
                    hardfork,
                };

                let validation = match crate::validate_morph_tx(&mut db, &input) {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::debug!(
                            target: "morph_txpool::maintain",
                            tx_hash = ?tx.hash(),
                            ?sender,
                            ?err,
                            "Removing MorphTx: validation failed"
                        );
                        to_remove.insert(*tx.hash());
                        continue;
                    }
                };

                let fields = consensus_tx.morph_fields();
                let state_token_balance = validation.token_info.as_ref().map(|info| info.balance);
                let token_id = fields.as_ref().map(|f| f.fee_token_id);
                let fee_limit = fields.as_ref().map(|f| f.fee_limit);

                if !consume_sender_budget(
                    &mut budget,
                    validation.uses_token_fee,
                    consensus_tx.value(),
                    consensus_tx.gas_limit(),
                    consensus_tx.max_fee_per_gas(),
                    l1_data_fee,
                    token_id,
                    fee_limit,
                    validation.required_token_amount,
                    state_token_balance,
                ) {
                    tracing::debug!(
                        target: "morph_txpool::maintain",
                        tx_hash = ?tx.hash(),
                        ?sender,
                        uses_token_fee = validation.uses_token_fee,
                        token_id = ?token_id,
                        required_token_amount = ?validation.required_token_amount,
                        "Removing MorphTx: insufficient cumulative sender budget"
                    );
                    to_remove.insert(*tx.hash());
                    continue;
                }
            }
        }

        // Remove invalid transactions
        if !to_remove.is_empty() {
            let count = to_remove.len();
            let hashes: Vec<_> = to_remove.into_iter().collect();
            pool.remove_transactions(hashes);
            tracing::info!(
                target: "morph_txpool::maintain",
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

        let first = consume_sender_budget(
            &mut budget,
            false,
            U256::from(20u64),
            10,
            3,
            U256::from(5u64),
            None,
            None,
            U256::ZERO,
            None,
        );
        assert!(first);
        // total cost = value(20) + gas(30) + l1(5) = 55
        assert_eq!(budget.eth_balance, U256::from(45u64));

        let second = consume_sender_budget(
            &mut budget,
            false,
            U256::from(20u64),
            10,
            3,
            U256::from(5u64),
            None,
            None,
            U256::ZERO,
            None,
        );
        assert!(!second);
        assert_eq!(budget.eth_balance, U256::from(45u64));
    }

    #[test]
    fn consume_token_fee_path_tracks_cumulative_token_budget() {
        let mut budget = SenderBudget {
            eth_balance: U256::from(10u64),
            token_balances: HashMap::new(),
        };

        let first = consume_sender_budget(
            &mut budget,
            true,
            U256::ZERO,
            1,
            1,
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

        let second = consume_sender_budget(
            &mut budget,
            true,
            U256::ZERO,
            1,
            1,
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
        let limited = consume_sender_budget(
            &mut budget,
            true,
            U256::ZERO,
            1,
            1,
            U256::ZERO,
            Some(9),
            Some(U256::from(30u64)),
            U256::from(40u64),
            Some(U256::from(100u64)),
        );
        assert!(!limited);

        // Enough token, but ETH value exceeds remaining ETH budget => reject
        let eth_value_fail = consume_sender_budget(
            &mut budget,
            true,
            U256::from(6u64),
            1,
            1,
            U256::ZERO,
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
        let tx1 = consume_sender_budget(
            &mut budget,
            true,
            U256::from(10u64), // value in ETH
            1,
            1,
            U256::ZERO,
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
        let tx2 = consume_sender_budget(
            &mut budget,
            false,
            U256::from(20u64), // value
            5,                 // gas_limit
            4,                 // max_fee_per_gas => gas fee = 20
            U256::from(10u64), // l1 fee
            None,
            None,
            U256::ZERO,
            None,
        );
        assert!(tx2);
        // total eth cost = 20(value) + 20(gas) + 10(l1) = 50
        assert_eq!(budget.eth_balance, U256::from(40u64));

        // Tx3: token-fee path should now fail because remaining token budget is only 30.
        let tx3 = consume_sender_budget(
            &mut budget,
            true,
            U256::ZERO,
            1,
            1,
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
}
