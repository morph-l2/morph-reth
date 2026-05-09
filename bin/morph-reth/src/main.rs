//! Morph-Reth CLI
//!
//! This is the main entry point for the Morph L2 execution layer client.
//! It extends reth with Morph-specific functionality.

use clap::Parser;
use morph_chainspec::{MorphChainSpec, MorphChainSpecParser};
use morph_consensus::MorphConsensus;
use morph_evm::{MorphEvmConfig, evm::MorphEvmFactory};
use morph_node::{
    MorphAddOns, MorphArgs, MorphNode,
    exex::{ReferenceIndexControl, reference_index_exex},
};
use morph_reference_index::ReferenceIndexDb;
use reth_chainspec::EthChainSpec;
use reth_cli_util::sigsegv_handler;
use reth_ethereum_cli::Cli;
use reth_node_builder::Node;
use reth_rpc_server_types::DefaultRpcModuleValidator;
use std::sync::Arc;
use tracing::info;

fn main() {
    // Override reth's default version info with morph-reth's own version,
    // commit SHA, and build timestamp. Must be called before CLI parsing.
    morph_node::version::init_version_metadata();

    // Install signal handler for segmentation faults
    sigsegv_handler::install();

    // Enable backtraces by default.
    // SAFETY: Called at process startup before any other threads are spawned,
    // so there are no concurrent readers of the environment.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    // Component builder: creates EVM config and consensus
    let components = |spec: Arc<MorphChainSpec>| {
        (
            MorphEvmConfig::new(spec.clone(), MorphEvmFactory::default()),
            MorphConsensus::new(spec),
        )
    };

    // Parse CLI arguments and run the node
    if let Err(err) =
        Cli::<MorphChainSpecParser, MorphArgs, DefaultRpcModuleValidator>::parse()
            .run_with_components::<MorphNode>(components, async move |builder, morph_args| {
                info!(target: "morph::cli", "Starting Morph-Reth node");

                // Open the reference index DB before launching the node so we
                // can wire it into both the ExEx and the add-ons.
                let chain_spec = builder.config().chain.clone();
                let datadir = builder.config().datadir();
                let reference_index_path = datadir.data_dir().join("morph").join("reference_index");
                let chain_id = chain_spec.chain().id();
                let genesis_hash = chain_spec.genesis_hash(); // from EthChainSpec trait

                info!(
                    target: "morph::reference_index",
                    path = %reference_index_path.display(),
                    chain_id,
                    "opening Morph reference index database"
                );
                let db = ReferenceIndexDb::open(&reference_index_path, chain_id, genesis_hash)?;
                let (control, startup_rx) = ReferenceIndexControl::new(db);

                let exex_control = control.clone();
                let node = MorphNode::new(morph_args);

                let handle = builder
                    .with_types::<MorphNode>()
                    .with_components(node.components_builder())
                    .with_add_ons(MorphAddOns::new().with_reference_index(control))
                    .install_exex("morph-reference-index", async move |ctx| {
                        Ok(reference_index_exex(ctx, exex_control, startup_rx))
                    })
                    .launch_with_debug_capabilities()
                    .await?;

                info!(target: "morph::cli", "Node started successfully");

                // Wait for node exit
                handle.node_exit_future.await
            })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
