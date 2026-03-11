use alloy_primitives::B256;
use clap::{Parser, ValueEnum};
use eyre::{Context, ContextCompat, Result, ensure};
use morph_chainspec::{MORPH_HOODI, MORPH_MAINNET, MorphChainSpec};
use morph_node::{MorphNode, validator::fetch_geth_disk_root};
use reth_provider::{
    ProviderFactory,
    providers::{ProviderNodeTypes, ReadOnlyConfig},
};
use reth_storage_api::{BlockNumReader, StateRootProvider};
use reth_trie::HashedPostState;
use state_root_check::{BlockRootComparison, find_first_mismatch};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum ChainArg {
    Morph,
    #[default]
    #[value(name = "morph-hoodi")]
    MorphHoodi,
}

impl ChainArg {
    fn chain_spec(self) -> Arc<MorphChainSpec> {
        match self {
            Self::Morph => MORPH_MAINNET.clone(),
            Self::MorphHoodi => MORPH_HOODI.clone(),
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "state-root-check")]
#[command(about = "Compare local reth MPT roots with geth morph_diskRoot")]
struct Args {
    #[arg(long)]
    datadir: PathBuf,
    #[arg(long, default_value = "morph-hoodi")]
    chain: ChainArg,
    #[arg(long)]
    geth_rpc_url: Option<String>,
    #[arg(long, conflicts_with_all = ["fresh_root_at", "bisect"])]
    block: Option<u64>,
    #[arg(long, conflicts_with_all = ["block", "bisect"])]
    fresh_root_at: Option<u64>,
    #[arg(long, requires_all = ["from_block", "to_block"], conflicts_with_all = ["block", "fresh_root_at"])]
    bisect: bool,
    #[arg(long, requires = "bisect")]
    from_block: Option<u64>,
    #[arg(long, requires = "bisect")]
    to_block: Option<u64>,
}

enum Mode {
    LocalRoot {
        block: u64,
    },
    Compare {
        block: u64,
        geth_rpc_url: String,
    },
    Bisect {
        from: u64,
        to: u64,
        geth_rpc_url: String,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.datadir.exists(),
        "datadir does not exist: {}",
        args.datadir.display()
    );

    let factory = MorphNode::provider_factory_builder()
        .open_read_only(
            args.chain.chain_spec(),
            ReadOnlyConfig::from_datadir(&args.datadir),
        )
        .with_context(|| format!("failed to open datadir {}", args.datadir.display()))?;

    let latest_block = factory.best_block_number()?;
    let mode = resolve_mode(&args, latest_block)?;

    match mode {
        Mode::LocalRoot { block } => {
            let root = compute_local_state_root(&factory, block)?;
            println!("fresh_root_at #{block}: {root:#x}");
        }
        Mode::Compare {
            block,
            geth_rpc_url,
        } => {
            let comparison = compare_block_roots(&factory, &geth_rpc_url, block)?;
            print_comparison(&comparison);
        }
        Mode::Bisect {
            from,
            to,
            geth_rpc_url,
        } => {
            let mut cache = HashMap::new();
            let first = find_first_mismatch(from, to, |block| {
                if let Some(is_match) = cache.get(&block) {
                    return Ok(*is_match);
                }

                let comparison = compare_block_roots(&factory, &geth_rpc_url, block)?;
                let is_match = comparison.is_match();
                print_probe(&comparison);
                cache.insert(block, is_match);
                Ok(is_match)
            })?;

            match first {
                Some(block) => println!("first_mismatch: {block}"),
                None => println!("all blocks matched in range [{from}, {to}]"),
            }
        }
    }

    Ok(())
}

fn resolve_mode(args: &Args, latest_block: u64) -> Result<Mode> {
    if let Some(block) = args.fresh_root_at {
        return Ok(Mode::LocalRoot { block });
    }

    if args.bisect {
        let from = args
            .from_block
            .context("--from-block is required when --bisect is set")?;
        let to = args
            .to_block
            .context("--to-block is required when --bisect is set")?;
        ensure!(
            from <= to,
            "--from-block must be less than or equal to --to-block"
        );
        return Ok(Mode::Bisect {
            from,
            to,
            geth_rpc_url: args
                .geth_rpc_url
                .clone()
                .context("--geth-rpc-url is required for --bisect")?,
        });
    }

    let block = args.block.unwrap_or(latest_block);
    if let Some(geth_rpc_url) = args.geth_rpc_url.clone() {
        Ok(Mode::Compare {
            block,
            geth_rpc_url,
        })
    } else {
        Ok(Mode::LocalRoot { block })
    }
}

fn compute_local_state_root<N>(factory: &ProviderFactory<N>, block: u64) -> Result<B256>
where
    N: ProviderNodeTypes,
{
    let provider = factory
        .history_by_block_number(block)
        .with_context(|| format!("failed to open historical state at block {block}"))?;
    let root = provider
        .state_root(HashedPostState::default())
        .with_context(|| format!("failed to compute local state root at block {block}"))?;
    Ok(root)
}

fn compare_block_roots<N>(
    factory: &ProviderFactory<N>,
    geth_rpc_url: &str,
    block: u64,
) -> Result<BlockRootComparison>
where
    N: ProviderNodeTypes,
{
    let reth_root = compute_local_state_root(factory, block)?;
    let geth_disk_root =
        fetch_geth_disk_root(geth_rpc_url, block).map_err(|err| eyre::eyre!(err))?;
    Ok(BlockRootComparison {
        block_number: block,
        reth_root,
        geth_disk_root,
    })
}

fn print_comparison(comparison: &BlockRootComparison) {
    let status = if comparison.is_match() {
        "MATCH"
    } else {
        "MISMATCH"
    };
    println!("block #{}", comparison.block_number);
    println!("reth_root: {:#x}", comparison.reth_root);
    println!("geth_disk_root: {:#x}", comparison.geth_disk_root);
    println!("status: {status}");
}

fn print_probe(comparison: &BlockRootComparison) {
    let status = if comparison.is_match() {
        "MATCH"
    } else {
        "MISMATCH"
    };
    println!(
        "probe #{} => {} reth={:#x} geth={:#x}",
        comparison.block_number, status, comparison.reth_root, comparison.geth_disk_root
    );
}
