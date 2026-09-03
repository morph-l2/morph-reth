//! Morph node CLI arguments.

use std::path::PathBuf;

use clap::Args;
use morph_chainspec::MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK;
use morph_proofs::DEFAULT_PROOFS_HISTORY_WINDOW;
use morph_rpc::eth::proofs::DEFAULT_MAX_MULTI_PROOF_TARGETS;

/// Default maximum L2 transaction payload bytes per block (720 KiB).
///
/// `720 KiB = 120 KiB × 6`. A Morph batch can carry up to 6 EIP-4844 blobs.
/// Each blob's usable payload is `4096 × 31 = 126_976` bytes (~124 KiB), so
/// six blobs hold 761_856 bytes uncompressed. 120 KiB per blob is the
/// historical headroom under that usable size; six of them stay under the
/// uncompressed 6-blob budget and do not require the submitter to split a
/// single L2 block.
pub const MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES: u64 = MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK;

/// Morph-specific CLI arguments.
///
/// Block packing is bounded by header `gasLimit`, the payload builder time
/// budget, and `--morph.max-tx-payload-bytes` (the uncompressed L2 payload
/// that must fit in one 6-blob batch).
///
/// Note: Block building deadline is configured via reth's built-in `--builder.deadline` flag.
#[derive(Debug, Clone, Args)]
#[command(next_help_heading = "Morph")]
pub struct MorphArgs {
    /// Maximum L2 transaction payload bytes per block (L1 messages excluded).
    ///
    /// Default: 737280 bytes (720 KiB), sized so one L2 block fits in a
    /// 6-blob batch even without compression.
    ///
    /// Import-time consensus always enforces
    /// [`morph_chainspec::MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK`], independent of
    /// this flag. Packing above that value produces blocks other nodes reject.
    #[arg(
        long = "morph.max-tx-payload-bytes",
        value_name = "BYTES",
        default_value_t = MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES
    )]
    pub max_tx_payload_bytes: u64,

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

    /// Maximum account targets accepted by one `eth_getMultiProof` request.
    ///
    /// Each target costs an account-trie path plus a storage-trie cursor, so this bounds the most
    /// expensive dimension of a batch. Raise it when a prover needs blocks that touch more
    /// accounts than the default in one round trip; the separate 1024 storage-key cap is fixed to
    /// match go-ethereum's `eth_getProof`.
    #[arg(
        long = "proofs-history.max-multi-proof-targets",
        value_name = "COUNT",
        default_value_t = DEFAULT_MAX_MULTI_PROOF_TARGETS,
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..)
    )]
    pub proofs_history_max_multi_proof_targets: usize,
}

impl Default for MorphArgs {
    fn default() -> Self {
        Self {
            max_tx_payload_bytes: MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES,
            proofs_history: false,
            proofs_history_storage_path: None,
            proofs_history_window: DEFAULT_PROOFS_HISTORY_WINDOW,
            proofs_history_verification_interval: 0,
            proofs_history_max_multi_proof_targets: DEFAULT_MAX_MULTI_PROOF_TARGETS,
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
        assert_eq!(args.max_tx_payload_bytes, 720 * 1024);
        assert!(!args.proofs_history);
        assert_eq!(args.proofs_history_storage_path, None);
        assert_eq!(args.proofs_history_window, DEFAULT_PROOFS_HISTORY_WINDOW);
        assert_eq!(args.proofs_history_verification_interval, 0);
        assert_eq!(
            args.proofs_history_max_multi_proof_targets,
            DEFAULT_MAX_MULTI_PROOF_TARGETS
        );
    }

    #[test]
    fn test_custom_payload_bytes() {
        let args = CommandParser::<MorphArgs>::parse_from([
            "test",
            "--morph.max-tx-payload-bytes",
            "100000",
        ])
        .args;
        assert_eq!(args.max_tx_payload_bytes, 100000);
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
            "--proofs-history.max-multi-proof-targets",
            "512",
        ])
        .args;

        assert!(args.proofs_history);
        assert_eq!(
            args.proofs_history_storage_path.as_deref(),
            Some(std::path::Path::new("/tmp/morph-proofs"))
        );
        assert_eq!(args.proofs_history_window, 2_592_000);
        assert_eq!(args.proofs_history_verification_interval, 64);
        assert_eq!(args.proofs_history_max_multi_proof_targets, 512);
    }

    #[test]
    fn rejects_a_zero_multi_proof_target_limit() {
        // Zero would reject every non-empty batch, so clap must refuse it up front.
        assert!(
            CommandParser::<MorphArgs>::try_parse_from([
                "test",
                "--proofs-history.max-multi-proof-targets",
                "0",
            ])
            .is_err()
        );
    }

    #[test]
    fn unused_tx_count_flag_is_not_supported() {
        assert!(
            CommandParser::<MorphArgs>::try_parse_from(["test", "--morph.max-tx-per-block", "1"])
                .is_err()
        );
    }

    #[test]
    fn test_default_trait_impl() {
        let args = MorphArgs::default();
        assert_eq!(
            args.max_tx_payload_bytes,
            MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES
        );
    }
}
