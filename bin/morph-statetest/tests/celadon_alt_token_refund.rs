//! Golden roots from morph-geth 5a0d0d771 (go-ethereum#371), which reads them back
//! from this same fixture.
//!
//! The fee token is registered with `priceRatio = 3` against `scale = 1`, so
//! converting ETH to token units is inexact. Each fork runs three calldata
//! lengths against three consecutive gas limits:
//!
//! - one, two and three non-zero calldata bytes cost 21_016, 21_032 and 21_048
//!   gas, which covers every remainder of the *net* fee modulo the price ratio;
//! - the gas limits 100_001..=100_003 cover every remainder of the *prepaid* fee.
//!
//! Tokens collected, per gas limit:
//!
//! | net gas | Emerald, Jade       | Celadon             | floor without the credit |
//! |---------|---------------------|---------------------|--------------------------|
//! | 21_016  | 7_005, 7_005, 7_006 | 7_006, 7_006, 7_006 | 7_006, 7_006, 7_006      |
//! | 21_032  | 7_011, 7_010, 7_011 | 7_011, 7_011, 7_011 | 7_011, 7_011, 7_012      |
//! | 21_048  | 7_016, 7_016, 7_016 | 7_016, 7_016, 7_016 | 7_017, 7_016, 7_017      |
//!
//! Celadon collects `ceil(net / 3)` on every gas limit, so it ends on one state
//! root per calldata length. Emerald and Jade round the prepaid fee and the refund
//! up independently and come out a token unit short on three of the nine, so the
//! first two rows end on two roots each.
//!
//! The last column is why one calldata length is not enough. With a net fee of
//! 21_016 the prepaid rounding credit never carries into the refund, so a client
//! that rounds the refund down but drops the credit still lands on every root of
//! that row. The other two rows are the ones that pin the credit itself.
//!
//! The gas figures also pin the transaction's gas: morph does not apply the
//! EIP-7623 calldata floor, which would bill 21_040 for the first row and miss
//! every root here.
use morph_statetest::runner::run_suite_str;

#[test]
fn celadon_alt_token_refund_matches_geth() {
    let outcomes = run_suite_str(include_str!("fixtures/celadon_alt_token_refund.json")).unwrap();
    assert_eq!(
        outcomes.len(),
        27,
        "3 forks × 3 calldata lengths × 3 gas limits"
    );
    for outcome in outcomes {
        assert!(
            outcome.pass,
            "{} / {}: {}",
            outcome.test, outcome.fork, outcome.error_msg
        );
    }
}
