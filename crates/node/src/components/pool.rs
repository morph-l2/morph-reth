//! Morph transaction pool builder.

use crate::MorphNode;
use alloy_consensus::BlockHeader;
use morph_evm::MorphEvmConfig;
use morph_primitives;
use morph_txpool::MorphTransactionValidator;
use reth_node_api::FullNodeTypes;
use reth_node_builder::components::{TxPoolBuilder, spawn_maintenance_tasks};
use reth_node_builder::{BuilderContext, components::PoolBuilder};
use reth_provider::BlockReaderIdExt;
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{
    PoolConfig, TransactionValidationTaskExecutor, blobstore::InMemoryBlobStore,
};

/// Resolves the initial block gas limit for the transaction pool and validator from
/// the latest canonical header, falling back to genesis only if no latest header exists.
fn initial_block_gas_limit<Provider>(provider: &Provider, genesis_gas_limit: u64) -> u64
where
    Provider: BlockReaderIdExt,
{
    provider
        .latest_header()
        .ok()
        .flatten()
        .map(|header| header.gas_limit())
        .unwrap_or(genesis_gas_limit)
}

/// Synchronizes `pool_config.gas_limit` with the resolved initial block gas limit so
/// `AllTransactions` does not reject transactions above upstream `PoolConfig::default()`'s
/// hardcoded 30M limit before the first canonical head update arrives.
fn configure_initial_pool_config<Provider>(
    mut pool_config: PoolConfig,
    provider: &Provider,
    genesis_gas_limit: u64,
) -> (PoolConfig, u64)
where
    Provider: BlockReaderIdExt,
{
    let initial_gas_limit = initial_block_gas_limit(provider, genesis_gas_limit);
    pool_config.gas_limit = initial_gas_limit;
    (pool_config, initial_gas_limit)
}

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
        // Initialize both `pool_config.gas_limit` (enforced by `AllTransactions::insert_tx`)
        // and the validator's `block_gas_limit` from the latest canonical header (falling back
        // to genesis on an empty DB) so transactions above upstream `PoolConfig::default()`'s
        // hardcoded 30M limit are accepted immediately on startup before a new block arrives.
        let (pool_config, initial_gas_limit) = configure_initial_pool_config(
            ctx.pool_config(),
            ctx.provider(),
            ctx.chain_spec().inner.genesis().gas_limit,
        );
        let max_tx_gas_limit = ctx.config().txpool.max_tx_gas_limit;

        // Use in-memory blob store (Morph doesn't support EIP-4844 blobs)
        let blob_store = InMemoryBlobStore::default();

        // Build the Morph-specific EVM config for the validator and the maintenance task
        let morph_evm_config =
            MorphEvmConfig::new(ctx.chain_spec(), morph_evm::MorphEvmFactory::default());

        // Build the transaction validator with Morph-specific checks
        let validator = TransactionValidationTaskExecutor::eth_builder(
            ctx.provider().clone(),
            morph_evm_config.clone(),
        )
        .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
        .with_local_transactions_config(pool_config.local_transactions_config.clone())
        .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
        .with_max_tx_gas_limit(max_tx_gas_limit)
        .set_block_gas_limit(initial_gas_limit)
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
        let validator = validator.map(|inner| {
            MorphTransactionValidator::new(inner).with_max_tx_gas_limit(max_tx_gas_limit)
        });

        // Build the transaction pool
        let pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build(blob_store, pool_config.clone());

        // Spawn standard pool maintenance tasks (from reth)
        spawn_maintenance_tasks(ctx, pool.clone(), &pool_config)?;

        // Revalidate L1 fees for all senders and ERC20 balances for MorphTx (0x7F).
        // Reth's standard maintenance only tracks ETH costs without L1 data fees.
        ctx.task_executor().spawn_critical_blocking_task(
            "txpool maintenance - morph pool",
            morph_txpool::maintain_morph_pool(
                pool.clone(),
                ctx.provider().clone(),
                morph_evm_config,
            ),
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

    #[test]
    fn test_initial_block_gas_limit_falls_back_to_genesis_without_latest_header() {
        let provider = reth_provider::noop::NoopProvider::default();
        assert_eq!(PoolConfig::default().gas_limit, 30_000_000);

        let (pool_config, initial_gas_limit) =
            configure_initial_pool_config(PoolConfig::default(), &provider, 60_000_000);
        assert_eq!(initial_gas_limit, 60_000_000);
        assert_eq!(pool_config.gas_limit, 60_000_000);
    }

    #[cfg(feature = "test-utils")]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_configure_initial_pool_config_uses_canonical_tip_above_30m_genesis()
    -> eyre::Result<()> {
        use crate::test_utils::{TestNodeBuilder, advance_empty_block};
        use alloy_consensus::{SignableTransaction, TxEip1559};
        use alloy_eips::eip2718::Encodable2718;
        use alloy_primitives::{Address, Bytes, TxKind};
        use alloy_signer::SignerSync;
        use reth_chainspec::ChainSpecProvider;
        use reth_payload_primitives::BuiltPayload;
        use reth_transaction_pool::{CoinbaseTipOrdering, Pool, TransactionPool};

        let (mut nodes, wallet) = TestNodeBuilder::new()
            .with_gas_limit(30_000_000)
            .with_desired_gas_limit(60_000_000)
            .build()
            .await?;
        let mut node = nodes.pop().unwrap();

        // Advance 1 block so canonical tip gas_limit ramps above both genesis (30M)
        // and `PoolConfig::default().gas_limit` (30M).
        let payload = advance_empty_block(&mut node).await?;
        let tip_gas_limit = payload.block().header().gas_limit();
        assert!(
            tip_gas_limit > 30_000_000,
            "tip gas limit should ramp above 30M, got {tip_gas_limit}"
        );

        let genesis_gas_limit = node.inner.provider.chain_spec().inner.genesis().gas_limit;
        assert_eq!(genesis_gas_limit, 30_000_000);

        // Simulate node restart pool initialization BEFORE `spawn_maintenance_tasks` runs:
        let (pool_config, initial_gas_limit) = configure_initial_pool_config(
            PoolConfig::default(),
            &node.inner.provider,
            genesis_gas_limit,
        );
        assert_eq!(initial_gas_limit, tip_gas_limit);
        assert_eq!(pool_config.gas_limit, tip_gas_limit);

        let blob_store = InMemoryBlobStore::default();
        let evm_config = MorphEvmConfig::new(
            node.inner.provider.chain_spec(),
            morph_evm::MorphEvmFactory::default(),
        );
        let validator =
            TransactionValidationTaskExecutor::eth_builder(node.inner.provider.clone(), evm_config)
                .set_block_gas_limit(initial_gas_limit)
                .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
                .disable_balance_check()
                .no_eip4844()
                .build_with_tasks(node.inner.task_executor.clone(), blob_store.clone())
                .map(MorphTransactionValidator::new);

        // Build `Pool` directly without spawning `maintain_transaction_pool_future`,
        // proving `AllTransactions::insert_tx` accepts `30M < gas <= tip_gas_limit`
        // solely from `pool_config.gas_limit = initial_gas_limit`.
        let restarted_pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            blob_store,
            pool_config,
        );
        assert_eq!(restarted_pool.block_info().block_gas_limit, tip_gas_limit);

        let tx_gas = 30_000_000 + (tip_gas_limit - 30_000_000) / 2;
        let tx = TxEip1559 {
            chain_id: wallet.chain_id,
            nonce: 0,
            gas_limit: tx_gas,
            max_fee_per_gas: 20_000_000_000u128,
            max_priority_fee_per_gas: 1_000_000_000u128,
            to: TxKind::Call(Address::with_last_byte(0x42)),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let sig = wallet
            .inner
            .sign_hash_sync(&tx.signature_hash())
            .map_err(|e| eyre::eyre!("{e}"))?;
        let signed = MorphTxEnvelope::Eip1559(tx.into_signed(sig));
        let len = signed.encode_2718_len();
        let pooled = MorphPooledTransaction::new(
            Recovered::new_unchecked(signed, wallet.inner.address()),
            len,
        );

        restarted_pool
            .add_external_transaction(pooled)
            .await
            .expect("tx between 30M genesis and ramped tip must be admitted before maintenance");
        assert_eq!(restarted_pool.pending_transactions().len(), 1);

        // Also verify disk-backed `backup_local_transactions_task` journal replay (which
        // `spawn_maintenance_tasks` spawns *before* `maintain_transaction_pool_future`):
        // - Unpatched `PoolConfig::default()` silently drops a >30M local tx and deletes the
        //   journal file.
        // - `configure_initial_pool_config` replays and admits both a >30M local tx and a
        //   calldata-heavy local tx below EIP-7623 floor gas before maintenance ever runs.
        use reth_transaction_pool::{
            TransactionOrigin,
            maintain::{LocalTransactionBackupConfig, TxBackup, backup_local_transactions_task},
        };

        let make_backup_rlp = |nonce: u64, gas_limit: u64, input: Bytes| -> eyre::Result<Bytes> {
            let tx = TxEip1559 {
                chain_id: wallet.chain_id,
                nonce,
                gas_limit,
                max_fee_per_gas: 20_000_000_000u128,
                max_priority_fee_per_gas: 1_000_000_000u128,
                to: TxKind::Call(Address::with_last_byte(0x42)),
                value: U256::ZERO,
                access_list: Default::default(),
                input,
            };
            let sig = wallet
                .inner
                .sign_hash_sync(&tx.signature_hash())
                .map_err(|e| eyre::eyre!("{e}"))?;
            Ok(MorphTxEnvelope::Eip1559(tx.into_signed(sig))
                .encoded_2718()
                .into())
        };

        let backup_entries_json = serde_json::to_vec(&vec![
            TxBackup {
                rlp: make_backup_rlp(0, tx_gas, Bytes::new())?,
                origin: TransactionOrigin::Local,
            },
            TxBackup {
                rlp: make_backup_rlp(1, 40_000, Bytes::from(vec![0xab; 1_024]))?,
                origin: TransactionOrigin::Local,
            },
        ])?;

        // 1. Control: unpatched `PoolConfig::default()` drops nonce 0 (>30M) during journal
        //    replay and deletes the backup file!
        let temp_dir = tempfile::tempdir()?;
        let unpatched_backup_path = temp_dir.path().join("unpatched_txs.json");
        std::fs::write(&unpatched_backup_path, &backup_entries_json)?;
        let unpatched_evm = MorphEvmConfig::new(
            node.inner.provider.chain_spec(),
            morph_evm::MorphEvmFactory::default(),
        );
        let unpatched_blob_store = InMemoryBlobStore::default();
        let unpatched_validator = TransactionValidationTaskExecutor::eth_builder(
            node.inner.provider.clone(),
            unpatched_evm,
        )
        .set_block_gas_limit(initial_gas_limit)
        .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
        .disable_balance_check()
        .no_eip4844()
        .build_with_tasks::<MorphPooledTransaction, _>(
            node.inner.task_executor.clone(),
            unpatched_blob_store.clone(),
        )
        .map(MorphTransactionValidator::new);
        let unpatched_pool = Pool::new(
            unpatched_validator,
            CoinbaseTipOrdering::default(),
            unpatched_blob_store,
            PoolConfig::default(),
        );
        let unpatched_rt = reth_tasks::Runtime::test();
        let unpatched_pool_for_task = unpatched_pool.clone();
        let unpatched_cfg =
            LocalTransactionBackupConfig::with_local_txs_backup(unpatched_backup_path.clone());
        unpatched_rt.spawn_critical_with_graceful_shutdown_signal("unpatched-backup", |shutdown| {
            backup_local_transactions_task(shutdown, unpatched_pool_for_task, unpatched_cfg)
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while unpatched_backup_path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert!(
            unpatched_pool.pending_transactions().is_empty(),
            "unpatched PoolConfig::default() drops >30M backed-up tx (and strands nonce 1)"
        );
        unpatched_rt.graceful_shutdown();

        // 2. Patched `configure_initial_pool_config`: replays and admits BOTH local txs into
        //    pending immediately before `maintain_transaction_pool_future` ever runs!
        let patched_backup_path = temp_dir.path().join("patched_txs.json");
        std::fs::write(&patched_backup_path, &backup_entries_json)?;
        let (patched_pool_config, patched_gas_limit) = configure_initial_pool_config(
            PoolConfig::default(),
            &node.inner.provider,
            genesis_gas_limit,
        );
        let patched_evm = MorphEvmConfig::new(
            node.inner.provider.chain_spec(),
            morph_evm::MorphEvmFactory::default(),
        );
        let patched_blob_store = InMemoryBlobStore::default();
        let patched_validator = TransactionValidationTaskExecutor::eth_builder(
            node.inner.provider.clone(),
            patched_evm,
        )
        .set_block_gas_limit(patched_gas_limit)
        .with_custom_tx_type(morph_primitives::MORPH_TX_TYPE_ID)
        .disable_balance_check()
        .no_eip4844()
        .build_with_tasks::<MorphPooledTransaction, _>(
            node.inner.task_executor.clone(),
            patched_blob_store.clone(),
        )
        .map(MorphTransactionValidator::new);
        let patched_pool = Pool::new(
            patched_validator,
            CoinbaseTipOrdering::default(),
            patched_blob_store,
            patched_pool_config,
        );
        let patched_rt = reth_tasks::Runtime::test();
        let patched_pool_for_task = patched_pool.clone();
        let patched_cfg =
            LocalTransactionBackupConfig::with_local_txs_backup(patched_backup_path.clone());
        patched_rt.spawn_critical_with_graceful_shutdown_signal("patched-backup", |shutdown| {
            backup_local_transactions_task(shutdown, patched_pool_for_task, patched_cfg)
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while patched_backup_path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(
            patched_pool.pending_transactions().len(),
            2,
            "both >30M and calldata-heavy local txs must be replayed into pending before maintenance"
        );
        patched_rt.graceful_shutdown();

        Ok(())
    }
}
