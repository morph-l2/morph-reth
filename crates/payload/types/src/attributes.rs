//! Morph payload attributes types.

use alloy_eips::eip2718::Decodable2718;
use alloy_eips::eip4895::{Withdrawal, Withdrawals};
use alloy_primitives::{Address, B256, Bytes};
use alloy_rpc_types_engine::{PayloadAttributes, PayloadId};
use morph_primitives::MorphTxEnvelope;
use reth_payload_builder::EthPayloadBuilderAttributes;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives_traits::{Recovered, SignerRecoverable, WithEncoded};
use sha2::{Digest, Sha256};

/// Morph-specific payload attributes for Engine API.
///
/// This extends the standard Ethereum [`PayloadAttributes`] with L2-specific fields
/// for L1 message inclusion.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphPayloadAttributes {
    /// Standard Ethereum payload attributes.
    #[serde(flatten)]
    pub inner: PayloadAttributes,

    /// L1 message transactions to include at the beginning of the block.
    ///
    /// **IMPORTANT**: This field contains **only L1 messages** (L1→L2 deposit transactions).
    /// L2 transactions are always pulled from the transaction pool, matching go-ethereum's behavior.
    ///
    /// L1 messages:
    /// - Must have sequential queue indices
    /// - Are never in the mempool
    /// - Must be explicitly provided by the sequencer
    /// - Are executed before any L2 transactions
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transactions: Option<Vec<Bytes>>,

    /// Optional gas limit override used by derivation/safe import.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::quantity::opt"
    )]
    pub gas_limit: Option<u64>,

    /// Optional base fee override used by derivation/safe import.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::quantity::opt"
    )]
    pub base_fee_per_gas: Option<u64>,
}

impl reth_payload_primitives::PayloadAttributes for MorphPayloadAttributes {
    fn timestamp(&self) -> u64 {
        self.inner.timestamp
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.inner.withdrawals.as_ref()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root
    }
}

/// Internal payload builder attributes.
///
/// This is the internal representation used by the payload builder,
/// with decoded L1 messages and computed payload ID.
#[derive(Debug, Clone)]
pub struct MorphPayloadBuilderAttributes {
    /// Inner Ethereum payload builder attributes.
    pub inner: EthPayloadBuilderAttributes,

    /// Decoded L1 message transactions with original encoded bytes.
    ///
    /// **IMPORTANT**: This contains **only L1 messages**, not L2 transactions.
    /// L2 transactions are always pulled from the transaction pool.
    ///
    /// L1 messages are decoded and recovered during construction to avoid
    /// repeated decoding in the payload builder.
    pub transactions: Vec<WithEncoded<Recovered<MorphTxEnvelope>>>,

    /// Optional gas limit override propagated to EVM env construction.
    pub gas_limit: Option<u64>,

    /// Optional base fee override propagated to EVM env construction.
    pub base_fee_per_gas: Option<u64>,
}

impl PayloadBuilderAttributes for MorphPayloadBuilderAttributes {
    type RpcPayloadAttributes = MorphPayloadAttributes;
    type Error = alloy_rlp::Error;

    fn try_new(
        parent: B256,
        attributes: MorphPayloadAttributes,
        version: u8,
    ) -> Result<Self, Self::Error> {
        let id = payload_id_morph(&parent, &attributes, version);

        // Decode and recover L1 message transactions
        let transactions = attributes
            .transactions
            .unwrap_or_default()
            .into_iter()
            .map(|data| {
                let mut buf = data.as_ref();
                let tx = MorphTxEnvelope::decode_2718(&mut buf)?;
                if !buf.is_empty() {
                    return Err(alloy_rlp::Error::UnexpectedLength);
                }
                let recovered = tx
                    .try_into_recovered()
                    .map_err(|_| alloy_rlp::Error::Custom("failed to recover signer"))?;
                Ok(WithEncoded::new(data, recovered))
            })
            .collect::<Result<Vec<_>, alloy_rlp::Error>>()?;

        // Build inner Ethereum attributes
        let inner = EthPayloadBuilderAttributes {
            id,
            parent,
            timestamp: attributes.inner.timestamp,
            suggested_fee_recipient: attributes.inner.suggested_fee_recipient,
            prev_randao: attributes.inner.prev_randao,
            withdrawals: attributes.inner.withdrawals.unwrap_or_default().into(),
            parent_beacon_block_root: attributes.inner.parent_beacon_block_root,
        };

        Ok(Self {
            inner,
            transactions,
            gas_limit: attributes.gas_limit,
            base_fee_per_gas: attributes.base_fee_per_gas,
        })
    }

    fn payload_id(&self) -> PayloadId {
        self.inner.id
    }

    fn parent(&self) -> B256 {
        self.inner.parent
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root
    }

    fn suggested_fee_recipient(&self) -> Address {
        self.inner.suggested_fee_recipient
    }

    fn prev_randao(&self) -> B256 {
        self.inner.prev_randao
    }

    fn withdrawals(&self) -> &Withdrawals {
        &self.inner.withdrawals
    }
}

impl MorphPayloadBuilderAttributes {
    /// Returns true if there are L1 messages to execute.
    pub fn has_l1_messages(&self) -> bool {
        !self.transactions.is_empty()
    }
}

impl From<EthPayloadBuilderAttributes> for MorphPayloadBuilderAttributes {
    fn from(inner: EthPayloadBuilderAttributes) -> Self {
        Self {
            inner,
            transactions: vec![],
            gas_limit: None,
            base_fee_per_gas: None,
        }
    }
}

/// Compute payload ID from parent hash and attributes.
///
/// Uses SHA-256 hashing with the version byte as the first byte of the result.
fn payload_id_morph(parent: &B256, attributes: &MorphPayloadAttributes, version: u8) -> PayloadId {
    let mut hasher = Sha256::new();

    // Hash parent
    hasher.update(parent.as_slice());

    // Hash timestamp
    hasher.update(&attributes.inner.timestamp.to_be_bytes()[..]);

    // Hash prev_randao
    hasher.update(attributes.inner.prev_randao.as_slice());

    // Hash suggested_fee_recipient
    hasher.update(attributes.inner.suggested_fee_recipient.as_slice());

    // Hash withdrawals if present
    if let Some(withdrawals) = &attributes.inner.withdrawals {
        let mut buf = Vec::new();
        alloy_rlp::encode_list(withdrawals, &mut buf);
        hasher.update(&buf);
    }

    // Hash parent beacon block root if present
    if let Some(root) = &attributes.inner.parent_beacon_block_root {
        hasher.update(root.as_slice());
    }

    // Hash whether L1 message list was explicitly supplied.
    hasher.update([u8::from(attributes.transactions.is_some())]);

    // Hash L1 messages if present.
    if let Some(txs) = &attributes.transactions {
        hasher.update(&txs.len().to_be_bytes()[..]);
        for tx in txs {
            let tx_hash = alloy_primitives::keccak256(tx);
            hasher.update(tx_hash.as_slice());
        }
    }

    // Hash optional gas/base fee overrides.
    if let Some(gas_limit) = attributes.gas_limit {
        hasher.update([1u8]);
        hasher.update(gas_limit.to_be_bytes());
    } else {
        hasher.update([0u8]);
    }
    if let Some(base_fee) = attributes.base_fee_per_gas {
        hasher.update([1u8]);
        hasher.update(base_fee.to_be_bytes());
    } else {
        hasher.update([0u8]);
    }

    // Finalize and create payload ID
    let mut result = hasher.finalize();
    result[0] = version;

    PayloadId::new(
        result.as_slice()[..8]
            .try_into()
            .expect("sufficient length"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_attributes() -> MorphPayloadAttributes {
        MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp: 1234567890,
                prev_randao: B256::random(),
                suggested_fee_recipient: Address::random(),
                withdrawals: None,
                parent_beacon_block_root: None,
            },
            transactions: None,
            gas_limit: None,
            base_fee_per_gas: None,
        }
    }

    #[test]
    fn test_default_attributes() {
        let attrs = MorphPayloadAttributes::default();
        assert!(attrs.transactions.is_none());
    }

    #[test]
    fn test_with_transactions() {
        let mut attrs = create_test_attributes();
        attrs.transactions = Some(vec![Bytes::from(vec![0x01])]);

        assert_eq!(attrs.transactions.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_payload_id_deterministic() {
        let parent = B256::random();
        let attrs = create_test_attributes();

        let id1 = payload_id_morph(&parent, &attrs, 1);
        let id2 = payload_id_morph(&parent, &attrs, 1);

        assert_eq!(id1, id2);
    }

    #[test]
    fn test_payload_id_different_versions() {
        let parent = B256::random();
        let attrs = create_test_attributes();

        let id_v1 = payload_id_morph(&parent, &attrs, 1);
        let id_v2 = payload_id_morph(&parent, &attrs, 2);

        // Different versions should produce different IDs
        assert_ne!(id_v1, id_v2);
    }

    #[test]
    fn test_payload_id_different_with_transactions() {
        let parent = B256::random();
        let attrs1 = create_test_attributes();
        let mut attrs2 = create_test_attributes();
        attrs2.transactions = Some(vec![Bytes::from(vec![0x01])]);

        let id1 = payload_id_morph(&parent, &attrs1, 1);
        let id2 = payload_id_morph(&parent, &attrs2, 1);

        // Different transactions should produce different IDs
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_serde_roundtrip() {
        let mut attrs = create_test_attributes();
        attrs.transactions = Some(vec![Bytes::from(vec![0x01, 0x02])]);

        let json = serde_json::to_string(&attrs).expect("serialize");
        let decoded: MorphPayloadAttributes = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(attrs, decoded);
    }

    #[test]
    fn test_serde_flattened_inner() {
        // The inner PayloadAttributes should be flattened
        let json = r#"{
            "timestamp": "0x499602d2",
            "prevRandao": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "suggestedFeeRecipient": "0x0000000000000000000000000000000000000002"
        }"#;

        let attrs: MorphPayloadAttributes = serde_json::from_str(json).expect("deserialize");
        assert_eq!(attrs.inner.timestamp, 1234567890);
        assert!(attrs.transactions.is_none());
    }

    #[test]
    fn test_serde_with_transactions() {
        let json = r#"{
            "timestamp": "0x499602d2",
            "prevRandao": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "suggestedFeeRecipient": "0x0000000000000000000000000000000000000002",
            "transactions": ["0x0102"]
        }"#;

        let attrs: MorphPayloadAttributes = serde_json::from_str(json).expect("deserialize");
        assert_eq!(attrs.transactions.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_payload_id_different_versions_are_distinct() {
        let parent = B256::random();
        let attrs = create_test_attributes();

        // Every distinct version should produce a different ID
        let ids: Vec<_> = (0..=5)
            .map(|v| payload_id_morph(&parent, &attrs, v))
            .collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "version {i} and {j} should differ");
            }
        }
    }

    #[test]
    fn test_payload_id_different_parents() {
        let attrs = create_test_attributes();

        let id1 = payload_id_morph(&B256::from([0x01; 32]), &attrs, 1);
        let id2 = payload_id_morph(&B256::from([0x02; 32]), &attrs, 1);

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_payload_id_different_timestamps() {
        let parent = B256::random();
        let mut attrs1 = create_test_attributes();
        attrs1.inner.timestamp = 100;
        let mut attrs2 = create_test_attributes();
        attrs2.inner.timestamp = 200;

        let id1 = payload_id_morph(&parent, &attrs1, 1);
        let id2 = payload_id_morph(&parent, &attrs2, 1);

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_payload_id_none_vs_empty_transactions() {
        let parent = B256::random();
        let mut attrs1 = create_test_attributes();
        attrs1.transactions = None;
        let mut attrs2 = create_test_attributes();
        attrs2.transactions = Some(vec![]);

        let id1 = payload_id_morph(&parent, &attrs1, 1);
        let id2 = payload_id_morph(&parent, &attrs2, 1);

        // None vs Some(empty) should produce different IDs because
        // we hash whether the field is Some or None
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_payload_id_with_gas_limit_override() {
        let parent = B256::random();
        let mut attrs1 = create_test_attributes();
        attrs1.gas_limit = None;
        let mut attrs2 = create_test_attributes();
        attrs2.gas_limit = Some(30_000_000);

        let id1 = payload_id_morph(&parent, &attrs1, 1);
        let id2 = payload_id_morph(&parent, &attrs2, 1);

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_payload_id_with_base_fee_override() {
        let parent = B256::random();
        let mut attrs1 = create_test_attributes();
        attrs1.base_fee_per_gas = None;
        let mut attrs2 = create_test_attributes();
        attrs2.base_fee_per_gas = Some(1_000_000_000);

        let id1 = payload_id_morph(&parent, &attrs1, 1);
        let id2 = payload_id_morph(&parent, &attrs2, 1);

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_payload_id_with_withdrawals() {
        let parent = B256::random();
        let mut attrs1 = create_test_attributes();
        attrs1.inner.withdrawals = None;
        let mut attrs2 = create_test_attributes();
        attrs2.inner.withdrawals = Some(vec![]);

        let id1 = payload_id_morph(&parent, &attrs1, 1);
        let id2 = payload_id_morph(&parent, &attrs2, 1);

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_payload_id_with_beacon_root() {
        let parent = B256::random();
        let mut attrs1 = create_test_attributes();
        attrs1.inner.parent_beacon_block_root = None;
        let mut attrs2 = create_test_attributes();
        attrs2.inner.parent_beacon_block_root = Some(B256::from([0x42; 32]));

        let id1 = payload_id_morph(&parent, &attrs1, 1);
        let id2 = payload_id_morph(&parent, &attrs2, 1);

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_payload_attributes_trait_impl() {
        use reth_payload_primitives::PayloadAttributes as _;

        let mut attrs = create_test_attributes();
        attrs.inner.timestamp = 42;
        attrs.inner.withdrawals = Some(vec![]);
        attrs.inner.parent_beacon_block_root = Some(B256::from([0x01; 32]));

        assert_eq!(attrs.timestamp(), 42);
        assert!(attrs.withdrawals().is_some());
        assert_eq!(
            attrs.parent_beacon_block_root(),
            Some(B256::from([0x01; 32]))
        );
    }

    #[test]
    fn test_builder_attributes_has_l1_messages_empty() {
        let attrs = MorphPayloadBuilderAttributes::try_new(B256::ZERO, create_test_attributes(), 1)
            .unwrap();
        assert!(!attrs.has_l1_messages());
    }

    #[test]
    fn test_builder_attributes_accessors() {
        let parent = B256::from([0x42; 32]);
        let mut rpc_attrs = create_test_attributes();
        rpc_attrs.inner.timestamp = 999;
        rpc_attrs.inner.suggested_fee_recipient = Address::from([0x01; 20]);
        rpc_attrs.inner.prev_randao = B256::from([0x02; 32]);
        rpc_attrs.gas_limit = Some(30_000_000);
        rpc_attrs.base_fee_per_gas = Some(1_000_000_000);

        let attrs = MorphPayloadBuilderAttributes::try_new(parent, rpc_attrs, 1).unwrap();

        assert_eq!(attrs.parent(), parent);
        assert_eq!(attrs.timestamp(), 999);
        assert_eq!(attrs.suggested_fee_recipient(), Address::from([0x01; 20]));
        assert_eq!(attrs.prev_randao(), B256::from([0x02; 32]));
        assert!(attrs.parent_beacon_block_root().is_none());
        assert_eq!(attrs.gas_limit, Some(30_000_000));
        assert_eq!(attrs.base_fee_per_gas, Some(1_000_000_000));
    }

    #[test]
    fn test_serde_with_gas_and_base_fee_overrides() {
        let json = r#"{
            "timestamp": "0x499602d2",
            "prevRandao": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "suggestedFeeRecipient": "0x0000000000000000000000000000000000000002",
            "gasLimit": "0x1c9c380",
            "baseFeePerGas": "0x3b9aca00"
        }"#;

        let attrs: MorphPayloadAttributes = serde_json::from_str(json).expect("deserialize");
        assert_eq!(attrs.gas_limit, Some(30_000_000));
        assert_eq!(attrs.base_fee_per_gas, Some(1_000_000_000));
    }

    #[test]
    fn test_serde_optional_fields_absent() {
        let json = r#"{
            "timestamp": "0x1",
            "prevRandao": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "suggestedFeeRecipient": "0x0000000000000000000000000000000000000000"
        }"#;

        let attrs: MorphPayloadAttributes = serde_json::from_str(json).expect("deserialize");
        assert!(attrs.transactions.is_none());
        assert!(attrs.gas_limit.is_none());
        assert!(attrs.base_fee_per_gas.is_none());
    }
}
