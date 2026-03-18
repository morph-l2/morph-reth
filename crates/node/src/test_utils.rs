//! Test utilities for Morph node E2E testing.
//!
//! Provides helpers for setting up an ephemeral Morph node with an in-memory
//! database, creating payload attributes, and advancing the chain — following
//! the same pattern as scroll-reth's `test_utils`.

use crate::MorphNode;
use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use morph_payload_types::MorphPayloadBuilderAttributes;
use reth_e2e_test_utils::{
    NodeHelperType, TmpDB, transaction::TransactionTestContext, wallet::Wallet,
};
use reth_node_api::NodeTypesWithDBAdapter;
use reth_payload_builder::EthPayloadBuilderAttributes;
use reth_provider::providers::BlockchainProvider;
use reth_tasks::TaskManager;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Morph Node Helper type alias for E2E tests.
pub type MorphTestNode =
    NodeHelperType<MorphNode, BlockchainProvider<NodeTypesWithDBAdapter<MorphNode, TmpDB>>>;

/// Creates an ephemeral Morph node for E2E testing.
///
/// This spins up a fully functional Morph node with an in-memory database,
/// connected to other nodes if `num_nodes > 1`. Follows scroll-reth's
/// `setup()` pattern.
pub async fn setup(
    num_nodes: usize,
    is_dev: bool,
) -> eyre::Result<(Vec<MorphTestNode>, TaskManager, Wallet)> {
    // Build a minimal test genesis with all Morph hardforks activated at genesis.
    let genesis: Genesis = serde_json::from_value(serde_json::json!({
        "config": {
            "chainId": 2910,
            "homesteadBlock": 0,
            "eip150Block": 0,
            "eip155Block": 0,
            "eip158Block": 0,
            "byzantiumBlock": 0,
            "constantinopleBlock": 0,
            "petersburgBlock": 0,
            "istanbulBlock": 0,
            "berlinBlock": 0,
            "londonBlock": 0,
            "shanghaiTime": 0,
            "cancunTime": 0,
            "pragueTime": 0,
            "bernoulliBlock": 0,
            "curieBlock": 0,
            "morph203Time": 0,
            "viridianTime": 0,
            "emeraldTime": 0,
            "jadeTime": 0,
            "morph": {
                "feeVaultAddress": "0x4200000000000000000000000000000000000011"
            }
        },
        "nonce": "0x0",
        "timestamp": "0x0",
        "extraData": "0x",
        "gasLimit": "0x1c9c380",
        "difficulty": "0x0",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "coinbase": "0x4200000000000000000000000000000000000011",
        "alloc": {
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266": {
                "balance": "0x200000000000000000000000000000000000000000000000000000000000000"
            }
        },
        "baseFeePerGas": "0xf4240"
    }))?;

    let chain_spec = morph_chainspec::MorphChainSpec::from_genesis(genesis);

    reth_e2e_test_utils::setup_engine(
        num_nodes,
        Arc::new(chain_spec),
        is_dev,
        Default::default(),
        morph_payload_attributes,
    )
    .await
}

/// Advance the chain by `length` blocks, each containing one transfer transaction.
///
/// Returns the built payloads for inspection.
pub async fn advance_chain(
    length: usize,
    node: &mut MorphTestNode,
    wallet: Arc<Mutex<Wallet>>,
) -> eyre::Result<Vec<morph_payload_types::MorphBuiltPayload>> {
    node.advance(length as u64, |_| {
        let wallet = wallet.clone();
        Box::pin(async move {
            let mut wallet = wallet.lock().await;
            let nonce = wallet.inner_nonce;
            wallet.inner_nonce += 1;
            transfer_tx_with_nonce(wallet.chain_id, wallet.inner.clone(), nonce).await
        })
    })
    .await
}

/// Creates a signed transfer transaction with an explicit nonce.
///
/// The morph reth fork does not include `transfer_tx_nonce_bytes`, so we
/// build the transaction request manually and delegate signing to the
/// framework's `TransactionTestContext::sign_tx`.
async fn transfer_tx_with_nonce(chain_id: u64, signer: PrivateKeySigner, nonce: u64) -> Bytes {
    let tx = TransactionRequest {
        nonce: Some(nonce),
        value: Some(U256::from(100)),
        to: Some(TxKind::Call(Address::random())),
        gas: Some(21000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        chain_id: Some(chain_id),
        ..Default::default()
    };
    let signed = TransactionTestContext::sign_tx(signer, tx).await;
    signed.encoded_2718().into()
}

/// Creates Morph payload attributes for a given timestamp.
///
/// This is the attributes generator function passed to reth's E2E test framework.
/// It creates minimal attributes with no L1 messages, suitable for testing.
pub fn morph_payload_attributes(timestamp: u64) -> MorphPayloadBuilderAttributes {
    let attributes = PayloadAttributes {
        timestamp,
        prev_randao: B256::ZERO,
        suggested_fee_recipient: Address::ZERO,
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
    };

    MorphPayloadBuilderAttributes::from(EthPayloadBuilderAttributes::new(B256::ZERO, attributes))
}
