//! Morph node CLI arguments.

use clap::Args;
use morph_chainspec::MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK;

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
}

impl Default for MorphArgs {
    fn default() -> Self {
        Self {
            max_tx_payload_bytes: MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES,
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
