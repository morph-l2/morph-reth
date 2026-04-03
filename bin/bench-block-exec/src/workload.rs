use clap::Args;

#[derive(Args)]
pub struct RunWorkloadArgs {
    /// Engine API RPC URL (authenticated).
    #[arg(long, default_value = "http://127.0.0.1:8551")]
    pub engine_rpc: String,
    /// Path to JWT secret file.
    #[arg(long)]
    pub jwt_secret: String,
    /// HTTP RPC URL (unauthenticated, for readiness checks).
    #[arg(long, default_value = "http://127.0.0.1:8545")]
    pub http_rpc: String,
    /// Workload layer: "eth-transfer" or "erc20-transfer".
    #[arg(long)]
    pub layer: String,
    /// Number of transactions per block.
    #[arg(long)]
    pub txs_per_block: u64,
    /// Number of blocks to produce.
    #[arg(long)]
    pub blocks: u64,
    /// Output file path for per-block results (JSON lines).
    #[arg(long)]
    pub output: String,
    /// Engine name for tagging results (e.g., "geth" or "reth").
    #[arg(long, default_value = "unknown")]
    pub engine_name: String,
    /// Path to the BenchToken contract artifact JSON (required for erc20-transfer layer).
    #[arg(long)]
    pub contract_artifact: Option<String>,
    /// Hex-encoded sender private key (0x-prefixed).
    #[arg(long)]
    pub sender_key: String,
    /// Chain ID.
    #[arg(long, default_value = "99999")]
    pub chain_id: u64,
}

pub async fn run(_args: RunWorkloadArgs) -> eyre::Result<()> {
    todo!("run-workload not yet implemented")
}
