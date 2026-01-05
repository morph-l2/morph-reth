//! Request/response types for L2 Engine API methods.

use alloy_primitives::Bytes;
use serde::{Deserialize, Serialize};

/// Parameters for engine_assembleL2Block.
///
/// This struct contains the input parameters for building a new L2 block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssembleL2BlockParams {
    /// Block number to build.
    #[serde(with = "alloy_serde::quantity")]
    pub number: u64,

    /// Transactions to include in the block.
    /// These are RLP-encoded transaction bytes.
    #[serde(default)]
    pub transactions: Vec<Bytes>,
}

impl AssembleL2BlockParams {
    /// Create a new [`AssembleL2BlockParams`].
    pub fn new(number: u64, transactions: Vec<Bytes>) -> Self {
        Self {
            number,
            transactions,
        }
    }

    /// Create params for an empty block.
    pub fn empty(number: u64) -> Self {
        Self {
            number,
            transactions: Vec::new(),
        }
    }
}

/// Generic success/failure response for L2 Engine API methods.
///
/// This is used by methods like engine_validateL2Block that return
/// a simple success/failure status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    fn test_generic_response_serde() {
        let response = GenericResponse::success();

        let json = serde_json::to_string(&response).expect("serialize");
        assert_eq!(json, r#"{"success":true}"#);

        let decoded: GenericResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(response, decoded);
    }
}
