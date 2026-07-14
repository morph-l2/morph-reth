//! Morph node CLI arguments.

use std::path::PathBuf;

use clap::Args;

/// Default maximum transaction payload bytes per block (120KB).
///
/// This matches Morph's go-ethereum configuration.
pub const MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES: u64 = 122_880;

/// Default proof-history retention window: 30 days at a two-second block time.
pub const DEFAULT_PROOFS_HISTORY_WINDOW: u64 = 1_296_000;

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

    /// Enable the forward-only historical proof index and its RPC overrides.
    #[arg(long = "proofs-history", default_value_t = false)]
    pub proofs_history: bool,

    /// Proof-history MDBX directory.
    ///
    /// Defaults to `<chain-datadir>/historical-proofs` when proof history is enabled.
    #[arg(long = "proofs-history.storage-path", value_name = "PATH")]
    pub proofs_history_storage_path: Option<PathBuf>,

    /// Number of canonical blocks retained in proof history.
    #[arg(
        long = "proofs-history.window",
        value_name = "BLOCKS",
        default_value_t = DEFAULT_PROOFS_HISTORY_WINDOW,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub proofs_history_window: u64,

    /// Re-execute every Nth block to verify notification trie data; zero disables verification.
    #[arg(
        long = "proofs-history.verification-interval",
        value_name = "BLOCKS",
        default_value_t = 0
    )]
    pub proofs_history_verification_interval: u64,
}

impl Default for MorphArgs {
    fn default() -> Self {
        Self {
            max_tx_payload_bytes: MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES,
            max_tx_per_block: None,
            proofs_history: false,
            proofs_history_storage_path: None,
            proofs_history_window: DEFAULT_PROOFS_HISTORY_WINDOW,
            proofs_history_verification_interval: 0,
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
        assert!(!args.proofs_history);
        assert_eq!(args.proofs_history_storage_path, None);
        assert_eq!(args.proofs_history_window, 1_296_000);
        assert_eq!(args.proofs_history_verification_interval, 0);
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

    #[test]
    fn test_all_args_combined() {
        let args = CommandParser::<MorphArgs>::parse_from([
            "test",
            "--morph.max-tx-payload-bytes",
            "200000",
            "--morph.max-tx-per-block",
            "1000",
        ])
        .args;
        assert_eq!(args.max_tx_payload_bytes, 200000);
        assert_eq!(args.max_tx_per_block, Some(1000));
    }

    #[test]
    fn reference_index_disable_flag_is_not_supported() {
        assert!(
            CommandParser::<MorphArgs>::try_parse_from(["test", "--morph.disable-reference-index"])
                .is_err()
        );
        assert!(
            CommandParser::<MorphArgs>::try_parse_from(["test", "--disable-reference-index"])
                .is_err()
        );
    }

    #[test]
    fn test_proofs_history_args() {
        let args = CommandParser::<MorphArgs>::parse_from([
            "test",
            "--proofs-history",
            "--proofs-history.storage-path",
            "/tmp/morph-proofs",
            "--proofs-history.window",
            "2592000",
            "--proofs-history.verification-interval",
            "64",
        ])
        .args;

        assert!(args.proofs_history);
        assert_eq!(
            args.proofs_history_storage_path.as_deref(),
            Some(std::path::Path::new("/tmp/morph-proofs"))
        );
        assert_eq!(args.proofs_history_window, 2_592_000);
        assert_eq!(args.proofs_history_verification_interval, 64);
    }

    #[test]
    fn test_default_trait_impl() {
        let args = MorphArgs::default();
        assert_eq!(
            args.max_tx_payload_bytes,
            MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES
        );
        assert!(args.max_tx_per_block.is_none());
    }
}
