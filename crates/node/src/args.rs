//! Morph node CLI arguments.

use clap::Args;

/// Morph-specific CLI arguments.
///
/// Extends the standard reth CLI. Currently has no Morph-only flags: block packing is
/// bounded by header `gasLimit` and the payload builder time budget.
///
/// Note: Block building deadline is configured via reth's built-in `--builder.deadline` flag.
#[derive(Debug, Clone, Args, Default)]
pub struct MorphArgs {}

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
        let _args = CommandParser::<MorphArgs>::parse_from(["test"]).args;
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
    fn unused_packing_flags_are_not_supported() {
        assert!(
            CommandParser::<MorphArgs>::try_parse_from([
                "test",
                "--morph.max-tx-payload-bytes",
                "1"
            ])
            .is_err()
        );
        assert!(
            CommandParser::<MorphArgs>::try_parse_from(["test", "--morph.max-tx-per-block", "1"])
                .is_err()
        );
    }
}
