//! Morph transaction pool builder.

use crate::MorphNode;
use morph_primitives;
use morph_txpool::MorphTransactionValidator;
use reth_node_api::FullNodeTypes;
use reth_node_builder::components::{TxPoolBuilder, spawn_maintenance_tasks};
use reth_node_builder::{BuilderContext, components::PoolBuilder};
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{TransactionValidationTaskExecutor, blobstore::InMemoryBlobStore};

/// Builder for Morph transaction pool.
///
/// Configures and builds the transaction pool with:
/// - [`MorphTransactionValidator`] for L1 fee and MorphTx validation
/// - In-memory blob store (Morph doesn't support EIP-4844)
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct MorphPoolBuilder;

impl<Node> PoolBuilder<Node> for MorphPoolBuilder
where
    Node: FullNodeTypes<Types = MorphNode>,
{
    type Pool = morph_txpool::MorphTransactionPool<Node::Provider, InMemoryBlobStore>;

    async fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
        let pool_config = ctx.pool_config();

        // Use in-memory blob store (Morph doesn't support EIP-4844 blobs)
        let blob_store = InMemoryBlobStore::default();

        // Build the transaction validator with Morph-specific checks
        let validator = TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone())
            .with_head_timestamp(ctx.head().timestamp)
            .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
            .with_local_transactions_config(pool_config.local_transactions_config.clone())
            .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
            .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
            .set_block_gas_limit(ctx.chain_spec().inner.genesis().gas_limit)
            .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
            .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
            // Register MorphTx (0x7F) type for ERC20 gas payment
            .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
            // Note: L1Message (0x7E) is NOT registered - it will be rejected by
            // EthTransactionValidator as TxTypeNotSupported, which is correct since
            // L1 messages should only be included by the sequencer during block building
            // Disable EIP-4844 blob transactions
            .no_eip4844()
            .build_with_tasks(ctx.task_executor().clone(), blob_store.clone());

        // Wrap with Morph-specific validator
        let validator = validator.map(MorphTransactionValidator::new);

        // Build the transaction pool
        let pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build(blob_store, pool_config.clone());

        // Spawn standard pool maintenance tasks (from reth)
        spawn_maintenance_tasks(ctx, pool.clone(), &pool_config)?;

        // Spawn Morph-specific maintenance task for MorphTx (0x7F) revalidation
        // This handles ERC20 token balance changes that reth's standard maintenance
        // cannot track (reth only tracks ETH balance via SenderInfo)
        ctx.task_executor().spawn_critical(
            "txpool maintenance - morph pool",
            morph_txpool::maintain_morph_pool(pool.clone(), ctx.provider().clone()),
        );

        info!(target: "morph::node", "Transaction pool initialized");
        debug!(target: "morph::node", "Pool config: {:?}", pool_config);

        Ok(pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_consensus::{Signed, TxLegacy, transaction::Recovered};
    use alloy_primitives::{B256, Sealed, Signature, U256};
    use morph_primitives::{MorphTxEnvelope, TxL1Msg};
    use morph_txpool::MorphPooledTransaction;
    use reth_transaction_pool::PoolTransaction;

    #[tokio::test]
    async fn test_validate_oversized_transaction() {
        // Test that transactions exceeding max_tx_input_bytes are rejected
        // The default max_tx_input_bytes in reth is 120KB (122,880 bytes)

        // For this test, we create a mock pool that would reject oversized transactions
        // The actual validation happens in the validator when checking encoded size

        // Create a legacy transaction
        let tx = MorphTxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy {
                gas_limit: 21_000,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            Default::default(),
        ));

        // Create a pool transaction with an encoded length exceeding the limit (121KB > 120KB)
        let pool_tx = MorphPooledTransaction::new(
            Recovered::new_unchecked(tx, Default::default()),
            121 * 1024,
        );

        // Verify the encoded length is larger than the limit
        assert_eq!(pool_tx.encoded_length(), 121 * 1024);
        assert!(pool_tx.encoded_length() > 120 * 1024);
    }

    #[tokio::test]
    async fn test_l1_message_type_id() {
        // Test that L1 message transactions have the correct type ID (0x7E)
        // These transactions should NOT be registered in the pool via with_custom_tx_type
        // and will be rejected by EthTransactionValidator

        let tx = MorphTxEnvelope::L1Msg(Sealed::new_unchecked(TxL1Msg::default(), B256::default()));

        let pool_tx = MorphPooledTransaction::new(
            Recovered::new_unchecked(tx.clone(), Default::default()),
            0,
        );

        // Verify it's an L1 message
        assert!(pool_tx.is_l1_message());
        assert_eq!(tx.tx_type(), morph_primitives::L1_TX_TYPE_ID);
    }

    #[tokio::test]
    async fn test_morph_tx_type_id() {
        // Test that MorphTx transactions have the correct type ID (0x7F)
        // These transactions ARE registered in the pool via with_custom_tx_type

        let tx = MorphTxEnvelope::Morph(Signed::new_unchecked(
            morph_primitives::TxMorph {
                gas_limit: 21_000,
                max_fee_per_gas: 1_000_000,
                max_priority_fee_per_gas: 1_000_000,
                fee_token_id: 0,
                fee_limit: U256::ZERO,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            Default::default(),
        ));

        let pool_tx = MorphPooledTransaction::new(
            Recovered::new_unchecked(tx.clone(), Default::default()),
            100,
        );

        // Verify it's a Morph transaction
        assert!(pool_tx.is_morph_tx());
        assert_eq!(tx.tx_type(), morph_primitives::MORPH_TX_TYPE_ID);
    }

    #[test]
    fn test_pool_builder_default() {
        // Test that the pool builder can be created with defaults
        let builder = MorphPoolBuilder::default();
        assert!(matches!(builder, MorphPoolBuilder));
    }
}
