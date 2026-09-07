//! Morph-Reth CLI
//!
//! This is the main entry point for the Morph L2 execution layer client.
//! It extends reth with Morph-specific functionality.

mod proofs;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

// Required for `override_allocator_on_supported_platforms` — ensures the linker
// pulls in tikv_jemalloc_sys symbols so jemalloc takes over malloc/free.
#[cfg(all(feature = "jemalloc", unix))]
use reth_cli_util::allocator::tikv_jemalloc_sys as _;

#[cfg(all(feature = "jemalloc-prof", unix))]
#[unsafe(export_name = "malloc_conf")]
static MALLOC_CONF: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

use alloy_primitives::U256;
use clap::Parser;
use morph_chainspec::{MORPH_DEFAULT_PRIORITY_FEE, MorphChainSpec, MorphChainSpecParser};
use morph_consensus::MorphConsensus;
use morph_evm::{MorphEvmConfig, evm::MorphEvmFactory};
use morph_node::{MorphAddOns, MorphArgs, MorphNode};
use morph_proofs::{MdbxProofsStorage, MorphProofsStorage, MorphProofsStore, ProofDbIdentity};
use morph_proofs_exex::MorphProofsExEx;
use proofs::MorphSubcommand;
use reth_chainspec::EthChainSpec;
use reth_cli_util::sigsegv_handler;
use reth_ethereum_cli::{Cli, Commands};
use reth_node_builder::Node;
use reth_rpc_server_types::DefaultRpcModuleValidator;
use std::sync::Arc;
use tracing::info;

fn morph_default_suggested_fee() -> U256 {
    U256::from_limbs([MORPH_DEFAULT_PRIORITY_FEE, 0, 0, 0])
}

fn apply_morph_cli_defaults(
    cli: &mut Cli<MorphChainSpecParser, MorphArgs, DefaultRpcModuleValidator, MorphSubcommand>,
) {
    if let Commands::Node(command) = &mut cli.command {
        command
            .rpc
            .gas_price_oracle
            .default_suggested_fee
            .get_or_insert_with(morph_default_suggested_fee);
    }
}

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

    let mut cli =
        Cli::<MorphChainSpecParser, MorphArgs, DefaultRpcModuleValidator, MorphSubcommand>::parse();
    apply_morph_cli_defaults(&mut cli);

    // Run the node
    if let Err(err) =
        cli.run_with_components::<MorphNode>(components, async move |builder, morph_args| {
            info!(target: "morph::cli", "Starting Morph-Reth node");

            let chain_spec = builder.config().chain.clone();
            let datadir = builder.config().datadir();
            let chain_id = chain_spec.chain().id();
            let genesis_hash = chain_spec.genesis_hash();

            let proof_history = if morph_args.proofs_history {
                let path = morph_args
                    .proofs_history_storage_path
                    .clone()
                    .unwrap_or_else(|| datadir.data_dir().join("historical-proofs"));
                if !path.is_dir() {
                    return Err(eyre::eyre!(
                        "proof history directory {} does not exist; run `morph-reth proofs init` first, or check --proofs-history.storage-path",
                        path.display()
                    ));
                }
                info!(
                    target: "morph::proofs",
                    path = %path.display(),
                    chain_id,
                    "opening Morph historical proof database"
                );
                let storage: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::new(
                    MdbxProofsStorage::open(
                        &path,
                        ProofDbIdentity::new(chain_id, genesis_hash),
                    )
                    .map_err(|error| {
                        eyre::eyre!(
                            "failed to open historical proof database at {}: {error}",
                            path.display()
                        )
                    })?,
                );
                if storage.get_earliest_block_number()?.is_none()
                    || storage.get_latest_block_number()?.is_none()
                {
                    return Err(eyre::eyre!(
                        "proof history is enabled but {} is not initialized; run `morph-reth proofs init` first",
                        path.display()
                    ));
                }
                Some((
                    storage,
                    morph_args.proofs_history_window,
                    morph_args.proofs_history_verification_interval,
                    morph_args.proofs_history_max_multi_proof_targets,
                ))
            } else {
                None
            };
            let node = MorphNode::new(morph_args);

            let mut add_ons = MorphAddOns::new();
            if let Some((storage, _, _, max_multi_proof_targets)) = &proof_history {
                add_ons = add_ons.with_proof_history(storage.clone(), *max_multi_proof_targets);
            }

            let mut node_builder = builder
                .with_types::<MorphNode>()
                .with_components(node.components_builder())
                .with_add_ons(add_ons);

            if let Some((storage, window, verification_interval, _)) = proof_history {
                node_builder = node_builder.install_exex(
                    "morph-proof-history",
                    async move |ctx| {
                        let exex = MorphProofsExEx::builder(ctx, storage)
                            .with_proofs_history_window(window)
                            .with_verification_interval(verification_interval)
                            .build();
                        Ok(async move { exex.run().await })
                    },
                );
            }

            let handle = node_builder.launch_with_debug_capabilities().await?;

            info!(target: "morph::cli", "Node started successfully");

            // Wait for node exit
            handle.node_exit_future.await
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type MorphCli =
        Cli<MorphChainSpecParser, MorphArgs, DefaultRpcModuleValidator, MorphSubcommand>;

    #[test]
    fn parses_top_level_proofs_command() {
        let cli = MorphCli::try_parse_from(["morph-reth", "proofs", "init", "--chain", "hoodi"])
            .expect("top-level proofs command must parse");
        assert!(matches!(
            cli.command,
            Commands::Ext(MorphSubcommand::Proofs(_))
        ));
    }

    #[test]
    fn disabled_mode_keeps_reth_historical_overlay_off() {
        let cli = MorphCli::try_parse_from(["morph-reth", "node", "--chain", "hoodi"])
            .expect("node command must parse");
        let Commands::Node(command) = cli.command else {
            panic!("expected node command")
        };
        assert!(!command.ext.proofs_history);
        assert_eq!(command.rpc.rpc_eth_proof_window, 0);
    }
}
