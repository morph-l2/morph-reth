//! Morph-Reth CLI
//!
//! This is the main entry point for the Morph L2 execution layer client.
//! It extends reth with Morph-specific functionality.

use clap::Parser;
use morph_chainspec::{MorphChainSpec, MorphChainSpecParser};
use morph_consensus::MorphConsensus;
use morph_evm::{MorphEvmConfig, evm::MorphEvmFactory};
use morph_node::{MorphArgs, MorphNode};
use reth_cli_util::sigsegv_handler;
use reth_ethereum_cli::Cli;
use reth_rpc_server_types::DefaultRpcModuleValidator;
use std::sync::Arc;
use tracing::info;

fn main() {
    // Install signal handler for segmentation faults
    sigsegv_handler::install();

    // Enable backtraces by default
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

                let handle = builder
                    .node(MorphNode::new(morph_args))
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
