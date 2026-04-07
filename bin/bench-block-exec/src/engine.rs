/// Authenticated Engine RPC client for Morph L2 Engine API.
///
/// This module implements a JWT-authenticated client that talks to the Engine API
/// (`engine_assembleL2Block`, `engine_newL2Block`) and measures per-call timing.
use alloy_primitives::{Bytes, B256};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use eyre::ensure;
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HeaderMap, HeaderValue, HttpClient, HttpClientBuilder},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Hex-quantity serde helpers
// ---------------------------------------------------------------------------

/// Serialize/deserialize a `u64` as a `"0x…"` hex quantity string.
mod quantity {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(val: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{val:#x}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let s = String::deserialize(d)?;
        let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(&s);
        u64::from_str_radix(s, 16).map_err(serde::de::Error::custom)
    }
}

/// Serialize/deserialize an `Option<u64>` as a `"0x…"` hex quantity string.
mod quantity_opt {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(val: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        match val {
            Some(v) => s.serialize_str(&format!("{v:#x}")),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        let opt: Option<String> = Option::deserialize(d)?;
        match opt {
            None => Ok(None),
            Some(s) => {
                let s =
                    s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(&s);
                u64::from_str_radix(s, 16).map(Some).map_err(serde::de::Error::custom)
            }
        }
    }
}

/// Accepts both hex quantity strings (`"0x0"`) and bare JSON numbers (`0`).
///
/// morph-reth returns hex strings for `nextL1MessageIndex`; morph-geth may
/// return bare numbers. This module handles both transparently.
mod quantity_or_number {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(val: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Serialize as bare number: geth expects u64, reth accepts both.
        serializer.serialize_u64(*val)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Number(n) => n
                .as_u64()
                .ok_or_else(|| serde::de::Error::custom("number does not fit u64")),
            serde_json::Value::String(s) => {
                let s = s
                    .strip_prefix("0x")
                    .or_else(|| s.strip_prefix("0X"))
                    .unwrap_or(&s);
                u64::from_str_radix(s, 16).map_err(serde::de::Error::custom)
            }
            _ => Err(serde::de::Error::custom(
                "expected number or hex string for quantity_or_number",
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Engine API types
// ---------------------------------------------------------------------------

/// Parameters for `engine_assembleL2Block`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssembleL2BlockParams {
    /// Block number to assemble.
    #[serde(with = "quantity")]
    pub number: u64,

    /// Transactions to include (RLP-encoded).
    #[serde(default)]
    pub transactions: Vec<Bytes>,

    /// Optional block timestamp.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "quantity_opt"
    )]
    pub timestamp: Option<u64>,
}

/// Response from `engine_assembleL2Block` / input for `engine_newL2Block`.
///
/// Only the fields the benchmark actively reads are strongly typed.
/// All remaining fields (e.g. `miner`, `baseFeePerGas`, `logsBloom`,
/// `withdrawTrieRoot`) are captured in [`extra`] via `#[serde(flatten)]`
/// so they pass through to `engine_newL2Block` unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutableL2Data {
    /// Parent block hash.
    pub parent_hash: B256,

    /// Block number.
    #[serde(with = "quantity")]
    pub number: u64,

    /// Gas limit.
    #[serde(with = "quantity")]
    pub gas_limit: u64,

    /// Gas used by all transactions.
    #[serde(with = "quantity")]
    pub gas_used: u64,

    /// Block timestamp.
    #[serde(with = "quantity")]
    pub timestamp: u64,

    /// RLP-encoded transactions.
    #[serde(default)]
    pub transactions: Vec<Bytes>,

    /// State root after execution.
    pub state_root: B256,

    /// Receipts root.
    pub receipts_root: B256,

    /// Block hash.
    pub hash: B256,

    /// Next L1 message queue index.
    ///
    /// morph-reth serializes this as a hex quantity string (like all other numeric
    /// fields). morph-geth may use a bare JSON number. Use `quantity_or_number` to
    /// accept both formats.
    #[serde(with = "quantity_or_number")]
    pub next_l1_message_index: u64,

    /// Remaining fields preserved for round-trip fidelity.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Per-block timing record
// ---------------------------------------------------------------------------

/// Timing data for a single block's assemble + import cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockTiming {
    pub block_number: u64,
    pub tx_count: u64,
    pub assemble_ms: f64,
    pub import_ms: f64,
    pub total_ms: f64,
    pub layer: String,
    pub engine: String,
}

/// Extended timing record for new benchmark modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockTimingV2 {
    pub block_number: u64,
    pub tx_count: u64,
    pub expected_tx_count: u64,
    pub engine: String,
    pub mode: String,
    pub workload: String,
    pub senders: u64,
    pub warmup_blocks: u64,

    // Timing breakdown (milliseconds)
    pub submit_ms: f64,
    pub pool_wait_ms: f64,
    pub assemble_ms: f64,
    pub import_ms: f64,
    pub total_ms: f64,

    // Derived metrics
    pub gas_used: u64,
    pub tps: f64,
    pub mgas_per_sec: f64,
    pub inclusion_rate: f64,

    // Cumulative (for sustained mode)
    pub cumulative_blocks: u64,
    pub cumulative_txs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rolling_avg_tps_100: Option<f64>,

    // Error flag
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
}

impl BlockTimingV2 {
    /// Compute derived fields after timing is recorded.
    pub fn finalize(&mut self) {
        self.total_ms = self.submit_ms + self.pool_wait_ms + self.assemble_ms + self.import_ms;
        if self.total_ms > 0.0 {
            let secs = self.total_ms / 1000.0;
            self.tps = self.tx_count as f64 / secs;
            self.mgas_per_sec = self.gas_used as f64 / secs / 1_000_000.0;
        }
        self.inclusion_rate = if self.expected_tx_count > 0 {
            self.tx_count as f64 / self.expected_tx_count as f64
        } else {
            1.0
        };
    }
}

// ---------------------------------------------------------------------------
// JWT helpers
// ---------------------------------------------------------------------------

/// HMAC-SHA256: `H((K ^ opad) || H((K ^ ipad) || message))`
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;

    // If key > block size, hash it first.
    let key = if key.len() > BLOCK_SIZE {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };

    // Pad key to BLOCK_SIZE.
    let mut padded = vec![0u8; BLOCK_SIZE];
    padded[..key.len()].copy_from_slice(&key);

    // ipad = key ^ 0x36
    let mut ipad = vec![0x36u8; BLOCK_SIZE];
    for (i, b) in padded.iter().enumerate() {
        ipad[i] ^= b;
    }

    // opad = key ^ 0x5c
    let mut opad = vec![0x5cu8; BLOCK_SIZE];
    for (i, b) in padded.iter().enumerate() {
        opad[i] ^= b;
    }

    // H((K ^ ipad) || message)
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(message);
    let inner_hash = inner.finalize();

    // H((K ^ opad) || inner_hash)
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner_hash);

    let result = outer.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Create a JWT token signed with the given hex-encoded secret.
///
/// The secret must be a 64-character hex string representing 32 bytes.
/// The token carries `{"iat":<unix_timestamp>}` and uses HS256.
pub fn create_jwt_token(secret_hex: &str) -> eyre::Result<String> {
    let secret_hex = secret_hex.strip_prefix("0x").unwrap_or(secret_hex);
    ensure!(
        secret_hex.len() == 64,
        "JWT secret must be 32 bytes (64 hex chars), got {} hex chars",
        secret_hex.len()
    );
    let secret = hex_decode(secret_hex)?;

    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);

    let iat = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"iat":{iat}}}"#));

    let signing_input = format!("{header}.{payload}");
    let signature = hmac_sha256(&secret, signing_input.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(signature);

    Ok(format!("{signing_input}.{sig_b64}"))
}

/// Decode a hex string (without `0x` prefix) into bytes.
fn hex_decode(hex: &str) -> eyre::Result<Vec<u8>> {
    ensure!(hex.len() % 2 == 0, "hex string has odd length");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(Into::into))
        .collect()
}

// ---------------------------------------------------------------------------
// EngineClient
// ---------------------------------------------------------------------------

/// JWT-authenticated client for the Morph L2 Engine API.
pub struct EngineClient {
    jwt_secret_hex: String,
}

impl EngineClient {
    /// Create a new `EngineClient`.
    ///
    /// `engine_rpc_url` is stored for documentation / logging purposes;
    /// the actual URL is passed per-call so callers can target different nodes.
    pub fn new(_engine_rpc_url: &str, jwt_secret_hex: String) -> eyre::Result<Self> {
        // Validate the secret eagerly.
        let stripped = jwt_secret_hex
            .strip_prefix("0x")
            .unwrap_or(&jwt_secret_hex);
        ensure!(
            stripped.len() == 64,
            "JWT secret must be 32 bytes (64 hex chars), got {} hex chars",
            stripped.len()
        );
        Ok(Self { jwt_secret_hex })
    }

    /// Build a fresh [`HttpClient`] with a Bearer JWT Authorization header.
    fn authed_client(&self, url: &str) -> eyre::Result<HttpClient> {
        let token = create_jwt_token(&self.jwt_secret_hex)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        let client = HttpClientBuilder::default()
            .set_headers(headers)
            .build(url)?;
        Ok(client)
    }

    /// Call `engine_assembleL2Block` and return the response along with elapsed
    /// milliseconds.
    pub async fn assemble_l2_block(
        &self,
        url: &str,
        params: AssembleL2BlockParams,
    ) -> eyre::Result<(ExecutableL2Data, f64)> {
        let client = self.authed_client(url)?;
        let start = Instant::now();
        let data: ExecutableL2Data = client
            .request("engine_assembleL2Block", (params,))
            .await?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        Ok((data, elapsed_ms))
    }

    /// Call `engine_newL2Block` and return the elapsed milliseconds.
    pub async fn new_l2_block(
        &self,
        url: &str,
        data: ExecutableL2Data,
    ) -> eyre::Result<f64> {
        let client = self.authed_client(url)?;
        let start = Instant::now();
        let _: () = client.request("engine_newL2Block", (data,)).await?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        Ok(elapsed_ms)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_token_is_valid_format() {
        // 32 random bytes as hex
        let secret = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let token = create_jwt_token(secret).expect("should create token");

        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT must have 3 dot-separated parts");

        // Header should decode to valid JSON with alg=HS256
        let header_bytes = URL_SAFE_NO_PAD.decode(parts[0]).expect("decode header");
        let header: serde_json::Value =
            serde_json::from_slice(&header_bytes).expect("parse header JSON");
        assert_eq!(header["alg"], "HS256");
        assert_eq!(header["typ"], "JWT");

        // Payload should contain an "iat" numeric field
        let payload_bytes = URL_SAFE_NO_PAD.decode(parts[1]).expect("decode payload");
        let payload: serde_json::Value =
            serde_json::from_slice(&payload_bytes).expect("parse payload JSON");
        assert!(
            payload["iat"].is_number(),
            "iat must be a number, got: {payload:?}"
        );
    }

    #[test]
    fn jwt_rejects_short_secret() {
        let short = "deadbeef"; // only 4 bytes
        let result = create_jwt_token(short);
        assert!(result.is_err(), "should reject a non-32-byte secret");
    }

    #[test]
    fn assemble_params_serializes_to_camel_case() {
        let params = AssembleL2BlockParams {
            number: 100,
            transactions: vec![],
            timestamp: Some(0x6553f100),
        };

        let json = serde_json::to_value(&params).expect("serialize");
        let obj = json.as_object().expect("should be object");

        // camelCase field names
        assert!(obj.contains_key("number"), "missing 'number'");
        assert!(obj.contains_key("transactions"), "missing 'transactions'");
        assert!(obj.contains_key("timestamp"), "missing 'timestamp'");

        // Hex quantity format
        assert_eq!(obj["number"], "0x64");
        assert_eq!(obj["timestamp"], "0x6553f100");
    }

    #[test]
    fn assemble_params_omits_none_timestamp() {
        let params = AssembleL2BlockParams {
            number: 1,
            transactions: vec![],
            timestamp: None,
        };

        let json = serde_json::to_value(&params).expect("serialize");
        let obj = json.as_object().expect("should be object");
        assert!(
            !obj.contains_key("timestamp"),
            "None timestamp should be omitted"
        );
    }

    #[test]
    fn executable_l2_data_roundtrip_with_extra_fields() {
        let json = r#"{
            "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "miner": "0x0000000000000000000000000000000000000002",
            "number": "0x64",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x5208",
            "timestamp": "0x499602d2",
            "transactions": [],
            "stateRoot": "0x0000000000000000000000000000000000000000000000000000000000000003",
            "receiptsRoot": "0x0000000000000000000000000000000000000000000000000000000000000004",
            "hash": "0x0000000000000000000000000000000000000000000000000000000000000006",
            "nextL1MessageIndex": 10,
            "logsBloom": "0x00",
            "withdrawTrieRoot": "0x0000000000000000000000000000000000000000000000000000000000000005",
            "baseFeePerGas": "0x3b9aca00"
        }"#;

        let data: ExecutableL2Data = serde_json::from_str(json).expect("deserialize");
        assert_eq!(data.number, 100);
        assert_eq!(data.gas_limit, 30_000_000);
        assert_eq!(data.gas_used, 21000);
        assert_eq!(data.next_l1_message_index, 10);

        // Extra fields must survive the round-trip.
        assert!(data.extra.contains_key("miner"), "miner should be in extra");
        assert!(
            data.extra.contains_key("logsBloom"),
            "logsBloom should be in extra"
        );
        assert!(
            data.extra.contains_key("withdrawTrieRoot"),
            "withdrawTrieRoot should be in extra"
        );
        assert!(
            data.extra.contains_key("baseFeePerGas"),
            "baseFeePerGas should be in extra"
        );

        // Re-serialize and verify the extra fields are still present.
        let re_json = serde_json::to_value(&data).expect("re-serialize");
        let obj = re_json.as_object().expect("should be object");
        assert!(obj.contains_key("miner"));
        assert!(obj.contains_key("logsBloom"));
        assert!(obj.contains_key("withdrawTrieRoot"));
        assert!(obj.contains_key("baseFeePerGas"));
    }

    #[test]
    fn hmac_sha256_known_vector() {
        // RFC 4231 Test Case 2
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let expected: [u8; 32] = [
            0x5b, 0xdc, 0xc1, 0x46, 0xbf, 0x60, 0x75, 0x4e, 0x6a, 0x04, 0x24, 0x26, 0x08, 0x95,
            0x75, 0xc7, 0x5a, 0x00, 0x3f, 0x08, 0x9d, 0x27, 0x39, 0x83, 0x9d, 0xec, 0x58, 0xb9,
            0x64, 0xec, 0x38, 0x43,
        ];
        assert_eq!(hmac_sha256(key, data), expected);
    }

    #[test]
    fn engine_client_rejects_short_secret() {
        let result = EngineClient::new("http://localhost:8551", "deadbeef".into());
        assert!(result.is_err());
    }

    #[test]
    fn jwt_accepts_0x_prefixed_secret() {
        let secret = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let token = create_jwt_token(secret).expect("should handle 0x prefix");
        assert_eq!(token.split('.').count(), 3);
    }

    #[test]
    fn block_timing_serde_roundtrip() {
        let timing = BlockTiming {
            block_number: 42,
            tx_count: 10,
            assemble_ms: 12.5,
            import_ms: 8.3,
            total_ms: 20.8,
            layer: "eth-transfer".into(),
            engine: "reth".into(),
        };
        let json = serde_json::to_string(&timing).expect("serialize");
        let decoded: BlockTiming = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.block_number, 42);
        assert_eq!(decoded.tx_count, 10);
        assert!((decoded.total_ms - 20.8).abs() < f64::EPSILON);
    }
}
