//! Morph transaction environment.
//!
//! This module defines the flat, execution-facing view of a Morph transaction: one shape for
//! every transaction type, with token-fee fields where they apply.

use alloy_consensus::{
    SignableTransaction, Transaction as AlloyTransaction, transaction::Recovered,
};
use alloy_eips::{
    Typed2718,
    eip2718::{Encodable2718, WithEncoded},
    eip2930::AccessList,
    eip7702::{RecoveredAuthorization, SignedAuthorization},
};
use alloy_primitives::{B256, Bytes, ChainId, Signature, TxKind, U256};
use evm2::ethereum::LazyAuthorization;
use morph_primitives::{
    L1_TX_TYPE_ID, MORPH_TX_TYPE_ID, MorphTxEnvelope, TxMorph,
    transaction::morph_transaction::{MORPH_TX_VERSION_1, MORPH_TX_VERSION_2},
};
use reth_evm::{FromRecoveredTx, FromTxWithEncoded, TransactionEnvMut};

/// EIP-2718 type byte of a legacy transaction.
const LEGACY_TX_TYPE: u8 = 0x00;
/// EIP-2718 type byte of an EIP-2930 transaction.
const EIP2930_TX_TYPE: u8 = 0x01;

/// Morph transaction environment with token fee support.
///
/// The fields follow the semantics of the consensus transaction they were built from:
/// - [`Self::tx_type`] selects the handler (0x7E L1 message, 0x7F MorphTx, or a standard type)
/// - [`Self::rlp_bytes`] holds the encoded transaction for the L1 data fee calculation
/// - [`Self::version`], [`Self::fee_token_id`], [`Self::fee_limit`], [`Self::reference`] and
///   [`Self::memo`] carry the extended MorphTx fields
///
/// The sender is not part of this type: execution receives it as
/// `Recovered<MorphTxEnv>`, alongside the signature-recovery result.
///
/// Reference: <https://github.com/morph-l2/go-ethereum/pull/282>
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MorphTxEnv {
    /// EIP-2718 transaction type byte.
    pub tx_type: u8,
    /// Gas limit.
    pub gas_limit: u64,
    /// Legacy gas price, or the maximum fee per gas for dynamic-fee transactions.
    pub gas_price: u128,
    /// Maximum priority fee per gas, when the transaction type has one.
    pub gas_priority_fee: Option<u128>,
    /// Call target or contract creation.
    pub kind: TxKind,
    /// Transferred value.
    pub value: U256,
    /// Call data or initcode.
    pub data: Bytes,
    /// Sender nonce.
    pub nonce: u64,
    /// Chain id, when the transaction commits to one.
    pub chain_id: Option<ChainId>,
    /// EIP-2930 access list.
    pub access_list: AccessList,
    /// EIP-7702 authorizations, with their authorities recovered where possible.
    pub authorization_list: Vec<LazyAuthorization>,
    /// RLP encoded transaction bytes.
    /// Used only for L1 data fee calculation.
    pub rlp_bytes: Option<Bytes>,
    /// Version of the Morph transaction format.
    pub version: Option<u8>,
    /// Token ID for fee payment (only for TxMorph type 0x7F).
    /// 0 means ETH payment, > 0 means ERC20 token payment.
    pub fee_token_id: Option<u16>,
    /// Maximum amount of tokens the sender is willing to pay as fee.
    pub fee_limit: Option<U256>,
    /// Reference key for the transaction.
    /// Used for indexing and looking up transactions by external systems.
    pub reference: Option<B256>,
    /// Memo field for arbitrary data.
    pub memo: Option<Bytes>,
}

impl MorphTxEnv {
    /// Create a new Morph transaction environment with RLP bytes.
    pub fn with_rlp_bytes(mut self, rlp_bytes: Bytes) -> Self {
        self.rlp_bytes = Some(rlp_bytes);
        self
    }

    /// Set the version.
    pub fn with_version(mut self, version: u8) -> Self {
        self.version = Some(version);
        self
    }

    /// Set the fee token ID.
    pub fn with_fee_token_id(mut self, fee_token_id: u16) -> Self {
        self.fee_token_id = Some(fee_token_id);
        self
    }

    /// Set the fee limit.
    pub fn with_fee_limit(mut self, fee_limit: U256) -> Self {
        self.fee_limit = Some(fee_limit);
        self
    }

    /// Set the reference.
    pub fn with_reference(mut self, reference: B256) -> Self {
        self.reference = Some(reference);
        self
    }

    /// Set the memo.
    pub fn with_memo(mut self, memo: Bytes) -> Self {
        self.memo = Some(memo);
        self
    }

    /// Effective gas price paid per unit of gas at `base_fee`.
    ///
    /// Legacy and EIP-2930 transactions pay their gas price; the rest pay
    /// `min(max_fee, base_fee + priority_fee)`.
    #[inline]
    pub fn effective_gas_price_at(&self, base_fee: u128) -> u128 {
        if matches!(self.tx_type, LEGACY_TX_TYPE | EIP2930_TX_TYPE) {
            return self.gas_price;
        }
        match self.gas_priority_fee {
            Some(tip) => self.gas_price.min(base_fee.saturating_add(tip)),
            None => self.gas_price,
        }
    }

    /// Encodes this tx env into EIP-2718 bytes for L1 fee accounting.
    ///
    /// Used by simulation paths (`eth_call`, `eth_estimateGas`) that have env
    /// fields but no signed transaction. The envelope is **not** reconstructed
    /// from `tx_type`: alloy's `minimal_tx_type()` would pick Legacy / EIP-2930
    /// for the common `gasPrice` request shapes, which under-sizes the fee
    /// versus go-ethereum. Match `asUnsignedTx` (post-London): MorphTx,
    /// EIP-7702 when the authorization list is executable, otherwise EIP-1559.
    pub fn encode_for_l1_fee(&self, fallback_chain_id: u64) -> Bytes {
        // Signature validity is irrelevant for fee sizing, but encoded zero/non-zero
        // bytes matter for the pre-Curie calldata-gas formula. Match
        // go-ethereum's EstimateL1DataFeeForMessage placeholder:
        // 64 bytes of 0xff followed by yParity=1.
        let placeholder_signature = Signature::new(U256::MAX, U256::MAX, true);

        match self.build_morph_tx_for_l1_fee(fallback_chain_id, placeholder_signature) {
            Some(morph_tx) => {
                let signed = morph_tx.into_signed(placeholder_signature);
                MorphTxEnvelope::Morph(signed).rlp()
            }
            None => self
                .build_ethereum_envelope_for_l1_fee(fallback_chain_id, placeholder_signature)
                .rlp(),
        }
    }

    fn build_morph_tx_for_l1_fee(
        &self,
        fallback_chain_id: u64,
        placeholder_signature: Signature,
    ) -> Option<TxMorph> {
        if !self.is_morph_tx() {
            return None;
        }

        let version = self.version.unwrap_or_else(|| {
            // If version is missing (e.g. from an older transaction type), fall back to
            // the smallest version that can represent the request so the L1 fee is not
            // underpriced: V2 when an authorization list is present, otherwise V1.
            let fallback = if self.authorization_list.is_empty() {
                MORPH_TX_VERSION_1
            } else {
                MORPH_TX_VERSION_2
            };
            tracing::debug!(
                target: "morph::evm",
                fallback,
                "MorphTx version not set, falling back for L1 fee calculation to safely overestimate"
            );
            fallback
        });

        // V2 carries the authorization list in its RLP payload (an empty list
        // still encodes as `0xc0`); leaving it out would under-size the L1 data
        // fee by the whole list (geth's `asUnsignedMorphTx` sizes it too).
        // Recovered authorizations no longer carry their signature and reuse
        // the fee-sizing placeholder.
        let authorization_list = if version == MORPH_TX_VERSION_2 {
            self.signed_authorizations_for_l1_fee(placeholder_signature)
        } else {
            Vec::new()
        };

        Some(TxMorph {
            chain_id: self.chain_id.unwrap_or(fallback_chain_id),
            nonce: self.nonce,
            gas_limit: self.gas_limit,
            max_fee_per_gas: self.gas_price,
            max_priority_fee_per_gas: self.gas_priority_fee.unwrap_or_default(),
            to: self.kind,
            value: self.value,
            access_list: self.access_list.clone(),
            input: self.data.clone(),
            fee_token_id: self.fee_token_id.unwrap_or_default(),
            fee_limit: self.fee_limit.unwrap_or_default(),
            version,
            reference: self.reference,
            memo: self.memo.clone(),
            authorization_list,
        })
    }

    /// Rebuilds the envelope go-ethereum uses to size the simulated L1 fee.
    ///
    /// Post-London `asUnsignedTx` never emits Legacy or EIP-2930: those types
    /// only exist when `baseFee == nil`, which Morph never has. Dispatching on
    /// `tx_type` here was the #189 bug — RPC conversion sets that byte from
    /// `minimal_tx_type()`, so a `gasPrice`-only `eth_estimateGas` was sized
    /// as Legacy while geth always used DynamicFeeTx.
    fn build_ethereum_envelope_for_l1_fee(
        &self,
        fallback_chain_id: u64,
        signature: Signature,
    ) -> MorphTxEnvelope {
        debug_assert!(
            !self.is_l1_msg(),
            "L1 messages should not use fallback L1 fee calculation"
        );

        let chain_id = self.chain_id.unwrap_or(fallback_chain_id);
        // geth maps a legacy `gasPrice` onto both EIP-1559 caps (`ToMessage`).
        let max_priority_fee_per_gas = self.gas_priority_fee.unwrap_or(self.gas_price);

        // SetCodeTx cannot be a create (`ErrSetCodeTxCreate`); geth falls
        // through to DynamicFeeTx when `to` is missing or the auth list is empty.
        if !self.authorization_list.is_empty()
            && let TxKind::Call(to) = self.kind
        {
            return MorphTxEnvelope::Eip7702(
                alloy_consensus::TxEip7702 {
                    chain_id,
                    nonce: self.nonce,
                    gas_limit: self.gas_limit,
                    max_fee_per_gas: self.gas_price,
                    max_priority_fee_per_gas,
                    to,
                    value: self.value,
                    access_list: self.access_list.clone(),
                    authorization_list: self.signed_authorizations_for_l1_fee(signature),
                    input: self.data.clone(),
                }
                .into_signed(signature),
            );
        }

        MorphTxEnvelope::Eip1559(
            alloy_consensus::TxEip1559 {
                chain_id,
                nonce: self.nonce,
                gas_limit: self.gas_limit,
                max_fee_per_gas: self.gas_price,
                max_priority_fee_per_gas,
                to: self.kind,
                value: self.value,
                access_list: self.access_list.clone(),
                input: self.data.clone(),
            }
            .into_signed(signature),
        )
    }

    fn signed_authorizations_for_l1_fee(&self, placeholder: Signature) -> Vec<SignedAuthorization> {
        self.authorization_list
            .iter()
            .map(|auth| match auth {
                LazyAuthorization::Signed(signed) => signed.clone(),
                // Recovered authorizations no longer carry their original
                // signature, so reuse the fee-sizing placeholder.
                LazyAuthorization::Recovered(recovered) => {
                    recovered.clone().into_parts().0.into_signed(placeholder)
                }
            })
            .collect()
    }

    /// Builds the environment for `tx`, encoding it for the L1 data fee calculation.
    pub fn from_envelope(tx: &MorphTxEnvelope) -> Self {
        Self::from_envelope_with_rlp(tx, Bytes::from(tx.encoded_2718()))
    }

    /// Builds the environment for `tx` from already encoded EIP-2718 bytes.
    ///
    /// This is the core implementation used by both the `FromRecoveredTx` and
    /// `FromTxWithEncoded` conversions.
    fn from_envelope_with_rlp(tx: &MorphTxEnvelope, rlp_bytes: Bytes) -> Self {
        let mut env = Self {
            tx_type: tx.tx_type().into(),
            gas_limit: AlloyTransaction::gas_limit(tx),
            gas_price: tx.effective_gas_price(None),
            gas_priority_fee: AlloyTransaction::max_priority_fee_per_gas(tx),
            kind: AlloyTransaction::kind(tx),
            value: AlloyTransaction::value(tx),
            data: AlloyTransaction::input(tx).clone(),
            nonce: AlloyTransaction::nonce(tx),
            chain_id: AlloyTransaction::chain_id(tx),
            access_list: tx.access_list().cloned().unwrap_or_default(),
            authorization_list: tx
                .authorization_list()
                .unwrap_or_default()
                .iter()
                .map(|auth| {
                    LazyAuthorization::Recovered(RecoveredAuthorization::new_unchecked(
                        auth.inner().clone(),
                        auth.recover_authority().map_or(
                            alloy_eips::eip7702::RecoveredAuthority::Invalid,
                            alloy_eips::eip7702::RecoveredAuthority::Valid,
                        ),
                    ))
                })
                .collect(),
            rlp_bytes: Some(rlp_bytes),
            ..Default::default()
        };

        if let MorphTxEnvelope::Morph(signed) = tx {
            let morph = signed.tx();
            env.version = Some(morph.version);
            env.fee_token_id = Some(morph.fee_token_id);
            env.fee_limit = Some(morph.fee_limit);
            env.reference = morph.reference;
            env.memo = morph.memo.clone().filter(|memo| !memo.is_empty());
        }
        env
    }
}

impl Typed2718 for MorphTxEnv {
    #[inline]
    fn ty(&self) -> u8 {
        self.tx_type
    }
}

impl AlloyTransaction for MorphTxEnv {
    #[inline]
    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id
    }

    #[inline]
    fn nonce(&self) -> u64 {
        self.nonce
    }

    #[inline]
    fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    #[inline]
    fn gas_price(&self) -> Option<u128> {
        (!self.is_dynamic_fee()).then_some(self.gas_price)
    }

    #[inline]
    fn max_fee_per_gas(&self) -> u128 {
        self.gas_price
    }

    #[inline]
    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.gas_priority_fee
    }

    #[inline]
    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        None
    }

    #[inline]
    fn priority_fee_or_price(&self) -> u128 {
        self.gas_priority_fee.unwrap_or(self.gas_price)
    }

    #[inline]
    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        match base_fee {
            Some(base_fee) => self.effective_gas_price_at(base_fee as u128),
            None => self.gas_price,
        }
    }

    #[inline]
    fn is_dynamic_fee(&self) -> bool {
        !matches!(self.tx_type, LEGACY_TX_TYPE | EIP2930_TX_TYPE)
    }

    #[inline]
    fn kind(&self) -> TxKind {
        self.kind
    }

    #[inline]
    fn is_create(&self) -> bool {
        self.kind.is_create()
    }

    #[inline]
    fn value(&self) -> U256 {
        self.value
    }

    #[inline]
    fn input(&self) -> &Bytes {
        &self.data
    }

    #[inline]
    fn access_list(&self) -> Option<&AccessList> {
        Some(&self.access_list)
    }

    #[inline]
    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        None
    }

    /// Always `None`: the environment stores authorizations in their execution form
    /// ([`MorphTxEnv::authorization_list`]), which keeps recovered authorities rather than the
    /// signed tuples this accessor would have to return.
    #[inline]
    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        None
    }
}

impl FromRecoveredTx<MorphTxEnvelope> for MorphTxEnv {
    fn from_recovered_tx(tx: Recovered<MorphTxEnvelope>) -> Self {
        Self::from_envelope(tx.inner())
    }
}

impl FromTxWithEncoded<MorphTxEnvelope> for MorphTxEnv {
    fn from_tx_with_encoded(tx: WithEncoded<Recovered<MorphTxEnvelope>>) -> Self {
        let (encoded, recovered) = tx.split();
        Self::from_envelope_with_rlp(recovered.inner(), encoded)
    }
}

impl TransactionEnvMut for MorphTxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.gas_limit = gas_limit;
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        // An access list on a legacy request makes it EIP-2930. L1 fee encoding does not
        // read that type byte; the access list still lands on the EIP-1559 envelope used
        // for simulation fee sizing.
        self.access_list = access_list;
        if self.tx_type == LEGACY_TX_TYPE {
            self.tx_type = EIP2930_TX_TYPE;
        }
    }
}

/// Extension trait for transaction types to support Morph-specific functionality.
pub trait MorphTxExt {
    /// Returns whether this transaction is an L1 message transaction (type 0x7E).
    fn is_l1_msg(&self) -> bool;

    /// Returns whether this transaction is a TxMorph (type 0x7F).
    /// TxMorph supports ERC20 token-based gas payment.
    fn is_morph_tx(&self) -> bool;

    /// Returns whether this transaction uses token-based gas payment.
    fn uses_token_fee(&self) -> bool {
        false
    }

    /// Returns whether this transaction has a reference.
    fn has_reference(&self) -> bool {
        false
    }
}

impl MorphTxExt for MorphTxEnv {
    #[inline]
    fn is_l1_msg(&self) -> bool {
        self.tx_type == L1_TX_TYPE_ID
    }

    #[inline]
    fn is_morph_tx(&self) -> bool {
        self.tx_type == MORPH_TX_TYPE_ID
    }

    #[inline]
    fn uses_token_fee(&self) -> bool {
        self.fee_token_id.is_some_and(|id| id > 0)
    }

    #[inline]
    fn has_reference(&self) -> bool {
        self.reference.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::{
        eip2718::Decodable2718,
        eip7702::{Authorization, RecoveredAuthority},
    };
    use alloy_primitives::Address;

    const EIP1559_TX_TYPE: u8 = 0x02;
    const EIP7702_TX_TYPE: u8 = 0x04;

    #[test]
    fn test_l1_msg_detection() {
        let tx = MorphTxEnv {
            tx_type: L1_TX_TYPE_ID,
            ..Default::default()
        };
        assert!(tx.is_l1_msg());
        assert!(!tx.is_morph_tx());

        let regular_tx = MorphTxEnv::default();
        assert!(!regular_tx.is_l1_msg());
    }

    #[test]
    fn test_morph_tx_detection() {
        let tx = MorphTxEnv {
            tx_type: MORPH_TX_TYPE_ID,
            ..Default::default()
        };
        assert!(tx.is_morph_tx());
        assert!(!tx.is_l1_msg());
    }

    #[test]
    fn typed_2718_reports_the_type_byte() {
        let tx = MorphTxEnv {
            tx_type: MORPH_TX_TYPE_ID,
            ..Default::default()
        };
        assert_eq!(tx.ty(), MORPH_TX_TYPE_ID);
    }

    #[test]
    fn effective_gas_price_uses_the_transaction_fee_model() {
        let legacy = MorphTxEnv {
            gas_price: 10,
            ..Default::default()
        };
        assert_eq!(legacy.effective_gas_price_at(1), 10);

        let dynamic = MorphTxEnv {
            tx_type: EIP1559_TX_TYPE,
            gas_price: 10,
            gas_priority_fee: Some(2),
            ..Default::default()
        };
        assert_eq!(dynamic.effective_gas_price_at(1), 3);
        assert_eq!(
            dynamic.effective_gas_price_at(100),
            10,
            "capped by the max fee"
        );
    }

    #[test]
    fn test_rlp_bytes() {
        let tx = MorphTxEnv::default();
        assert!(tx.rlp_bytes.is_none());

        let tx_with_rlp = MorphTxEnv::default().with_rlp_bytes(Bytes::from(vec![1, 2, 3]));
        assert_eq!(tx_with_rlp.rlp_bytes, Some(Bytes::from(vec![1, 2, 3])));
    }

    #[test]
    fn test_encode_for_l1_fee_legacy() {
        let tx = MorphTxEnv {
            chain_id: Some(53077),
            gas_limit: 21_000,
            gas_price: 1,
            nonce: 1,
            kind: TxKind::Call(Address::ZERO),
            ..Default::default()
        };
        let encoded = tx.encode_for_l1_fee(53077);
        assert_eq!(
            encoded.first(),
            Some(&0x02),
            "post-London simulation L1 fee encoding is always EIP-1559"
        );
    }

    #[test]
    fn encode_for_l1_fee_uses_go_ethereum_placeholder_signature_shape() {
        let tx = MorphTxEnv {
            chain_id: Some(53077),
            gas_limit: 21_000,
            gas_price: 1,
            nonce: 1,
            kind: TxKind::Call(Address::ZERO),
            ..Default::default()
        };
        let encoded = tx.encode_for_l1_fee(53077);
        let full_ff_words = encoded
            .windows(32)
            .filter(|window| window.iter().all(|byte| *byte == 0xff))
            .count();

        assert_eq!(
            full_ff_words, 2,
            "fallback L1 fee encoding must use go-ethereum's 0xff...ff placeholder r/s words"
        );
    }

    #[test]
    fn encode_for_l1_fee_dynamic_matches_go_ethereum_bytes() {
        let tx = MorphTxEnv {
            tx_type: EIP1559_TX_TYPE,
            chain_id: Some(1),
            gas_limit: 8_000_000,
            gas_price: 16,
            gas_priority_fee: Some(16),
            nonce: 0,
            kind: TxKind::Call(Address::with_last_byte(0xf1)),
            data: Bytes::from_static(&[
                0x22, 0xcf, 0xae, 0xfc, 0x92, 0xe4, 0xed, 0xb9, 0xb0, 0xae, 0x01, 0xa6, 0x3a, 0x95,
                0xdf, 0x11, 0xf1, 0x27, 0x9b, 0x7b, 0x6b, 0xdd, 0xe4, 0xe0, 0x48, 0xf6, 0xec, 0xe8,
                0xf8, 0xc8, 0xf2, 0xc2, 0xde, 0x97, 0x54, 0x4a, 0x16, 0x3f, 0x7d, 0x30, 0x23, 0x15,
                0xee, 0x0f, 0x78, 0x69, 0xfb, 0xdc, 0xfd, 0x86,
            ]),
            ..Default::default()
        };

        assert_eq!(
            alloy_primitives::hex::encode_prefixed(tx.encode_for_l1_fee(1)),
            "0x02f89501801010837a12009400000000000000000000000000000000000000f180b222cfaefc92e4edb9b0ae01a63a95df11f1279b7b6bdde4e048f6ece8f8c8f2c2de97544a163f7d302315ee0f7869fbdcfd86c001a0ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffa0ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        );
    }

    #[test]
    fn encode_for_l1_fee_sizes_eip2930_request_as_eip1559_like_geth() {
        let tx = MorphTxEnv {
            tx_type: EIP2930_TX_TYPE,
            chain_id: Some(53077),
            gas_limit: 21_000,
            gas_price: 1,
            nonce: 1,
            kind: TxKind::Call(Address::ZERO),
            ..Default::default()
        };

        let encoded = tx.encode_for_l1_fee(53077);
        assert_eq!(encoded.first(), Some(&0x02));

        let decoded = MorphTxEnvelope::decode_2718(&mut encoded.as_ref()).unwrap();
        let MorphTxEnvelope::Eip1559(decoded) = decoded else {
            panic!("geth sizes access-list simulation as DynamicFeeTx, not EIP-2930");
        };
        assert!(decoded.tx().access_list.is_empty());
        assert_eq!(decoded.tx().max_priority_fee_per_gas, 1);
        assert_eq!(decoded.tx().max_fee_per_gas, 1);
    }

    #[test]
    fn encode_for_l1_fee_maps_legacy_gas_price_onto_both_eip1559_caps() {
        let cases: [(&str, MorphTxEnv, usize); 3] = [
            (
                "no fee fields",
                MorphTxEnv {
                    chain_id: Some(2818),
                    gas_limit: 21_000,
                    kind: TxKind::Call(Address::ZERO),
                    ..Default::default()
                },
                103,
            ),
            (
                "gasPrice only",
                MorphTxEnv {
                    chain_id: Some(2818),
                    gas_limit: 21_000,
                    gas_price: 1_000_000_000,
                    kind: TxKind::Call(Address::ZERO),
                    ..Default::default()
                },
                111,
            ),
            (
                "gasPrice + empty access list",
                MorphTxEnv {
                    tx_type: EIP2930_TX_TYPE,
                    chain_id: Some(2818),
                    gas_limit: 21_000,
                    gas_price: 1_000_000_000,
                    kind: TxKind::Call(Address::ZERO),
                    ..Default::default()
                },
                111,
            ),
        ];

        for (name, tx, geth_len) in cases {
            let encoded = tx.encode_for_l1_fee(2818);
            assert_eq!(
                encoded.first().copied(),
                Some(0x02),
                "{name}: EIP-1559 prefix"
            );
            assert_eq!(
                encoded.len(),
                geth_len,
                "{name}: geth DynamicFee byte length"
            );
        }
    }

    #[test]
    fn encode_for_l1_fee_empty_authorization_list_stays_eip1559() {
        let tx = MorphTxEnv {
            tx_type: EIP7702_TX_TYPE,
            chain_id: Some(53077),
            gas_limit: 100_000,
            gas_price: 20_000_000_000,
            gas_priority_fee: Some(1_000_000_000),
            nonce: 1,
            kind: TxKind::Call(Address::with_last_byte(0xf1)),
            authorization_list: vec![],
            ..Default::default()
        };

        let encoded = tx.encode_for_l1_fee(53077);
        assert_eq!(
            encoded.first(),
            Some(&0x02),
            "empty auth list cannot execute as SetCodeTx; geth sizes it as DynamicFeeTx"
        );
    }

    #[test]
    fn encode_for_l1_fee_authorization_list_without_to_stays_eip1559() {
        let tx = MorphTxEnv {
            tx_type: EIP7702_TX_TYPE,
            chain_id: Some(53077),
            gas_limit: 100_000,
            gas_price: 20_000_000_000,
            gas_priority_fee: Some(1_000_000_000),
            nonce: 1,
            kind: TxKind::Create,
            authorization_list: vec![LazyAuthorization::Signed(sample_signed_authorization())],
            ..Default::default()
        };

        let encoded = tx.encode_for_l1_fee(53077);
        assert_eq!(
            encoded.first(),
            Some(&0x02),
            "SetCodeTx create is invalid; geth falls through to DynamicFeeTx"
        );
    }

    #[test]
    fn encode_for_l1_fee_preserves_eip7702_authorization_list() {
        let signed_authorization = sample_signed_authorization();
        let tx = MorphTxEnv {
            tx_type: EIP7702_TX_TYPE,
            chain_id: Some(53077),
            gas_limit: 100_000,
            gas_price: 20_000_000_000,
            gas_priority_fee: Some(1_000_000_000),
            nonce: 1,
            kind: TxKind::Call(Address::with_last_byte(0xf1)),
            authorization_list: vec![LazyAuthorization::Signed(signed_authorization.clone())],
            ..Default::default()
        };

        let encoded = tx.encode_for_l1_fee(53077);
        assert_eq!(encoded.first(), Some(&0x04));

        let decoded = MorphTxEnvelope::decode_2718(&mut encoded.as_ref()).unwrap();
        let MorphTxEnvelope::Eip7702(decoded) = decoded else {
            panic!("expected EIP-7702 fallback envelope");
        };
        assert_eq!(decoded.tx().authorization_list, vec![signed_authorization]);
    }

    #[test]
    fn set_access_list_upgrades_legacy_tx_type_like_upstream() {
        let mut tx = MorphTxEnv::default();
        tx.set_access_list(AccessList::default());
        assert_eq!(tx.tx_type, EIP2930_TX_TYPE);
    }

    #[test]
    fn test_encode_for_l1_fee_morph_tx() {
        let tx = MorphTxEnv {
            tx_type: MORPH_TX_TYPE_ID,
            chain_id: Some(53077),
            gas_limit: 21_000,
            gas_price: 1,
            nonce: 1,
            kind: TxKind::Call(Address::ZERO),
            fee_token_id: Some(1),
            fee_limit: Some(U256::from(1000)),
            ..Default::default()
        };
        assert!(!tx.encode_for_l1_fee(53077).is_empty());
    }

    fn sample_signed_authorization() -> SignedAuthorization {
        Authorization {
            chain_id: U256::from(53077),
            address: Address::with_last_byte(0x42),
            nonce: 7,
        }
        .into_signed(Signature::new(U256::from(1), U256::from(2), true))
    }

    fn morph_v2_tx_env(version: Option<u8>, authorization: LazyAuthorization) -> MorphTxEnv {
        MorphTxEnv {
            tx_type: MORPH_TX_TYPE_ID,
            chain_id: Some(53077),
            gas_limit: 100_000,
            gas_price: 20_000_000_000,
            gas_priority_fee: Some(1_000_000_000),
            nonce: 1,
            kind: TxKind::Call(Address::with_last_byte(0xf1)),
            authorization_list: vec![authorization],
            version,
            fee_token_id: Some(1),
            fee_limit: Some(U256::from(1000)),
            ..Default::default()
        }
    }

    /// The simulated MorphTx V2 envelope must carry the authorization list so
    /// `eth_estimateGas` / `eth_call` size the L1 data fee like geth's
    /// `asUnsignedMorphTx`; dropping it would under-price the transaction.
    #[test]
    fn encode_for_l1_fee_morph_tx_v2_includes_authorization_list() {
        let signed_authorization = sample_signed_authorization();
        let tx = morph_v2_tx_env(
            Some(MORPH_TX_VERSION_2),
            LazyAuthorization::Signed(signed_authorization.clone()),
        );

        let encoded = tx.encode_for_l1_fee(53077);
        assert_eq!(encoded[0], MORPH_TX_TYPE_ID);
        assert_eq!(encoded[1], MORPH_TX_VERSION_2);

        let decoded = MorphTxEnvelope::decode_2718(&mut encoded.as_ref()).unwrap();
        let MorphTxEnvelope::Morph(decoded) = decoded else {
            panic!("expected MorphTx envelope");
        };
        assert_eq!(decoded.tx().version, MORPH_TX_VERSION_2);
        assert_eq!(decoded.tx().fee_token_id, 1);
        assert_eq!(decoded.tx().authorization_list, vec![signed_authorization]);

        // Sanity: the list actually contributes bytes versus the V1 shape.
        let v1 = MorphTxEnv {
            version: Some(MORPH_TX_VERSION_1),
            authorization_list: vec![],
            ..tx
        };
        assert!(encoded.len() > v1.encode_for_l1_fee(53077).len());
    }

    #[test]
    fn encode_for_l1_fee_morph_tx_version_fallback_picks_v2_with_authorizations() {
        let tx = morph_v2_tx_env(
            None,
            LazyAuthorization::Signed(sample_signed_authorization()),
        );

        let encoded = tx.encode_for_l1_fee(53077);
        let decoded = MorphTxEnvelope::decode_2718(&mut encoded.as_ref()).unwrap();
        let MorphTxEnvelope::Morph(decoded) = decoded else {
            panic!("expected MorphTx envelope");
        };
        assert_eq!(decoded.tx().version, MORPH_TX_VERSION_2);
        assert_eq!(decoded.tx().authorization_list.len(), 1);

        // Without a list the fallback stays V1.
        let no_list = MorphTxEnv {
            authorization_list: vec![],
            ..tx
        };
        let encoded = no_list.encode_for_l1_fee(53077);
        assert_eq!(encoded[1], MORPH_TX_VERSION_1);
    }

    #[test]
    fn encode_for_l1_fee_morph_tx_v2_recovered_authorization_uses_placeholder_signature() {
        let recovered = RecoveredAuthorization::new_unchecked(
            Authorization {
                chain_id: U256::from(53077),
                address: Address::with_last_byte(0x42),
                nonce: 7,
            },
            RecoveredAuthority::Valid(Address::with_last_byte(0x99)),
        );
        let tx = morph_v2_tx_env(
            Some(MORPH_TX_VERSION_2),
            LazyAuthorization::Recovered(recovered),
        );

        let encoded = tx.encode_for_l1_fee(53077);
        let decoded = MorphTxEnvelope::decode_2718(&mut encoded.as_ref()).unwrap();
        let MorphTxEnvelope::Morph(decoded) = decoded else {
            panic!("expected MorphTx envelope");
        };
        let list = decoded.tx().authorization_list.clone();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].nonce, 7);
        // go-ethereum's placeholder: r = s = 0xff..ff, yParity = 1
        assert_eq!(list[0].r(), U256::MAX);
        assert_eq!(list[0].s(), U256::MAX);
    }

    #[test]
    fn from_recovered_tx_morph_v2_populates_authorization_list_and_version() {
        use alloy_consensus::Signed;

        let signed_authorization = sample_signed_authorization();
        let morph_tx = TxMorph {
            chain_id: 53077,
            nonce: 1,
            gas_limit: 100_000,
            max_fee_per_gas: 20_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(Address::with_last_byte(0xf1)),
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: Bytes::new(),
            version: MORPH_TX_VERSION_2,
            fee_token_id: 0,
            fee_limit: U256::ZERO,
            reference: None,
            memo: None,
            authorization_list: vec![signed_authorization.clone()],
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            morph_tx,
            Signature::new(U256::from(1), U256::from(2), false),
            B256::ZERO,
        ));

        let env = MorphTxEnv::from_envelope(&envelope);
        assert_eq!(env.version, Some(MORPH_TX_VERSION_2));
        assert_eq!(env.tx_type, MORPH_TX_TYPE_ID);
        assert_eq!(env.authorization_list.len(), 1);
        match &env.authorization_list[0] {
            LazyAuthorization::Recovered(recovered) => {
                let (authorization, _authority) = recovered.clone().into_parts();
                assert_eq!(&authorization, signed_authorization.inner());
            }
            other => panic!("expected a recovered authorization, got {other:?}"),
        }
    }
}
