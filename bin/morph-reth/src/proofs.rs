//! Management commands for the independent historical-proof database.

use std::{path::PathBuf, sync::Arc};

use alloy_consensus::BlockHeader;
use clap::{Parser, Subcommand};
use morph_chainspec::{MorphChainSpec, MorphChainSpecParser};
use morph_node::MorphNode;
use morph_proofs::{
    DEFAULT_PROOFS_HISTORY_WINDOW, InitializationJob, MdbxProofsStorage, MorphProofStoragePruner,
    MorphProofsStorage, MorphProofsStore, ProofDbIdentity, RethTrieStorageLayout,
    persisted_canonical_head, proof::state_root_at,
};
use reth_chainspec::EthChainSpec;
use reth_cli_commands::common::{AccessRights, Environment, EnvironmentArgs};
use reth_cli_runner::CliRunner;
use reth_ethereum_cli::ExtendedCommand;
use reth_provider::{
    BlockHashReader, BlockNumReader, BlockReader, DBProvider, DatabaseProviderFactory,
    HeaderProvider, StorageSettingsCache, TransactionVariant,
};
use tracing::info;

/// Morph-specific top-level CLI commands.
#[derive(Debug, Subcommand)]
pub(crate) enum MorphSubcommand {
    /// Manage the historical proof database.
    #[command(name = "proofs")]
    Proofs(ProofsCommand),
}

impl ExtendedCommand for MorphSubcommand {
    fn execute(self, runner: CliRunner) -> eyre::Result<()> {
        match self {
            Self::Proofs(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute(runtime))
            }
        }
    }
}

/// Manage Morph historical proofs.
#[derive(Debug, Parser)]
pub(crate) struct ProofsCommand {
    #[command(subcommand)]
    command: ProofsSubcommand,
}

impl ProofsCommand {
    async fn execute(self, runtime: reth_tasks::Runtime) -> eyre::Result<()> {
        match self.command {
            ProofsSubcommand::Init(command) => command.execute(runtime),
            ProofsSubcommand::Prune(command) => command.execute(runtime),
            ProofsSubcommand::Unwind(command) => command.execute(runtime),
        }
    }
}

#[derive(Debug, Subcommand)]
enum ProofsSubcommand {
    /// Initialize proof history from the current canonical tip.
    Init(InitCommand),
    /// Remove proof history outside the configured window.
    Prune(PruneCommand),
    /// Remove proof history at and after a canonical block.
    Unwind(UnwindCommand),
}

#[derive(Debug, Parser)]
struct InitCommand {
    #[command(flatten)]
    env: EnvironmentArgs<MorphChainSpecParser>,

    /// Proof-history MDBX directory (defaults to `<chain-datadir>/historical-proofs`).
    #[arg(long = "proofs-history.storage-path", value_name = "PATH")]
    storage_path: Option<PathBuf>,
}

impl InitCommand {
    fn execute(self, runtime: reth_tasks::Runtime) -> eyre::Result<()> {
        let Environment {
            provider_factory,
            data_dir,
            ..
        } = self.env.init::<MorphNode>(AccessRights::RO, runtime)?;
        let path = self
            .storage_path
            .unwrap_or_else(|| data_dir.data_dir().join("historical-proofs"));
        let storage = open_storage(&path, &self.env.chain)?;

        if let Some((number, hash)) = storage.get_earliest_block_number()? {
            info!(target: "morph::proofs", block_number = number, ?hash, "Proof history is already initialized");
            return Ok(());
        }

        let trie_layout =
            RethTrieStorageLayout::from_is_v2(provider_factory.cached_storage_settings().is_v2());
        let provider = provider_factory
            .database_provider_ro()?
            .disable_long_read_transaction_safety();
        let chain_info = provider.chain_info()?;
        info!(
            target: "morph::proofs",
            path = %path.display(),
            block_number = chain_info.best_number,
            block_hash = ?chain_info.best_hash,
            ?trie_layout,
            "Initializing proof history from canonical state"
        );

        InitializationJob::new(storage, provider.into_tx(), trie_layout)
            .run(chain_info.best_number, chain_info.best_hash)?;

        info!(target: "morph::proofs", "Proof history initialized");
        Ok(())
    }
}

#[derive(Debug, Parser)]
struct PruneCommand {
    #[command(flatten)]
    env: EnvironmentArgs<MorphChainSpecParser>,

    /// Proof-history MDBX directory (defaults to `<chain-datadir>/historical-proofs`).
    #[arg(long = "proofs-history.storage-path", value_name = "PATH")]
    storage_path: Option<PathBuf>,

    /// Number of canonical blocks to retain.
    #[arg(
        long = "proofs-history.window",
        value_name = "BLOCKS",
        default_value_t = DEFAULT_PROOFS_HISTORY_WINDOW,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    window: u64,

    /// Maximum number of blocks pruned per transaction.
    #[arg(
        long = "proofs-history.prune-batch-size",
        value_name = "BLOCKS",
        default_value_t = 1_000,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    batch_size: u64,
}

impl PruneCommand {
    fn execute(self, runtime: reth_tasks::Runtime) -> eyre::Result<()> {
        let Environment {
            provider_factory,
            data_dir,
            ..
        } = self.env.init::<MorphNode>(AccessRights::RO, runtime)?;
        let path = self
            .storage_path
            .unwrap_or_else(|| data_dir.data_dir().join("historical-proofs"));
        let storage = open_storage(&path, &self.env.chain)?;

        let persisted_head = persisted_canonical_head(&provider_factory)?;
        let output =
            MorphProofStoragePruner::new(storage, provider_factory, self.window, self.batch_size)
                .try_run_up_to(Some(persisted_head))?;
        info!(target: "morph::proofs", ?output, "Proof-history pruning completed");
        Ok(())
    }
}

#[derive(Debug, Parser)]
struct UnwindCommand {
    #[command(flatten)]
    env: EnvironmentArgs<MorphChainSpecParser>,

    /// Proof-history MDBX directory (defaults to `<chain-datadir>/historical-proofs`).
    #[arg(long = "proofs-history.storage-path", value_name = "PATH")]
    storage_path: Option<PathBuf>,

    /// First block to remove, inclusive.
    #[arg(long, value_name = "BLOCK")]
    target: u64,
}

impl UnwindCommand {
    fn execute(self, runtime: reth_tasks::Runtime) -> eyre::Result<()> {
        let Environment {
            provider_factory,
            data_dir,
            ..
        } = self.env.init::<MorphNode>(AccessRights::RO, runtime)?;
        let path = self
            .storage_path
            .unwrap_or_else(|| data_dir.data_dir().join("historical-proofs"));
        let storage = open_storage(&path, &self.env.chain)?;

        let (Some((earliest, earliest_hash)), Some((latest, _))) = (
            storage.get_earliest_block_number()?,
            storage.get_latest_block_number()?,
        ) else {
            return Err(eyre::eyre!(
                "proof history at {} is not initialized",
                path.display()
            ));
        };

        if self.target <= earliest || self.target > latest {
            return Err(eyre::eyre!(
                "unwind target {} must be in ({earliest}, {latest}]",
                self.target
            ));
        }

        let block = provider_factory
            .recovered_block(self.target.into(), TransactionVariant::NoHash)?
            .ok_or_else(|| eyre::eyre!("canonical block {} not found", self.target))?;
        let parent_number = self.target - 1;
        let canonical_parent_hash = block.parent_hash();

        // Verify that proof history at `target - 1` actually matches the canonical parent before
        // stamping `canonical_parent_hash` onto `ProofWindow::LatestBlock`.
        if let Some(stored_parent_hash) = storage.get_block_hash(parent_number)? {
            if stored_parent_hash != canonical_parent_hash {
                return Err(eyre::eyre!(
                    "proof history at parent block {parent_number} ({stored_parent_hash}) does not match canonical parent ({canonical_parent_hash}); unwind to an earlier common ancestor or re-initialize"
                ));
            }
        } else {
            // Legacy proof DB without a `BlockHashes` row at `parent_number`: verify both the
            // earliest anchor and the reconstructed state root at `parent_number`.
            let canonical_earliest_hash = provider_factory.block_hash(earliest)?;
            if canonical_earliest_hash != Some(earliest_hash) {
                return Err(eyre::eyre!(
                    "proof history earliest block {earliest} ({earliest_hash}) does not match canonical hash ({canonical_earliest_hash:?}); please re-initialize"
                ));
            }
            let parent_header = provider_factory
                .header_by_number(parent_number)?
                .ok_or_else(|| eyre::eyre!("canonical header {parent_number} not found"))?;
            let stored_state_root = state_root_at(&storage, parent_number)?;
            if stored_state_root != parent_header.state_root() {
                return Err(eyre::eyre!(
                    "proof history state root at parent block {parent_number} ({stored_state_root}) does not match canonical state root ({}); unwind to an earlier common ancestor or re-initialize",
                    parent_header.state_root()
                ));
            }
        }

        info!(
            target: "morph::proofs",
            block_number = block.number(),
            block_hash = ?block.hash(),
            parent_number,
            parent_hash = ?canonical_parent_hash,
            "Unwinding proof history"
        );
        storage.unwind_history(block.block_with_parent())?;
        Ok(())
    }
}

fn open_storage(
    path: &std::path::Path,
    chain: &MorphChainSpec,
) -> eyre::Result<MorphProofsStorage<Arc<MdbxProofsStorage>>> {
    let identity = ProofDbIdentity::new(chain.chain().id(), chain.genesis_hash());
    let backend = MdbxProofsStorage::open(path, identity).map_err(|error| {
        eyre::eyre!(
            "failed to open proof history at {}: {error}",
            path.display()
        )
    })?;
    Ok(Arc::new(backend))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::ProofsCommand;

    #[test]
    fn parses_all_proof_commands() {
        ProofsCommand::try_parse_from(["proofs", "init", "--chain", "hoodi"])
            .expect("init command must parse");
        ProofsCommand::try_parse_from([
            "proofs",
            "prune",
            "--chain",
            "hoodi",
            "--proofs-history.window",
            "64",
        ])
        .expect("prune command must parse");
        ProofsCommand::try_parse_from(["proofs", "unwind", "--chain", "hoodi", "--target", "42"])
            .expect("unwind command must parse");
    }
}
