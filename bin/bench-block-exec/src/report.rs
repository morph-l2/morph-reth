use clap::Args;

#[derive(Args)]
pub struct SummarizeArgs {
    /// Directory containing per-round result files.
    #[arg(long)]
    pub results_dir: String,
    /// Output file path for the summary.
    #[arg(long)]
    pub output: Option<String>,
}

pub fn summarize(_args: SummarizeArgs) -> eyre::Result<()> {
    todo!("summarize not yet implemented")
}
