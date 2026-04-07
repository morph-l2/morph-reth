mod engine;
mod genesis;
pub mod mode_e2e;
pub mod mode_exec;
pub mod mode_sustained;
mod report;
pub mod sweep;
pub mod tx_factory;
mod verify;
mod workload;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bench-block-exec", about = "Morph block execution benchmark")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a benchmark genesis file.
    WriteGenesis(genesis::WriteGenesisArgs),
    /// Run the legacy workload benchmark (backward compat).
    RunWorkload(workload::RunWorkloadArgs),
    /// Run a benchmark in the specified mode.
    Run {
        #[command(subcommand)]
        mode: RunMode,
    },
    /// Automatically find the TPS inflection point.
    Sweep(sweep::SweepArgs),
    /// Verify state consistency between two nodes.
    VerifyState(verify::VerifyStateArgs),
    /// Summarize benchmark results into TSV.
    Summarize(report::SummarizeArgs),
}

#[derive(Subcommand)]
enum RunMode {
    /// Mode A: Pure execution (bypass txpool).
    Exec(mode_exec::ExecArgs),
    /// Mode B: End-to-end (txpool -> assembly -> import).
    E2e(mode_e2e::E2eArgs),
    /// Mode C: Sustained block production with optional warmup.
    Sustained(mode_sustained::SustainedArgs),
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::WriteGenesis(args) => genesis::run(args),
        Command::RunWorkload(args) => workload::run(args).await,
        Command::Run { mode } => match mode {
            RunMode::Exec(args) => mode_exec::run(args).await,
            RunMode::E2e(args) => mode_e2e::run(args).await,
            RunMode::Sustained(args) => mode_sustained::run(args).await,
        },
        Command::Sweep(args) => sweep::run(args).await,
        Command::VerifyState(args) => verify::run(args).await,
        Command::Summarize(args) => report::summarize(args),
    }
}
