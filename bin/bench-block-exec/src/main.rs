mod engine;
mod genesis;
pub mod mode_e2e;
pub mod mode_exec;
pub mod mode_sustained;
mod report;
pub mod tx_factory;
mod verify;
mod workload;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bench-block-exec", about = "Geth vs Reth block execution benchmark")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a custom benchmark genesis JSON.
    WriteGenesis(genesis::WriteGenesisArgs),
    /// Run a benchmark workload against a running node.
    RunWorkload(workload::RunWorkloadArgs),
    /// Verify state consistency between two nodes.
    VerifyState(verify::VerifyStateArgs),
    /// Aggregate benchmark results into a summary.
    Summarize(report::SummarizeArgs),
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::WriteGenesis(args) => genesis::run(args),
        Command::RunWorkload(args) => workload::run(args).await,
        Command::VerifyState(args) => verify::run(args).await,
        Command::Summarize(args) => report::summarize(args),
    }
}
