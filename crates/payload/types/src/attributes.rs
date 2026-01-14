//! Morph payload attributes types.

use alloy_eips::eip4895::{Withdrawal, Withdrawals};
use alloy_primitives::{Address, B256, Bytes};
use alloy_rpc_types_engine::PayloadAttributes;
use reth_payload_primitives::PayloadBuilderAttributes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Morph-specific payload attributes for Engine API.
///
/// This extends the standard Ethereum [`PayloadAttributes`] with L2-specific fields
/// for forced transaction inclusion (L1 messages).
///
/// # Compatibility
///
/// This type is designed to be compatible with both:
/// - Standard Ethereum Engine API (via the inner [`PayloadAttributes`])
/// - Morph L2 requirements (via the `transactions` field for L1 messages)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphPayloadAttributes {
    /// Standard Ethereum payload attributes.
    #[serde(flatten)]
    pub inner: PayloadAttributes,

    /// Forced transactions to include at the beginning of the block.
    ///
    /// This includes L1 messages that must be processed in order.
    /// These transactions are not in the mempool and must be explicitly provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transactions: Option<Vec<Bytes>>,
}

impl MorphPayloadAttributes {
    /// Create new [`MorphPayloadAttributes`] from standard attributes.
    pub fn new(inner: PayloadAttributes) -> Self {
        Self {
            inner,
            transactions: None,
        }
    }

    /// Returns the timestamp for the payload.
    pub fn timestamp(&self) -> u64 {
        self.inner.timestamp
    }

    /// Returns the suggested fee recipient (coinbase).
    pub fn suggested_fee_recipient(&self) -> Address {
        self.inner.suggested_fee_recipient
    }

    /// Returns the prev_randao value.
    pub fn prev_randao(&self) -> B256 {
        self.inner.prev_randao
    }

    /// Returns the parent beacon block root if set.
    pub fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root
    }

    /// Returns the withdrawals if set.
    pub fn withdrawals(&self) -> Option<&Vec<alloy_eips::eip4895::Withdrawal>> {
        self.inner.withdrawals.as_ref()
    }

    /// Returns the forced transactions (L1 messages) if any.
    pub fn forced_transactions(&self) -> &[Bytes] {
        self.transactions.as_deref().unwrap_or(&[])
    }

    /// Returns true if there are forced transactions.
    pub fn has_forced_transactions(&self) -> bool {
        self.transactions.as_ref().is_some_and(|t| !t.is_empty())
    }

    /// Builder method to set forced transactions.
    pub fn with_transactions(mut self, transactions: Vec<Bytes>) -> Self {
        self.transactions = Some(transactions);
        self
    }
}

impl From<PayloadAttributes> for MorphPayloadAttributes {
    fn from(inner: PayloadAttributes) -> Self {
        Self::new(inner)
    }
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
/// with decoded transactions and computed payload ID.
#[derive(Debug, Clone)]
pub struct MorphPayloadBuilderAttributes {
    /// Payload ID.
    pub id: alloy_rpc_types_engine::PayloadId,

    /// Parent block hash.
    pub parent: B256,

    /// Block timestamp.
    pub timestamp: u64,

    /// Suggested fee recipient (coinbase).
    pub suggested_fee_recipient: Address,

    /// Previous RANDAO value.
    pub prev_randao: B256,

    /// Withdrawals (usually empty for L2).
    pub withdrawals: Withdrawals,

    /// Parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,

    /// Forced transactions (L1 messages).
    pub transactions: Vec<Bytes>,
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

        Ok(Self {
            id,
            parent,
            timestamp: attributes.inner.timestamp,
            suggested_fee_recipient: attributes.inner.suggested_fee_recipient,
            prev_randao: attributes.inner.prev_randao,
            withdrawals: attributes.inner.withdrawals.unwrap_or_default().into(),
            parent_beacon_block_root: attributes.inner.parent_beacon_block_root,
            transactions: attributes.transactions.unwrap_or_default(),
        })
    }

    fn payload_id(&self) -> alloy_rpc_types_engine::PayloadId {
        self.id
    }

    fn parent(&self) -> B256 {
        self.parent
    }

    fn timestamp(&self) -> u64 {
        self.timestamp
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.parent_beacon_block_root
    }

    fn suggested_fee_recipient(&self) -> Address {
        self.suggested_fee_recipient
    }

    fn prev_randao(&self) -> B256 {
        self.prev_randao
    }

    fn withdrawals(&self) -> &Withdrawals {
        &self.withdrawals
    }
}

impl MorphPayloadBuilderAttributes {
    /// Returns true if there are forced transactions.
    pub fn has_forced_transactions(&self) -> bool {
        !self.transactions.is_empty()
    }
}

/// Compute payload ID from parent hash and attributes.
///
/// Uses SHA-256 hashing with the version byte as the first byte of the result.
fn payload_id_morph(
    parent: &B256,
    attributes: &MorphPayloadAttributes,
    version: u8,
) -> alloy_rpc_types_engine::PayloadId {
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

    // Hash forced transactions if present
    if let Some(txs) = attributes.transactions.as_ref().filter(|t| !t.is_empty()) {
        hasher.update(&txs.len().to_be_bytes()[..]);
        for tx in txs {
            let tx_hash = alloy_primitives::keccak256(tx);
            hasher.update(tx_hash.as_slice());
        }
    }

    // Finalize and create payload ID
    let mut result = hasher.finalize();
    result[0] = version;

    alloy_rpc_types_engine::PayloadId::new(
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
        }
    }

    #[test]
    fn test_new_from_payload_attributes() {
        let inner = PayloadAttributes {
            timestamp: 1234567890,
            prev_randao: B256::random(),
            suggested_fee_recipient: Address::random(),
            withdrawals: None,
            parent_beacon_block_root: None,
        };

        let attrs = MorphPayloadAttributes::new(inner.clone());
        assert_eq!(attrs.timestamp(), inner.timestamp);
        assert_eq!(
            attrs.suggested_fee_recipient(),
            inner.suggested_fee_recipient
        );
        assert!(!attrs.has_forced_transactions());
    }

    #[test]
    fn test_with_transactions() {
        let attrs = create_test_attributes().with_transactions(vec![Bytes::from(vec![0x01])]);

        assert_eq!(attrs.forced_transactions().len(), 1);
        assert!(attrs.has_forced_transactions());
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
        let attrs2 = create_test_attributes().with_transactions(vec![Bytes::from(vec![0x01])]);

        let id1 = payload_id_morph(&parent, &attrs1, 1);
        let id2 = payload_id_morph(&parent, &attrs2, 1);

        // Different transactions should produce different IDs
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_payload_builder_attributes_try_new() {
        let parent = B256::random();
        let attrs = create_test_attributes();

        let builder_attrs = MorphPayloadBuilderAttributes::try_new(parent, attrs.clone(), 1)
            .expect("should succeed");

        assert_eq!(builder_attrs.parent(), parent);
        assert_eq!(builder_attrs.timestamp(), attrs.timestamp());
    }

    #[test]
    fn test_serde_roundtrip() {
        let attrs = create_test_attributes().with_transactions(vec![Bytes::from(vec![0x01, 0x02])]);

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
        assert_eq!(attrs.timestamp(), 1234567890);
        assert!(!attrs.has_forced_transactions());
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
        assert_eq!(attrs.forced_transactions().len(), 1);
    }
}
