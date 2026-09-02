//! Morph payload attributes types.

use alloy_eips::eip2718::Decodable2718;
use alloy_eips::eip4895::{Withdrawal, Withdrawals};
use alloy_primitives::{Address, B256, Bytes};
use alloy_rpc_types_engine::{PayloadAttributes, PayloadId};
use morph_primitives::MorphTxEnvelope;
use reth_primitives_traits::{Recovered, SignerRecoverable, WithEncoded};
use sha2::{Digest, Sha256};

/// Engine API version byte stored in Morph payload IDs.
///
/// Morph's custom payload methods currently use version 1. The complete payload attributes,
/// including the txpool policy, are hashed separately from this protocol version.
pub const MORPH_PAYLOAD_BUILDER_VERSION: u8 = 1;

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

    /// Transactions to execute at the beginning of the block, in the given order.
    ///
    /// The exact meaning depends on [`Self::no_tx_pool`]:
    ///
    /// - `no_tx_pool == false` (sequencer assembly): **only L1 messages** (L1→L2 deposits).
    ///   They are executed first, then the builder appends the best transactions from the
    ///   local pool. This matches go-ethereum's `AssembleL2Block`.
    /// - `no_tx_pool == true` (derivation import): the **complete ordered transaction list**
    ///   of the block, L1 messages first followed by the committed L2 transactions. Nothing
    ///   is appended from the pool. This matches go-ethereum's `NewSafeL2Block`, which
    ///   executes the decoded block via `BlockChain.ProcessBlock` and never touches the miner.
    ///
    /// In both cases any L1 messages present must carry strictly sequential queue indices and
    /// must precede the L2 transactions. L1 messages are never in the mempool and must always
    /// be supplied explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transactions: Option<Vec<Bytes>>,

    /// Disables txpool selection, making block building deterministic.
    ///
    /// When true, the builder executes exactly [`Self::transactions`] and appends nothing from
    /// the local pool. Required by derivation (`engine_newSafeL2Block`): a follower's pool
    /// holds gossiped transactions that the sequencer committed to *later* blocks, so
    /// appending them to an earlier derived block forks the follower off the sequencer chain.
    ///
    /// This is deliberately explicit rather than inferred from `transactions.is_some()`:
    /// sequencer assembly also supplies `transactions` whenever the block has L1 messages,
    /// and inferring the flag there would stop the sequencer from packing the mempool at all.
    #[serde(default)]
    pub no_tx_pool: bool,

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
    fn payload_id(&self, parent_hash: &B256) -> PayloadId {
        self.morph_payload_id(parent_hash)
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.inner.withdrawals.as_ref()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root
    }

    fn slot_number(&self) -> Option<u64> {
        // Morph L2 has no PoS slot semantics.
        None
    }
}

impl MorphPayloadAttributes {
    /// Computes the Morph payload ID without decoding or recovering transaction bytes.
    pub fn morph_payload_id(&self, parent_hash: &B256) -> PayloadId {
        payload_id_morph(parent_hash, self, MORPH_PAYLOAD_BUILDER_VERSION)
    }
}

impl From<PayloadAttributes> for MorphPayloadAttributes {
    fn from(inner: PayloadAttributes) -> Self {
        Self {
            inner,
            transactions: None,
            no_tx_pool: false,
            gas_limit: None,
            base_fee_per_gas: None,
        }
    }
}

/// Internal payload builder attributes.
///
/// This is the internal representation used by the payload builder,
/// with decoded supplied transactions and a computed payload ID.
///
/// Implements `reth_payload_primitives::PayloadAttributes` so it can serve as the
/// `type Attributes` in `PayloadBuilder` (v2.0.0 requires the builder attributes to
/// implement PayloadAttributes). The serde impls are required by the trait bound.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MorphPayloadBuilderAttributes {
    /// Computed payload ID.
    pub id: PayloadId,

    /// Parent block hash.
    pub parent: B256,

    /// Block timestamp.
    pub timestamp: u64,

    /// Suggested fee recipient.
    pub suggested_fee_recipient: Address,

    /// Previous RANDAO value.
    pub prev_randao: B256,

    /// Withdrawals.
    pub withdrawals: Withdrawals,

    /// Parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,

    /// Decoded transactions to execute first, with their original encoded bytes.
    ///
    /// Holds only L1 messages during sequencer assembly, and the complete ordered block
    /// transaction list when [`Self::no_tx_pool`] is set. See
    /// [`MorphPayloadAttributes::transactions`] for the full contract.
    ///
    /// Decoded and recovered during construction to avoid repeated decoding in the
    /// payload builder.
    ///
    /// Skipped for serde: this is purely an internal runtime field derived from
    /// `MorphPayloadAttributes::transactions` during `try_new`. It is never
    /// serialised/deserialised as part of the PayloadAttributes trait contract.
    #[serde(skip)]
    pub transactions: Vec<WithEncoded<Recovered<MorphTxEnvelope>>>,

    /// Disables txpool selection; see [`MorphPayloadAttributes::no_tx_pool`].
    pub no_tx_pool: bool,

    /// Optional gas limit override propagated to EVM env construction.
    pub gas_limit: Option<u64>,

    /// Optional base fee override propagated to EVM env construction.
    pub base_fee_per_gas: Option<u64>,
}

impl MorphPayloadBuilderAttributes {
    /// Build from parent hash + RPC attributes + version byte, decoding supplied transactions.
    pub fn try_new(
        parent: B256,
        attributes: MorphPayloadAttributes,
        version: u8,
    ) -> Result<Self, alloy_rlp::Error> {
        let id = payload_id_morph(&parent, &attributes, version);
        let no_tx_pool = attributes.no_tx_pool;

        // Decode and recover the supplied transactions
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

        Ok(Self {
            id,
            parent,
            timestamp: attributes.inner.timestamp,
            suggested_fee_recipient: attributes.inner.suggested_fee_recipient,
            prev_randao: attributes.inner.prev_randao,
            withdrawals: attributes.inner.withdrawals.unwrap_or_default().into(),
            parent_beacon_block_root: attributes.inner.parent_beacon_block_root,
            transactions,
            no_tx_pool,
            gas_limit: attributes.gas_limit,
            base_fee_per_gas: attributes.base_fee_per_gas,
        })
    }

    /// Returns the payload ID.
    pub fn payload_id(&self) -> PayloadId {
        self.id
    }

    /// Returns the parent block hash.
    pub fn parent(&self) -> B256 {
        self.parent
    }

    /// Returns the block timestamp.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Returns the optional parent beacon block root.
    pub fn parent_beacon_block_root(&self) -> Option<B256> {
        self.parent_beacon_block_root
    }

    /// Returns the suggested fee recipient.
    pub fn suggested_fee_recipient(&self) -> Address {
        self.suggested_fee_recipient
    }

    /// Returns the previous RANDAO value.
    pub fn prev_randao(&self) -> B256 {
        self.prev_randao
    }

    /// Returns the withdrawals.
    pub fn withdrawals(&self) -> &Withdrawals {
        &self.withdrawals
    }

    /// Returns true if there are L1 messages to execute.
    pub fn has_l1_messages(&self) -> bool {
        self.transactions.iter().any(|tx| tx.value().is_l1_msg())
    }

    /// Returns true if the builder may append transactions from the local pool.
    pub fn include_tx_pool(&self) -> bool {
        !self.no_tx_pool
    }
}

/// `payload_id()` ignores the `parent_hash` arg and returns the pre-computed `self.id`
/// (already derived from parent + rpc-attrs during `try_new`).
impl reth_payload_primitives::PayloadAttributes for MorphPayloadBuilderAttributes {
    fn payload_id(&self, _parent_hash: &B256) -> PayloadId {
        self.id
    }

    fn timestamp(&self) -> u64 {
        self.timestamp
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        Some(self.withdrawals.as_ref())
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.parent_beacon_block_root
    }

    fn slot_number(&self) -> Option<u64> {
        // Morph L2 has no PoS slot semantics.
        None
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

    // Hash the txpool policy: an assemble and a derivation import of the same inputs are
    // different payloads (one may append pool transactions), so they must not share an id.
    hasher.update([u8::from(attributes.no_tx_pool)]);

    // Hash whether the transaction list was explicitly supplied.
    hasher.update([u8::from(attributes.transactions.is_some())]);

    // Hash the supplied transactions if present.
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
    use alloy_consensus::{Sealed, Signed, TxLegacy};
    use alloy_primitives::{Signature, TxKind, U256};
    use morph_primitives::transaction::TxL1Msg;

    fn create_test_attributes() -> MorphPayloadAttributes {
        MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp: 1234567890,
                prev_randao: B256::random(),
                suggested_fee_recipient: Address::random(),
                withdrawals: None,
                parent_beacon_block_root: None,
                // Morph L2 has no PoS slot semantics; field added in alloy 2.0.
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: None,
            no_tx_pool: false,
            gas_limit: None,
            base_fee_per_gas: None,
        }
    }

    #[test]
    fn test_default_attributes() {
        let attrs = MorphPayloadAttributes::default();
        assert!(attrs.transactions.is_none());
        // Sequencer assembly is the default; only derivation opts out of the pool.
        assert!(!attrs.no_tx_pool);
    }

    #[test]
    fn test_payload_id_distinguishes_tx_pool_policy() {
        // An assemble and a derivation import of identical inputs are different payloads:
        // one may append pool transactions. They must not collide on the same payload id.
        let parent = B256::random();
        let mut with_pool = create_test_attributes();
        with_pool.transactions = Some(vec![Bytes::from(vec![0x01])]);
        let mut without_pool = with_pool.clone();
        without_pool.no_tx_pool = true;

        assert_ne!(
            payload_id_morph(&parent, &with_pool, 1),
            payload_id_morph(&parent, &without_pool, 1),
        );
    }

    #[test]
    fn test_no_tx_pool_defaults_false_when_absent_from_json() {
        // Existing sequencer callers omit the field entirely and must keep packing the pool.
        let json = r#"{
            "timestamp": "0x499602d2",
            "prevRandao": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "suggestedFeeRecipient": "0x0000000000000000000000000000000000000002"
        }"#;

        let attrs: MorphPayloadAttributes = serde_json::from_str(json).expect("deserialize");
        assert!(!attrs.no_tx_pool);
    }

    #[test]
    fn test_no_tx_pool_deserializes_from_camel_case() {
        let json = r#"{
            "timestamp": "0x499602d2",
            "prevRandao": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "suggestedFeeRecipient": "0x0000000000000000000000000000000000000002",
            "noTxPool": true
        }"#;

        let attrs: MorphPayloadAttributes = serde_json::from_str(json).expect("deserialize");
        assert!(attrs.no_tx_pool);
    }

    #[test]
    fn test_builder_attributes_carry_tx_pool_policy() {
        let parent = B256::random();

        let mut attrs = create_test_attributes();
        attrs.no_tx_pool = true;
        let built = MorphPayloadBuilderAttributes::try_new(parent, attrs, 1).expect("try_new");
        assert!(built.no_tx_pool);
        assert!(!built.include_tx_pool());

        let built = MorphPayloadBuilderAttributes::try_new(parent, create_test_attributes(), 1)
            .expect("try_new");
        assert!(!built.no_tx_pool);
        assert!(built.include_tx_pool());
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
    fn test_morph_payload_id_does_not_decode_transactions() {
        let parent = B256::from([0x01; 32]);
        let mut attrs = create_test_attributes();
        attrs.transactions = Some(vec![Bytes::from_static(b"not a valid encoded transaction")]);

        let id = attrs.morph_payload_id(&parent);

        assert_eq!(
            id,
            payload_id_morph(&parent, &attrs, MORPH_PAYLOAD_BUILDER_VERSION)
        );
    }

    #[test]
    fn test_builder_attributes_detect_only_l1_messages() {
        let mut attrs = MorphPayloadBuilderAttributes::try_new(
            B256::ZERO,
            create_test_attributes(),
            MORPH_PAYLOAD_BUILDER_VERSION,
        )
        .unwrap();
        assert!(!attrs.has_l1_messages());

        let l2_tx = MorphTxEnvelope::Legacy(Signed::new_unhashed(
            TxLegacy {
                chain_id: Some(1),
                nonce: 0,
                gas_price: 1,
                gas_limit: 21_000,
                to: TxKind::Call(Address::ZERO),
                value: U256::ZERO,
                input: Bytes::new(),
            },
            Signature::test_signature(),
        ));
        attrs.transactions.push(WithEncoded::new(
            Bytes::new(),
            Recovered::new_unchecked(l2_tx, Address::ZERO),
        ));
        assert!(
            !attrs.has_l1_messages(),
            "a non-empty L2-only list must not be reported as L1 messages"
        );

        let l1_msg = MorphTxEnvelope::L1Msg(Sealed::new(TxL1Msg {
            queue_index: 0,
            gas_limit: 21_000,
            to: Address::ZERO,
            value: U256::ZERO,
            sender: Address::ZERO,
            input: Bytes::new(),
        }));
        attrs.transactions.push(WithEncoded::new(
            Bytes::new(),
            Recovered::new_unchecked(l1_msg, Address::ZERO),
        ));
        assert!(attrs.has_l1_messages());
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

        let attrs = MorphPayloadBuilderAttributes::try_new(
            parent,
            rpc_attrs,
            MORPH_PAYLOAD_BUILDER_VERSION,
        )
        .unwrap();

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
