//! Golden roots from morph-geth 5a0d0d771 (go-ethereum#371), which reads them back
//! from this same fixture.
//!
//! The fee token is registered with `priceRatio = 3` against `scale = 1`, so
//! converting ETH to token units is inexact, and the transaction carries one
//! non-zero calldata byte so its gas cost is not a multiple of that ratio. Those
//! two together are the only shape where rounding the prepaid fee and the refund
//! up independently disagrees with charging the ceiling of the net fee.
//!
//! Each fork runs three consecutive gas limits, whose prepaid conversions cover
//! every remainder modulo the price ratio:
//!
//! - Celadon collects `ceil(21_016 / 3) = 7_006` on all three.
//! - Emerald and Jade collect `7_005` on the first two — one token unit short —
//!   and `7_006` on the third, so they end on two distinct state roots where
//!   Celadon has one.
//!
//! The 21_016 also pins the transaction's gas: morph does not apply the EIP-7623
//! calldata floor, which would bill 21_040 and miss every root here.
use morph_statetest::runner::run_suite_str;

#[test]
fn celadon_alt_token_refund_matches_geth() {
    let outcomes = run_suite_str(include_str!("fixtures/celadon_alt_token_refund.json")).unwrap();
    assert_eq!(outcomes.len(), 9, "3 forks × 3 gas limits");
    for outcome in outcomes {
        assert!(
            outcome.pass,
            "{} / {}: {}",
            outcome.test, outcome.fork, outcome.error_msg
        );
    }
}
