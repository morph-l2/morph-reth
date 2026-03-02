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
                    // Startup correction: synchronize canonical_in_memory_state with the
                    // actual DB head before the engine tree is spawned.
                    //
                    // The engine API writes StageId::Finish after each imported block so
                    // that BlockchainProvider::new() reads the correct head on normal
                    // restarts. This hook handles two remaining edge cases:
                    //
                    //   1. First deployment / migration: if the node was previously running
                    //      without the StageId::Finish write, StageId::Finish is still 0
                    //      even though many blocks are in the DB.
                    //
                    //   2. Crash recovery: a process crash between the engine FCU commit
                    //      and the StageId::Finish write leaves the checkpoint one block
                    //      behind last_block_number(). This hook corrects the discrepancy.
                    //
                    // Running before EngineApiTreeHandler::spawn_new() ensures that both
                    // canonical_in_memory_state.canonical_head and
                    // tree_state.current_canonical_head are set to the true DB head, so
                    // the first FCU for the next block does not see SYNCING.
                    .on_component_initialized(|node| {
                        use reth_provider::{BlockNumReader, CanonChainTracker, HeaderProvider};
                        let provider = &node.provider;
                        if let Ok(db_head) = provider.last_block_number() {
                            if db_head > 0 {
                                if let Ok(Some(sealed_header)) = provider.sealed_header(db_head) {
                                    provider.set_canonical_head(sealed_header);
                                    tracing::info!(
                                        target: "morph::node",
                                        db_head,
                                        "on_component_initialized: set canonical head from DB"
                                    );
                                }
                            }
                        }
                        Ok(())
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
