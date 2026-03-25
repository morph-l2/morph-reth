use alloy_consensus::BlockHeader;
use alloy_genesis::Genesis;
use alloy_primitives::{U256, address, hex, map::HashSet};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_rpc_types_trace::{geth::GethDebugTracingCallOptions, parity::TraceType};
use morph_node::{MorphAddOns, MorphNode};
use morph_payload_builder::MorphBuilderConfig;
use morph_rpc::MorphTransactionRequest;
use reth_node_builder::{NodeBuilder, NodeHandle};
use reth_node_core::{args::DevArgs, node_config::NodeConfig};
use reth_provider::{CanonStateSubscriptions, providers::BlockchainProvider};
use reth_rpc_eth_api::helpers::EthTransactions;
use reth_tasks::TaskManager;
use std::sync::Arc;
use tokio_stream::StreamExt;

#[tokio::test]
async fn trace_replay_endpoints_succeed_without_beacon_root_on_cancun_morph() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let tasks = TaskManager::current();
    let exec = tasks.executor();
    let node_config = NodeConfig::test()
        .map_chain(custom_chain())
        .with_dev(DevArgs {
            dev: true,
            ..Default::default()
        });

    let NodeHandle {
        node,
        node_exit_future: _,
    } = NodeBuilder::new(node_config)
        .testing_node(exec)
        .with_types_and_provider::<MorphNode, BlockchainProvider<_>>()
        .with_components(MorphNode::components(MorphBuilderConfig::default()))
        .with_add_ons(MorphAddOns::default())
        .launch_with_debug_capabilities()
        .await?;

    let mut notifications = node.provider.canonical_state_stream();
    let raw_tx = hex!(
        "02f876820a28808477359400847735940082520894ab0840c0e43688012c1adb0f5e3fc665188f83d28a029d394a5d630544000080c080a0a044076b7e67b5deecc63f61a8d7913fab86ca365b344b5759d1fe3563b4c39ea019eab979dd000da04dfc72bb0377c092d30fd9e1cab5ae487de49586cc8b0090"
    );

    let tx_hash =
        EthTransactions::send_raw_transaction(node.rpc_registry.eth_api(), raw_tx.into()).await?;

    let head = notifications
        .next()
        .await
        .expect("dev node should mine a block");
    head.tip()
        .body()
        .transactions()
        .next()
        .expect("mined block should contain the submitted tx");
    assert!(
        head.tip().header().parent_beacon_block_root().is_none(),
        "Morph L2 blocks should not carry Ethereum parent_beacon_block_root even when cancunTime is active"
    );

    let block_hash = head.tip().hash();
    let block_number = head.tip().number();

    node.rpc_registry
        .debug_api()
        .debug_trace_transaction(tx_hash, Default::default())
        .await?;

    let traces_by_hash = node
        .rpc_registry
        .debug_api()
        .debug_trace_block(block_hash.into(), Default::default())
        .await?;
    assert_eq!(traces_by_hash.len(), 1);

    let traces_by_number = node
        .rpc_registry
        .debug_api()
        .debug_trace_block(block_number.into(), Default::default())
        .await?;
    assert_eq!(traces_by_number.len(), 1);

    assert!(
        node.rpc_registry
            .trace_api()
            .trace_transaction(tx_hash)
            .await?
            .is_some_and(|traces| !traces.is_empty())
    );
    assert!(
        node.rpc_registry
            .trace_api()
            .trace_transaction_opcode_gas(tx_hash)
            .await?
            .is_some()
    );
    let trace_types = HashSet::from_iter([TraceType::Trace]);
    node.rpc_registry
        .trace_api()
        .replay_transaction(tx_hash, trace_types.clone())
        .await?;
    assert!(
        node.rpc_registry
            .trace_api()
            .trace_block(block_hash.into())
            .await?
            .is_some_and(|traces| !traces.is_empty())
    );
    assert!(
        node.rpc_registry
            .trace_api()
            .replay_block_transactions(block_hash.into(), trace_types)
            .await?
            .is_some_and(|traces| !traces.is_empty())
    );
    assert!(
        node.rpc_registry
            .trace_api()
            .trace_block_opcode_gas(block_hash.into())
            .await?
            .is_some()
    );

    let call = MorphTransactionRequest::from(TransactionRequest {
        from: Some(address!("6Be02d1d3665660d22FF9624b7BE0551ee1Ac91b")),
        to: Some(address!("ab0840c0e43688012c1adb0f5e3fc665188f83d2").into()),
        gas: Some(21_000),
        gas_price: Some(1_000_000_000),
        value: Some(U256::ZERO),
        ..Default::default()
    });
    let mut opts = GethDebugTracingCallOptions::default();
    opts.tx_index = Some(0);

    node.rpc_registry
        .debug_api()
        .debug_trace_call(call, Some(block_hash.into()), opts)
        .await?;

    Ok(())
}

fn custom_chain() -> Arc<morph_chainspec::MorphChainSpec> {
    let custom_genesis = r#"
{
    "nonce": "0x42",
    "timestamp": "0x0",
    "extraData": "0x5343",
    "gasLimit": "0x13880",
    "difficulty": "0x0",
    "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "coinbase": "0x0000000000000000000000000000000000000000",
    "alloc": {
        "0x6Be02d1d3665660d22FF9624b7BE0551ee1Ac91b": {
            "balance": "0x4a47e3c12448f4ad000000"
        }
    },
    "number": "0x0",
    "gasUsed": "0x0",
    "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "config": {
        "chainId": 2600,
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
        "mergeNetsplitBlock": 0,
        "terminalTotalDifficulty": 0,
        "terminalTotalDifficultyPassed": true,
        "shanghaiTime": 0,
        "cancunTime": 0,
        "bernoulliBlock": 0,
        "curieBlock": 0,
        "morph203Time": 0,
        "viridianTime": 0,
        "emeraldTime": 0,
        "jadeTime": 0,
        "morph": {
            "feeVaultAddress": "0x530000000000000000000000000000000000000a"
        }
    }
}
"#;
    let genesis: Genesis = serde_json::from_str(custom_genesis).unwrap();
    Arc::new(genesis.into())
}
