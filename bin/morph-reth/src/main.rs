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
    if let Err(err) = Cli::<MorphChainSpecParser, MorphArgs, DefaultRpcModuleValidator>::parse()
        .run_with_components::<MorphNode>(
        components,
        async move |builder, morph_args| {
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
                    let best_before = provider.best_block_number();
                    let db_head = provider.last_block_number();
                    tracing::info!(
                        target: "morph::node",
                        best_before = ?best_before,
                        db_head = ?db_head,
                        "on_component_initialized: startup head snapshot"
                    );

                    match db_head {
                        Ok(db_head) if db_head > 0 => match provider.sealed_header(db_head) {
                            Ok(Some(sealed_header)) => {
                                provider.set_canonical_head(sealed_header);
                                let best_after = provider.best_block_number();
                                tracing::info!(
                                    target: "morph::node",
                                    db_head,
                                    best_after = ?best_after,
                                    "on_component_initialized: set canonical head from DB"
                                );
                            }
                            Ok(None) => {
                                tracing::warn!(
                                    target: "morph::node",
                                    db_head,
                                    "on_component_initialized: db head header missing"
                                );
                            }
                            Err(err) => {
                                tracing::warn!(
                                    target: "morph::node",
                                    db_head,
                                    error = %err,
                                    "on_component_initialized: failed to read sealed header"
                                );
                            }
                        },
                        Ok(_) => {
                            tracing::info!(
                                target: "morph::node",
                                "on_component_initialized: db head is zero, skip head correction"
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "morph::node",
                                error = %err,
                                "on_component_initialized: failed to read last_block_number"
                            );
                        }
                    }
                    Ok(())
                })
                .launch_with_debug_capabilities()
                .await?;

            info!(target: "morph::cli", "Node started successfully");

            // Wait for node exit
            handle.node_exit_future.await
        },
    ) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
