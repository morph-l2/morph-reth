//! Morph receipt conversion for `eth_` RPC responses.

use crate::eth::{MorphEthApi, MorphNodeCore};
use crate::types::receipt::MorphRpcReceipt;
use alloy_consensus::{Receipt, TxReceipt};
use alloy_eips::Typed2718;
use alloy_primitives::{B256, Bytes, U64, U256};
use alloy_rpc_types_eth::Log;
use morph_primitives::{
    L1_TX_TYPE_ID, MORPH_TX_TYPE_ID, MorphReceipt, MorphReceiptEnvelope, MorphTxType,
};
use reth_primitives_traits::NodePrimitives;
use reth_rpc_convert::{
    RpcConvert,
    transaction::{ConvertReceiptInput, ReceiptConverter},
};
use reth_rpc_eth_api::helpers::LoadReceipt;
use reth_rpc_eth_types::{EthApiError, receipt::build_receipt};
use std::fmt::Debug;

/// Converter for Morph receipts.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct MorphReceiptConverter;

impl<N> ReceiptConverter<N> for MorphReceiptConverter
where
    N: NodePrimitives<Receipt = MorphReceipt>,
{
    type RpcReceipt = MorphRpcReceipt;
    type Error = reth_rpc_eth_types::EthApiError;

    fn convert_receipts(
        &self,
        inputs: Vec<ConvertReceiptInput<'_, N>>,
    ) -> Result<Vec<Self::RpcReceipt>, Self::Error> {
        let mut receipts = Vec::with_capacity(inputs.len());
        for input in inputs {
            receipts.push(MorphReceiptBuilder::new(input).build());
        }
        Ok(receipts)
    }
}

/// Builds a [`MorphRpcReceipt`].
#[derive(Debug)]
struct MorphReceiptBuilder {
    receipt: MorphRpcReceipt,
}

impl MorphReceiptBuilder {
    /// Creates a new builder from a receipt conversion input.
    fn new<N>(input: ConvertReceiptInput<'_, N>) -> Self
    where
        N: NodePrimitives<Receipt = MorphReceipt>,
    {
        let tx_receipt_fields = morph_tx_receipt_fields(&input.receipt);
        let tx_type = morph_tx_type_from_u8(input.tx.ty());

        let core_receipt = build_receipt(input, None, |receipt, next_log_index, meta| {
            let map_logs = |receipt: Receipt| {
                let Receipt {
                    status,
                    cumulative_gas_used,
                    logs,
                } = receipt;
                let logs = Log::collect_for_receipt(next_log_index, meta, logs);
                Receipt {
                    status,
                    cumulative_gas_used,
                    logs,
                }
            };

            let receipt = match receipt {
                MorphReceipt::Legacy(receipt)
                | MorphReceipt::Eip2930(receipt)
                | MorphReceipt::Eip1559(receipt)
                | MorphReceipt::Eip7702(receipt)
                | MorphReceipt::Morph(receipt) => map_logs(receipt.inner),
                MorphReceipt::L1Msg(receipt) => map_logs(receipt),
            }
            .into_with_bloom();

            match tx_type {
                MorphTxType::Legacy => MorphReceiptEnvelope::Legacy(receipt),
                MorphTxType::Eip2930 => MorphReceiptEnvelope::Eip2930(receipt),
                MorphTxType::Eip1559 => MorphReceiptEnvelope::Eip1559(receipt),
                MorphTxType::Eip7702 => MorphReceiptEnvelope::Eip7702(receipt),
                MorphTxType::L1Msg => MorphReceiptEnvelope::L1Message(receipt),
                MorphTxType::Morph => MorphReceiptEnvelope::Morph(receipt),
            }
        });

        let receipt = MorphRpcReceipt {
            inner: core_receipt,
            l1_fee: tx_receipt_fields.l1_fee,
            version: tx_receipt_fields.version.map(U64::from),
            fee_token_id: tx_receipt_fields.fee_token_id.map(U64::from),
            fee_rate: tx_receipt_fields.fee_rate,
            token_scale: tx_receipt_fields.token_scale,
            fee_limit: tx_receipt_fields.fee_limit,
            reference: tx_receipt_fields.reference,
            memo: tx_receipt_fields.memo,
        };

        Self { receipt }
    }

    /// Consumes the builder and returns the built receipt.
    fn build(self) -> MorphRpcReceipt {
        self.receipt
    }
}

fn morph_tx_type_from_u8(tx_type: u8) -> MorphTxType {
    match tx_type {
        0 => MorphTxType::Legacy,
        1 => MorphTxType::Eip2930,
        2 => MorphTxType::Eip1559,
        4 => MorphTxType::Eip7702,
        L1_TX_TYPE_ID => MorphTxType::L1Msg,
        MORPH_TX_TYPE_ID => MorphTxType::Morph,
        _ => MorphTxType::Legacy,
    }
}

impl<N, Rpc> LoadReceipt for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
}

/// Morph-specific fee fields extracted from a receipt.
#[derive(Debug, Default)]
struct MorphTxReceiptFields {
    l1_fee: U256,
    version: Option<u8>,
    fee_token_id: Option<u16>,
    fee_rate: Option<U256>,
    token_scale: Option<U256>,
    fee_limit: Option<U256>,
    reference: Option<B256>,
    memo: Option<Bytes>,
}

/// Extracts Morph-specific fee fields from a receipt.
///
/// morph-geth's `eth_` RPC keeps Morph receipt extension keys present for
/// every receipt. Numeric metadata that is absent in storage is exposed as
/// zero, while `reference` / `memo` remain null unless populated by MorphTx v1.
fn morph_tx_receipt_fields(receipt: &MorphReceipt) -> MorphTxReceiptFields {
    match receipt {
        MorphReceipt::Legacy(r)
        | MorphReceipt::Eip2930(r)
        | MorphReceipt::Eip1559(r)
        | MorphReceipt::Eip7702(r)
        | MorphReceipt::Morph(r) => MorphTxReceiptFields {
            l1_fee: r.l1_fee,
            version: Some(r.version.unwrap_or_default()),
            fee_token_id: Some(r.fee_token_id.unwrap_or_default()),
            fee_rate: Some(r.fee_rate.unwrap_or_default()),
            token_scale: Some(r.token_scale.unwrap_or_default()),
            fee_limit: Some(r.fee_limit.unwrap_or_default()),
            reference: r.reference,
            memo: r.memo.clone(),
        },
        MorphReceipt::L1Msg(_) => MorphTxReceiptFields {
            version: Some(0),
            fee_token_id: Some(0),
            fee_rate: Some(U256::ZERO),
            token_scale: Some(U256::ZERO),
            fee_limit: Some(U256::ZERO),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Receipt;
    use alloy_primitives::{Bytes as PrimitiveBytes, b256};
    use morph_primitives::MorphTransactionReceipt;

    fn make_morph_receipt_with_fields() -> MorphTransactionReceipt {
        MorphTransactionReceipt {
            inner: Receipt {
                status: alloy_consensus::Eip658Value::Eip658(true),
                cumulative_gas_used: 100_000,
                logs: vec![],
            },
            l1_fee: U256::from(5000),
            version: Some(1),
            fee_token_id: Some(3),
            fee_rate: Some(U256::from(2_000_000)),
            token_scale: Some(U256::from(1_000_000)),
            fee_limit: Some(U256::from(999_999)),
            reference: Some(b256!(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )),
            memo: Some(PrimitiveBytes::from("test memo")),
        }
    }

    #[test]
    fn morph_tx_receipt_fields_extracts_all_fields_from_legacy() {
        let r = make_morph_receipt_with_fields();
        let receipt = MorphReceipt::Legacy(r.clone());
        let fields = morph_tx_receipt_fields(&receipt);

        assert_eq!(fields.l1_fee, r.l1_fee);
        assert_eq!(fields.version, r.version);
        assert_eq!(fields.fee_token_id, r.fee_token_id);
        assert_eq!(fields.fee_rate, r.fee_rate);
        assert_eq!(fields.token_scale, r.token_scale);
        assert_eq!(fields.fee_limit, r.fee_limit);
        assert_eq!(fields.reference, r.reference);
        assert_eq!(fields.memo, r.memo);
    }

    #[test]
    fn morph_tx_receipt_fields_extracts_from_eip1559() {
        let r = make_morph_receipt_with_fields();
        let receipt = MorphReceipt::Eip1559(r.clone());
        let fields = morph_tx_receipt_fields(&receipt);
        assert_eq!(fields.l1_fee, r.l1_fee);
        assert_eq!(fields.fee_token_id, r.fee_token_id);
    }

    #[test]
    fn morph_tx_receipt_fields_extracts_from_morph_type() {
        let r = make_morph_receipt_with_fields();
        let receipt = MorphReceipt::Morph(r.clone());
        let fields = morph_tx_receipt_fields(&receipt);
        assert_eq!(fields.l1_fee, r.l1_fee);
        assert_eq!(fields.version, Some(1));
        assert_eq!(fields.fee_token_id, Some(3));
    }

    #[test]
    fn l1_msg_receipt_returns_default_fields() {
        let receipt = MorphReceipt::L1Msg(Receipt {
            status: alloy_consensus::Eip658Value::Eip658(true),
            cumulative_gas_used: 50_000,
            logs: vec![],
        });
        let fields = morph_tx_receipt_fields(&receipt);

        assert_eq!(fields.l1_fee, U256::ZERO);
        assert_eq!(fields.version, Some(0));
        assert_eq!(fields.fee_token_id, Some(0));
        assert_eq!(fields.fee_rate, Some(U256::ZERO));
        assert_eq!(fields.token_scale, Some(U256::ZERO));
        assert_eq!(fields.fee_limit, Some(U256::ZERO));
        assert!(fields.reference.is_none());
        assert!(fields.memo.is_none());
    }

    #[test]
    fn morph_tx_receipt_fields_handles_zero_l1_fee() {
        let mut r = make_morph_receipt_with_fields();
        r.l1_fee = U256::ZERO;
        let receipt = MorphReceipt::Eip2930(r);
        let fields = morph_tx_receipt_fields(&receipt);
        assert_eq!(fields.l1_fee, U256::ZERO);
    }

    #[test]
    fn morph_tx_receipt_fields_eip7702() {
        let r = make_morph_receipt_with_fields();
        let receipt = MorphReceipt::Eip7702(r.clone());
        let fields = morph_tx_receipt_fields(&receipt);
        assert_eq!(fields.l1_fee, r.l1_fee);
        assert_eq!(fields.reference, r.reference);
    }

    #[test]
    fn morph_tx_receipt_fields_defaults_absent_numeric_metadata_to_zero() {
        let receipt = MorphReceipt::Eip1559(MorphTransactionReceipt {
            inner: Receipt {
                status: alloy_consensus::Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            l1_fee: U256::from(123),
            version: None,
            fee_token_id: None,
            fee_rate: None,
            token_scale: None,
            fee_limit: None,
            reference: None,
            memo: None,
        });

        let fields = morph_tx_receipt_fields(&receipt);

        assert_eq!(fields.l1_fee, U256::from(123));
        assert_eq!(fields.version, Some(0));
        assert_eq!(fields.fee_token_id, Some(0));
        assert_eq!(fields.fee_rate, Some(U256::ZERO));
        assert_eq!(fields.token_scale, Some(U256::ZERO));
        assert_eq!(fields.fee_limit, Some(U256::ZERO));
        assert!(fields.reference.is_none());
        assert!(fields.memo.is_none());
    }

    /// Regression test for the `transactionReceipts` subscription wiring.
    ///
    /// reth v2.2.0 exposes a `transactionReceipts` pubsub topic that, in the
    /// `SubscriptionKind::TransactionReceipts` arm of
    /// `reth_rpc::eth::pubsub`, calls `converter.convert_receipts(inputs)`
    /// against the same converter the RPC `eth_getBlockReceipts` /
    /// `eth_getTransactionReceipt` endpoints use. For Morph that converter
    /// is [`MorphReceiptConverter`].
    ///
    /// This test exercises the converter end-to-end on a Morph-tagged
    /// receipt and asserts every Morph-specific field survives the
    /// conversion. If any of these assertions break in a future bump,
    /// pubsub `transactionReceipts` subscribers would silently lose Morph
    /// metadata.
    #[test]
    fn transaction_receipts_subscription_preserves_morph_fields() {
        use alloy_consensus::{Signed, TxEip1559, transaction::Recovered};
        use alloy_primitives::{B256, Signature, U256, address};
        use morph_primitives::{MorphPrimitives, MorphTxEnvelope};
        use reth_primitives_traits::TransactionMeta;
        use reth_rpc_convert::transaction::{ConvertReceiptInput, ReceiptConverter};

        let signer = address!("0000000000000000000000000000000000000099");

        let envelope = MorphTxEnvelope::Eip1559(Signed::new_unchecked(
            TxEip1559 {
                chain_id: 2818,
                nonce: 0,
                gas_limit: 21_000,
                max_fee_per_gas: 2_000_000_000,
                max_priority_fee_per_gas: 1_000_000,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            B256::ZERO,
        ));
        let recovered: Recovered<&MorphTxEnvelope> = Recovered::new_unchecked(&envelope, signer);

        let receipt = MorphReceipt::Morph(make_morph_receipt_with_fields());

        let meta = TransactionMeta {
            tx_hash: B256::ZERO,
            index: 0,
            block_hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            block_number: 42,
            base_fee: Some(1_000_000_000),
            excess_blob_gas: None,
            timestamp: 1_700_000_000,
        };

        let input = ConvertReceiptInput::<'_, MorphPrimitives> {
            receipt,
            tx: recovered,
            gas_used: 21_000,
            next_log_index: 0,
            meta,
        };

        let rpc_receipts = MorphReceiptConverter
            .convert_receipts(vec![input])
            .expect("morph converter should not fail on a well-formed input");
        assert_eq!(rpc_receipts.len(), 1);
        let rpc = &rpc_receipts[0];

        // Morph-specific top-level RPC fields must round-trip from the
        // primitive `MorphTransactionReceipt` into `MorphRpcReceipt`.
        assert_eq!(rpc.l1_fee, U256::from(5000));
        assert_eq!(rpc.version, Some(U64::from(1)));
        assert_eq!(rpc.fee_token_id, Some(U64::from(3)));
        assert_eq!(rpc.fee_rate, Some(U256::from(2_000_000)));
        assert_eq!(rpc.token_scale, Some(U256::from(1_000_000)));
        assert_eq!(rpc.fee_limit, Some(U256::from(999_999)));
        assert_eq!(
            rpc.reference,
            Some(b256!(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            ))
        );
        assert_eq!(rpc.memo, Some(PrimitiveBytes::from("test memo")));

        // Standard inner fields (from `build_receipt`) must also be plumbed.
        assert_eq!(rpc.inner.from, signer);
        assert_eq!(rpc.inner.block_number, Some(42));
        assert_eq!(rpc.inner.transaction_index, Some(0));
    }

    #[test]
    fn receipt_rpc_type_follows_transaction_type_for_historical_receipts() {
        use alloy_consensus::{Signed, TxEip1559, transaction::Recovered};
        use alloy_primitives::{B256, Signature, U256, address};
        use morph_primitives::{MorphPrimitives, MorphTxEnvelope, MorphTxType};
        use reth_primitives_traits::TransactionMeta;
        use reth_rpc_convert::transaction::{ConvertReceiptInput, ReceiptConverter};

        let signer = address!("0000000000000000000000000000000000000099");
        let envelope = MorphTxEnvelope::Eip1559(Signed::new_unchecked(
            TxEip1559 {
                chain_id: 2818,
                nonce: 0,
                gas_limit: 21_000,
                max_fee_per_gas: 2_000_000_000,
                max_priority_fee_per_gas: 1_000_000,
                ..Default::default()
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            B256::ZERO,
        ));
        let recovered: Recovered<&MorphTxEnvelope> = Recovered::new_unchecked(&envelope, signer);

        // Historical receipts may be decoded from storage without preserving
        // the original typed receipt variant. RPC must follow the transaction
        // envelope type, not the storage receipt wrapper.
        let receipt = MorphReceipt::Legacy(MorphTransactionReceipt {
            inner: Receipt {
                status: alloy_consensus::Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            l1_fee: U256::ZERO,
            version: None,
            fee_token_id: None,
            fee_rate: None,
            token_scale: None,
            fee_limit: None,
            reference: None,
            memo: None,
        });

        let input = ConvertReceiptInput::<'_, MorphPrimitives> {
            receipt,
            tx: recovered,
            gas_used: 21_000,
            next_log_index: 0,
            meta: TransactionMeta {
                tx_hash: B256::ZERO,
                index: 0,
                block_hash: B256::ZERO,
                block_number: 42,
                base_fee: Some(1_000_000_000),
                excess_blob_gas: None,
                timestamp: 1_700_000_000,
            },
        };

        let rpc = MorphReceiptConverter
            .convert_receipts(vec![input])
            .expect("morph converter should not fail")
            .pop()
            .expect("converter must produce one receipt per input");

        assert_eq!(rpc.inner.inner.tx_type(), MorphTxType::Eip1559);
        assert_eq!(
            serde_json::to_value(&rpc).unwrap().get("type"),
            Some(&serde_json::json!("0x2"))
        );
    }

    /// Companion test: L1 message receipts must come back from the
    /// pubsub-style converter path with default Morph fields and the
    /// L1Msg envelope variant, just like `eth_getBlockReceipts`.
    #[test]
    fn transaction_receipts_subscription_l1_msg_carries_default_morph_fields() {
        use alloy_consensus::transaction::Recovered;
        use alloy_primitives::{Address, B256, Sealed, U256, address};
        use morph_primitives::transaction::TxL1Msg;
        use morph_primitives::{MorphPrimitives, MorphTxEnvelope};
        use reth_primitives_traits::TransactionMeta;
        use reth_rpc_convert::transaction::{ConvertReceiptInput, ReceiptConverter};

        let l1_msg = TxL1Msg {
            queue_index: 7,
            gas_limit: 100_000,
            sender: address!("000000000000000000000000000000000000dead"),
            ..Default::default()
        };
        let envelope = MorphTxEnvelope::L1Msg(Sealed::new_unchecked(l1_msg, B256::ZERO));
        let recovered: Recovered<&MorphTxEnvelope> =
            Recovered::new_unchecked(&envelope, Address::ZERO);

        let receipt = MorphReceipt::L1Msg(Receipt {
            status: alloy_consensus::Eip658Value::Eip658(true),
            cumulative_gas_used: 50_000,
            logs: vec![],
        });

        let meta = TransactionMeta {
            tx_hash: B256::ZERO,
            index: 0,
            block_hash: B256::ZERO,
            block_number: 1,
            base_fee: None,
            excess_blob_gas: None,
            timestamp: 0,
        };

        let input = ConvertReceiptInput::<'_, MorphPrimitives> {
            receipt,
            tx: recovered,
            gas_used: 50_000,
            next_log_index: 0,
            meta,
        };

        let rpc = MorphReceiptConverter
            .convert_receipts(vec![input])
            .expect("morph converter should not fail on a well-formed L1 message input")
            .pop()
            .expect("converter must produce one receipt per input");

        // L1 messages have no MorphTx metadata, but RPC compatibility with
        // morph-geth still exposes absent numeric extension fields as zero.
        assert_eq!(rpc.l1_fee, U256::ZERO);
        assert_eq!(rpc.version, Some(U64::ZERO));
        assert_eq!(rpc.fee_token_id, Some(U64::ZERO));
        assert_eq!(rpc.fee_rate, Some(U256::ZERO));
        assert_eq!(rpc.token_scale, Some(U256::ZERO));
        assert_eq!(rpc.fee_limit, Some(U256::ZERO));
        assert!(rpc.reference.is_none());
        assert!(rpc.memo.is_none());
    }
}
