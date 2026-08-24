//! Morph transaction pool builder.

use crate::MorphNode;
use morph_evm::MorphEvmConfig;
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

impl<Node, Evm> PoolBuilder<Node, Evm> for MorphPoolBuilder
where
    Node: FullNodeTypes<Types = MorphNode>,
    Evm: Send,
{
    type Pool = morph_txpool::MorphTransactionPool<
        Node::Provider,
        InMemoryBlobStore,
        morph_txpool::MorphPooledTransaction,
        MorphEvmConfig,
    >;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        _evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let pool_config = ctx.pool_config();

        // Use in-memory blob store (Morph doesn't support EIP-4844 blobs)
        let blob_store = InMemoryBlobStore::default();

        // Build the Morph-specific EVM config for the validator
        let morph_evm_config =
            MorphEvmConfig::new(ctx.chain_spec(), morph_evm::MorphEvmFactory::default());

        // Build the transaction validator with Morph-specific checks
        let validator = TransactionValidationTaskExecutor::eth_builder(
            ctx.provider().clone(),
            morph_evm_config,
        )
        .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
        .with_local_transactions_config(pool_config.local_transactions_config.clone())
        .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
        .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
        .set_block_gas_limit(ctx.chain_spec().inner.genesis().gas_limit)
        .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
        .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
        // Register MorphTx (0x7F) type for ERC20 gas payment
        .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
        // Disable the inner EthTransactionValidator's balance check.
        // MorphTx (fee_token_id > 0) users may have zero ETH but pay gas in ERC20 tokens.
        // Without this, the inner validator rejects them before reaching MorphTransactionValidator's
        // token fee validation. The MorphTransactionValidator already performs its own balance
        // checks for all tx types (including L1 data fee), so this is safe.
        .disable_balance_check()
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
        ctx.task_executor().spawn_critical_task(
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
    use reth_transaction_pool::{PoolTransaction, validate::DEFAULT_MAX_TX_INPUT_BYTES};

    #[tokio::test]
    async fn test_validate_oversized_transaction() {
        // `DEFAULT_MAX_TX_INPUT_BYTES` is 4 * TX_SLOT_BYTE_SIZE = 128 KiB. Despite the
        // "input" in the name, for non-blob transactions the validator compares it against
        // the full 2718-encoded length, which matches go-ethereum's `txMaxSize`.
        //
        // This test only covers the size the pool transaction reports; the rejection itself
        // lives in the validator, which needs a full provider to exercise.

        // Create a legacy transaction
        let tx = MorphTxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy {
                gas_limit: 21_000,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            Default::default(),
        ));

        // Create a pool transaction one byte over the limit
        let pool_tx = MorphPooledTransaction::new(
            Recovered::new_unchecked(tx, Default::default()),
            DEFAULT_MAX_TX_INPUT_BYTES + 1,
        );

        assert!(pool_tx.encoded_length() > DEFAULT_MAX_TX_INPUT_BYTES);
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
