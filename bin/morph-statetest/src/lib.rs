//! Morph-compatible Ethereum state-test parsing and execution.

pub mod runner;
pub mod schema;

#[cfg(test)]
mod tests {
    use crate::{runner::run_suite_str, schema::MorphTestSuite};
    use alloy_primitives::B256;

    const MINIMAL_SUITE: &str = r#"{
      "minimal_transfer": {
        "env": {
          "currentCoinbase": "0x0000000000000000000000000000000000000000",
          "currentDifficulty": "0x0",
          "currentGasLimit": "0x989680",
          "currentNumber": "0x1",
          "currentTimestamp": "0x1",
          "currentBaseFee": "0x1"
        },
        "pre": {
          "0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b": {
            "balance": "0xde0b6b3a7640000",
            "nonce": "0x0",
            "code": "0x",
            "storage": {}
          }
        },
        "transaction": {
          "nonce": "0x0",
          "gasPrice": "0x1",
          "gasLimit": ["0x5208"],
          "to": "0x0000000000000000000000000000000000000001",
          "value": ["0x1"],
          "data": ["0x"],
          "secretKey": "0x45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8",
          "feeTokenID": "0x0",
          "feeLimit": "0x0",
          "version": "0x0",
          "reference": "0x0000000000000000000000000000000000000000000000000000000000000000",
          "memo": "0x"
        },
        "post": {
          "Jade": [
            {
              "indexes": { "data": 0, "gas": 0, "value": 0 },
              "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
              "logs": "0x0000000000000000000000000000000000000000000000000000000000000000",
              "expectException": null
            }
          ]
        }
      }
    }"#;

    #[test]
    fn preserves_morph_fork_names() {
        let suite: MorphTestSuite = serde_json::from_str(MINIMAL_SUITE).unwrap();
        let unit = suite.0.get("minimal_transfer").unwrap();

        assert!(unit.post.contains_key("Jade"));
    }

    #[test]
    fn accepts_hex_string_transaction_type() {
        let suite_json = MINIMAL_SUITE.replace(
            r#""transaction": {
          "nonce": "0x0","#,
            r#""transaction": {
          "type": "0x7f",
          "nonce": "0x0","#,
        );
        let suite: MorphTestSuite = serde_json::from_str(&suite_json).unwrap();
        let unit = suite.0.get("minimal_transfer").unwrap();

        assert_eq!(unit.transaction.tx_type, Some(0x7f));
    }

    #[test]
    fn executes_minimal_transfer_and_returns_actual_roots() {
        let outcomes = run_suite_str(MINIMAL_SUITE).unwrap();

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].fork, "Jade");
        assert_ne!(outcomes[0].state_root, B256::ZERO);
    }
}
