//! Morph node CLI arguments.

use clap::Args;

/// Default maximum transaction payload bytes per block (120KB).
///
/// This matches Morph's go-ethereum configuration.
pub const MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES: u64 = 122_880;

/// Morph-specific CLI arguments.
///
/// These arguments extend the standard reth CLI with Morph-specific options
/// for block building and transaction limits.
///
/// Note: Block building deadline is configured via reth's built-in `--builder.deadline` flag.
#[derive(Debug, Clone, Args)]
#[command(next_help_heading = "Morph")]
pub struct MorphArgs {
    /// Maximum transaction payload bytes per block.
    ///
    /// Limits the total size of transactions included in a single block.
    /// Default: 122880 bytes (120KB), matching Morph's go-ethereum configuration.
    #[arg(
        long = "morph.max-tx-payload-bytes",
        value_name = "BYTES",
        default_value_t = MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES
    )]
    pub max_tx_payload_bytes: u64,

    /// Maximum number of transactions per block.
    ///
    /// If not set, there is no limit on the number of transactions.
    /// Morph Holesky testnet uses 1000 as the default limit.
    #[arg(long = "morph.max-tx-per-block", value_name = "COUNT")]
    pub max_tx_per_block: Option<u64>,
}

impl Default for MorphArgs {
    fn default() -> Self {
        Self {
            max_tx_payload_bytes: MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES,
            max_tx_per_block: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn test_default_args() {
        let args = CommandParser::<MorphArgs>::parse_from(["test"]).args;
        assert_eq!(
            args.max_tx_payload_bytes,
            MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES
        );
        assert_eq!(args.max_tx_per_block, None);
    }

    #[test]
    fn test_custom_args() {
        let args = CommandParser::<MorphArgs>::parse_from([
            "test",
            "--morph.max-tx-payload-bytes",
            "100000",
            "--morph.max-tx-per-block",
            "500",
        ])
        .args;
        assert_eq!(args.max_tx_payload_bytes, 100000);
        assert_eq!(args.max_tx_per_block, Some(500));
    }
}
