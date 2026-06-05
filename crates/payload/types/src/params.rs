//! Request/response types for L2 Engine API methods.

use alloy_primitives::Bytes;
use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Transaction list parameter for `engine_assembleL2BlockV2`.
///
/// go-ethereum's `AssembleL2BlockV2` takes a raw `[][]byte` positional parameter
/// (`eth/catalyst/l2_api.go`). Go's `encoding/json` serializes each `[]byte` element
/// as a **base64** string — unlike the hex-quantity [`Bytes`] used by every other
/// Morph engine type (which model their tx lists as `[]hexutil.Bytes`). To stay
/// wire-compatible with the consensus client, each element is decoded as base64.
///
/// For robustness against a future switch to hex (and to interoperate with reth-side
/// tooling), an element carrying a `0x` prefix is decoded as hex instead. The prefix
/// is unambiguous: standard base64 output never begins with `0x`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssembleV2Transactions(pub Vec<Bytes>);

impl AssembleV2Transactions {
    /// Consumes the wrapper and returns the decoded transaction bytes.
    pub fn into_inner(self) -> Vec<Bytes> {
        self.0
    }
}

impl Serialize for AssembleV2Transactions {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for tx in &self.0 {
            // Mirror go-ethereum's wire format: base64-encoded element strings.
            seq.serialize_element(&base64::engine::general_purpose::STANDARD.encode(tx))?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for AssembleV2Transactions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw: Vec<String> = Vec::deserialize(deserializer)?;
        let mut txs = Vec::with_capacity(raw.len());
        for (index, s) in raw.iter().enumerate() {
            let bytes = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                alloy_primitives::hex::decode(hex).map_err(|e| {
                    serde::de::Error::custom(format!("tx {index}: invalid hex: {e}"))
                })?
            } else {
                base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(|e| {
                        serde::de::Error::custom(format!("tx {index}: invalid base64: {e}"))
                    })?
            };
            txs.push(Bytes::from(bytes));
        }
        Ok(Self(txs))
    }
}

/// Parameters for engine_assembleL2Block.
///
/// This struct contains the input parameters for building a new L2 block.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssembleL2BlockParams {
    /// Block number to build.
    #[serde(with = "alloy_serde::quantity")]
    pub number: u64,

    /// Transactions to include in the block.
    /// These are RLP-encoded transaction bytes.
    #[serde(default)]
    pub transactions: Vec<Bytes>,

    /// Optional block timestamp.
    ///
    /// If not provided, builder can choose a local current timestamp.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::quantity::opt"
    )]
    pub timestamp: Option<u64>,
}

impl AssembleL2BlockParams {
    /// Create a new [`AssembleL2BlockParams`].
    pub fn new(number: u64, transactions: Vec<Bytes>) -> Self {
        Self {
            number,
            transactions,
            timestamp: None,
        }
    }

    /// Create params for an empty block.
    pub fn empty(number: u64) -> Self {
        Self {
            number,
            transactions: Vec::new(),
            timestamp: None,
        }
    }
}

/// Generic success/failure response for L2 Engine API methods.
///
/// This is used by methods like engine_validateL2Block that return
/// a simple success/failure status.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GenericResponse {
    /// Whether the operation was successful.
    pub success: bool,
}

impl GenericResponse {
    /// Create a success response.
    pub fn success() -> Self {
        Self { success: true }
    }

    /// Create a failure response.
    pub fn failure() -> Self {
        Self { success: false }
    }
}

impl From<bool> for GenericResponse {
    fn from(success: bool) -> Self {
        Self { success }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assemble_params_new() {
        let params = AssembleL2BlockParams::new(100, vec![Bytes::from(vec![0x01])]);
        assert_eq!(params.number, 100);
        assert_eq!(params.transactions.len(), 1);
    }

    #[test]
    fn test_assemble_params_empty() {
        let params = AssembleL2BlockParams::empty(100);
        assert_eq!(params.number, 100);
        assert!(params.transactions.is_empty());
    }

    #[test]
    fn test_generic_response() {
        let success = GenericResponse::success();
        assert!(success.success);

        let failure = GenericResponse::failure();
        assert!(!failure.success);
    }

    #[test]
    fn test_generic_response_from_bool() {
        let response: GenericResponse = true.into();
        assert!(response.success);

        let response: GenericResponse = false.into();
        assert!(!response.success);
    }

    #[test]
    fn test_assemble_params_serde() {
        let params = AssembleL2BlockParams::new(100, vec![Bytes::from(vec![0x01, 0x02])]);

        let json = serde_json::to_string(&params).expect("serialize");
        let decoded: AssembleL2BlockParams = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(params, decoded);
    }

    #[test]
    fn test_assemble_v2_txs_decodes_base64() {
        // go-ethereum marshals the `[][]byte` positional param via Go's encoding/json,
        // which base64-encodes each element. base64("0xdead") = "3q0=".
        let json = r#"["3q0="]"#;
        let txs: AssembleV2Transactions = serde_json::from_str(json).expect("deserialize base64");
        assert_eq!(txs.0, vec![Bytes::from(vec![0xde, 0xad])]);
    }

    #[test]
    fn test_assemble_v2_txs_decodes_hex_with_prefix() {
        // Robustness: also accept 0x-prefixed hex, so the type tolerates either wire
        // encoding (the `0x` prefix is unambiguous — base64 never starts with it).
        let json = r#"["0xbeef"]"#;
        let txs: AssembleV2Transactions = serde_json::from_str(json).expect("deserialize hex");
        assert_eq!(txs.0, vec![Bytes::from(vec![0xbe, 0xef])]);
    }

    #[test]
    fn test_assemble_v2_txs_empty() {
        let txs: AssembleV2Transactions = serde_json::from_str("[]").expect("deserialize empty");
        assert!(txs.0.is_empty());
    }

    #[test]
    fn test_assemble_v2_txs_base64_roundtrip() {
        let txs =
            AssembleV2Transactions(vec![Bytes::from(vec![0xde, 0xad]), Bytes::from(vec![0x01])]);
        let json = serde_json::to_string(&txs).expect("serialize");
        let decoded: AssembleV2Transactions = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(txs, decoded);
    }

    #[test]
    fn test_generic_response_serde() {
        let response = GenericResponse::success();

        let json = serde_json::to_string(&response).expect("serialize");
        assert_eq!(json, r#"{"success":true}"#);

        let decoded: GenericResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(response, decoded);
    }

    #[test]
    fn test_assemble_params_with_timestamp() {
        let mut params = AssembleL2BlockParams::new(100, vec![]);
        assert!(params.timestamp.is_none());

        params.timestamp = Some(1_700_000_000);
        assert_eq!(params.timestamp, Some(1_700_000_000));
    }

    #[test]
    fn test_assemble_params_serde_with_timestamp() {
        let json = r#"{
            "number": "0x64",
            "transactions": [],
            "timestamp": "0x6553f100"
        }"#;

        let params: AssembleL2BlockParams = serde_json::from_str(json).expect("deserialize");
        assert_eq!(params.number, 100);
        assert_eq!(params.timestamp, Some(0x6553f100));
    }

    #[test]
    fn test_assemble_params_serde_without_timestamp() {
        let json = r#"{
            "number": "0x1",
            "transactions": ["0xdead"]
        }"#;

        let params: AssembleL2BlockParams = serde_json::from_str(json).expect("deserialize");
        assert_eq!(params.number, 1);
        assert!(params.timestamp.is_none());
        assert_eq!(params.transactions.len(), 1);
    }

    #[test]
    fn test_assemble_params_default() {
        let params = AssembleL2BlockParams::default();
        assert_eq!(params.number, 0);
        assert!(params.transactions.is_empty());
        assert!(params.timestamp.is_none());
    }

    #[test]
    fn test_generic_response_failure_serde() {
        let response = GenericResponse::failure();
        let json = serde_json::to_string(&response).expect("serialize");
        assert_eq!(json, r#"{"success":false}"#);
    }
}
