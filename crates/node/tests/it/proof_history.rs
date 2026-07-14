//! End-to-end coexistence coverage for proof history and the reference index.

use std::{sync::Arc, time::Duration};

use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use jsonrpsee::{core::client::ClientT, http_client::HttpClient, rpc_params};
use morph_chainspec::MorphChainSpec;
use morph_node::{
    MorphAddOns, MorphNode,
    test_utils::{MorphTestNode, advance_empty_block, morph_payload_attributes},
};
use morph_proofs::{InitializationJob, MdbxProofsStorage, MorphProofsStorage, ProofDbIdentity};
use morph_proofs_exex::MorphProofsExEx;
use morph_reference_index::ReferenceTransactionResult;
use morph_rpc::{ProofsSyncStatus, ReferenceQueryArgs};
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::node::NodeTestContext;
use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig, NodeHandle};
use reth_node_core::args::{DiscoveryArgs, NetworkArgs, RpcServerArgs};
use reth_provider::{DBProvider, DatabaseProviderFactory, providers::BlockchainProvider};
use reth_rpc_server_types::RpcModuleSelection;
use reth_tasks::Runtime;

async fn launch_node_with_proof_history(
    chain_spec: Arc<MorphChainSpec>,
    storage: MorphProofsStorage<Arc<MdbxProofsStorage>>,
) -> eyre::Result<MorphTestNode> {
    let runtime = Runtime::test();
    let network = NetworkArgs {
        discovery: DiscoveryArgs {
            disable_discovery: true,
            ..DiscoveryArgs::default()
        },
        ..NetworkArgs::default()
    };
    let node_config = NodeConfig::new(chain_spec.clone())
        .with_network(network)
        .with_unused_ports()
        .with_rpc(
            RpcServerArgs::default()
                .with_unused_ports()
                .with_http()
                .with_http_api(RpcModuleSelection::All),
        );
    let node = MorphNode::default();
    let exex_storage = storage.clone();

    let NodeHandle {
        node,
        node_exit_future: _,
    } = NodeBuilder::new(node_config)
        .testing_node(runtime.clone())
        .with_types_and_provider::<MorphNode, BlockchainProvider<_>>()
        .with_components(node.components_builder())
        .with_add_ons(MorphAddOns::new().with_proof_history(storage))
        .install_exex("morph-proof-history", async move |ctx| {
            let head = ctx.head;
            let provider = ctx
                .provider()
                .database_provider_ro()?
                .disable_long_read_transaction_safety();
            InitializationJob::new(exex_storage.clone(), provider.into_tx())
                .run(head.number, head.hash)?;

            let exex = MorphProofsExEx::builder(ctx, exex_storage).build();
            Ok(async move { exex.run().await })
        })
        .launch_with_fn(|builder| {
            let launcher = EngineNodeLauncher::new(
                builder.task_executor().clone(),
                builder.config().datadir(),
                reth_node_api::TreeConfig::default().with_cross_block_cache_size(1024 * 1024),
            );
            builder.launch_with(launcher)
        })
        .await?;

    let node = NodeTestContext::new(node, morph_payload_attributes).await?;
    let genesis_hash = chain_spec.genesis_hash();
    node.update_forkchoice(genesis_hash, genesis_hash).await?;
    Ok(node)
}

async fn wait_for_proof_tip(client: &HttpClient, expected: u64) -> eyre::Result<ProofsSyncStatus> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last_status = None;
    while tokio::time::Instant::now() < deadline {
        if let Ok(status) = client
            .request::<ProofsSyncStatus, _>("debug_proofsSyncStatus", rpc_params![])
            .await
        {
            if status.latest == Some(expected) {
                return Ok(status);
            }
            last_status = Some(status);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(eyre::eyre!(
        "proof history did not reach block {expected}: {last_status:?}"
    ))
}

async fn wait_for_reference_index(client: &HttpClient) -> eyre::Result<()> {
    let args = ReferenceQueryArgs {
        reference: B256::repeat_byte(0x99),
        offset: None,
        limit: None,
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last_error = None;
    while tokio::time::Instant::now() < deadline {
        match client
            .request::<Vec<ReferenceTransactionResult>, _>(
                "morph_getTransactionHashesByReference",
                (args.clone(),),
            )
            .await
        {
            Ok(results) if results.is_empty() => return Ok(()),
            Ok(results) => {
                return Err(eyre::eyre!(
                    "unexpected reference-index results: {results:?}"
                ));
            }
            Err(error) => last_error = Some(error.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(eyre::eyre!(
        "reference index did not become ready: {}",
        last_error.as_deref().unwrap_or("no RPC response")
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_and_reference_index_advance_together() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let genesis: Genesis = serde_json::from_str(include_str!("../assets/test-genesis.json"))?;
    let chain_spec = Arc::new(MorphChainSpec::from_genesis(genesis));
    let proof_directory = tempfile::tempdir()?;
    let proof_storage = Arc::new(MdbxProofsStorage::open(
        proof_directory.path(),
        ProofDbIdentity::new(chain_spec.chain().id(), chain_spec.genesis_hash()),
    )?);
    let mut node = launch_node_with_proof_history(chain_spec, proof_storage).await?;

    advance_empty_block(&mut node).await?;
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let status = wait_for_proof_tip(&client, 1).await?;
    assert_eq!(status.earliest, Some(0));
    wait_for_reference_index(&client).await?;

    let normal_proof: EIP1186AccountProofResponse = client
        .request(
            "eth_getProof",
            rpc_params![Address::ZERO, Vec::<B256>::new(), "0x0"],
        )
        .await?;
    let auth_proof: EIP1186AccountProofResponse = node
        .auth_server_handle()
        .http_client()
        .request(
            "eth_getProof",
            rpc_params![Address::ZERO, Vec::<B256>::new(), "0x0"],
        )
        .await?;
    assert_eq!(normal_proof.address, Address::ZERO);
    assert_eq!(normal_proof, auth_proof);

    Ok(())
}
