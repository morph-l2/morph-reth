use clap::Args;

#[derive(Args)]
pub struct VerifyStateArgs {
    /// RPC URL for node A.
    #[arg(long)]
    pub rpc_a: String,
    /// RPC URL for node B.
    #[arg(long)]
    pub rpc_b: String,
    /// Number of accounts to sample for balance checks.
    #[arg(long, default_value = "100")]
    pub check_balances: u64,
    /// Output file path for verification results.
    #[arg(long)]
    pub output: Option<String>,
}

pub async fn run(_args: VerifyStateArgs) -> eyre::Result<()> {
    todo!("verify-state not yet implemented")
}
