use clap::Parser;
use morph_statetest::runner::{RunnerOptions, run_path};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(about = "Execute Ethereum statetest JSON fixtures with Morph EVM semantics")]
struct Cli {
    /// JSON file or directory containing statetest fixtures.
    path: PathBuf,
    /// Emit EIP-3155 execution trace events to stderr.
    #[arg(long)]
    trace: bool,
    /// Validate post-state roots from the fixture and exit non-zero on mismatch.
    #[arg(long)]
    validate: bool,
}

fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    let paths = collect_json_files(&cli.path)?;
    if paths.is_empty() {
        eyre::bail!("no JSON statetest files found at {}", cli.path.display());
    }

    let options = RunnerOptions {
        trace: cli.trace,
        validate: cli.validate,
    };
    for path in paths {
        for outcome in run_path(&path, options)? {
            println!("{}", serde_json::to_string(&outcome)?);
        }
    }

    Ok(())
}

fn collect_json_files(path: &Path) -> eyre::Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("json") => vec![path.to_path_buf()],
                _ => Vec::new(),
            },
        );
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            paths.extend(collect_json_files(&path)?);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}
