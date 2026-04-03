use clap::Args;

#[derive(Args)]
pub struct WriteGenesisArgs {
    /// Output path for the genesis JSON file.
    #[arg(long)]
    pub output: String,
    /// Hex-encoded sender address (0x-prefixed).
    #[arg(long)]
    pub sender: String,
    /// Sender balance in wei (decimal string).
    #[arg(long, default_value = "1000000000000000000000000000")]
    pub sender_balance: String,
}

pub fn run(_args: WriteGenesisArgs) -> eyre::Result<()> {
    todo!("genesis generation not yet implemented")
}
