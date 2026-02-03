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

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{TxHash, U256};
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
use std::collections::HashSet;

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

        // Revalidate each MorphTx and collect invalid ones
        let mut to_remove: HashSet<TxHash> = HashSet::new();

        for pooled_tx in morph_txs {
            let tx = &pooled_tx.transaction;
            let consensus_tx = tx.clone_into_consensus();
            let sender = tx.sender();

            // Get ETH balance for tx.value() check
            let eth_balance = match db.basic(sender) {
                Ok(Some(account)) => account.balance,
                Ok(None) => U256::ZERO,
                Err(err) => {
                    tracing::warn!(
                        target: "morph_txpool::maintain",
                        tx_hash = ?tx.hash(),
                        ?err,
                        "Failed to get account balance"
                    );
                    continue;
                }
            };

            // Calculate L1 data fee
            let mut encoded = Vec::with_capacity(consensus_tx.encode_2718_len());
            consensus_tx.encode_2718(&mut encoded);
            let l1_data_fee = l1_block_info.calculate_tx_l1_cost(&encoded, hardfork);

            // Use shared validation logic (includes ETH balance >= tx.value() check)
            let input = crate::MorphTxValidationInput {
                consensus_tx: &consensus_tx,
                sender,
                eth_balance,
                l1_data_fee,
                hardfork,
            };

            if let Err(err) = crate::validate_morph_tx(&mut db, &input) {
                tracing::debug!(
                    target: "morph_txpool::maintain",
                    tx_hash = ?tx.hash(),
                    ?err,
                    "Removing MorphTx: validation failed"
                );
                to_remove.insert(*tx.hash());
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
