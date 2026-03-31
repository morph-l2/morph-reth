//! Morph receipt conversion for `eth_` RPC responses.

use crate::eth::{MorphEthApi, MorphNodeCore};
use crate::types::receipt::MorphRpcReceipt;
use alloy_consensus::{Receipt, TxReceipt};
use alloy_primitives::{B256, Bytes, U64, U256};
use alloy_rpc_types_eth::Log;
use morph_primitives::{MorphReceipt, MorphReceiptEnvelope};
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

            match receipt {
                MorphReceipt::Legacy(receipt) => {
                    MorphReceiptEnvelope::Legacy(map_logs(receipt.inner).into_with_bloom())
                }
                MorphReceipt::Eip2930(receipt) => {
                    MorphReceiptEnvelope::Eip2930(map_logs(receipt.inner).into_with_bloom())
                }
                MorphReceipt::Eip1559(receipt) => {
                    MorphReceiptEnvelope::Eip1559(map_logs(receipt.inner).into_with_bloom())
                }
                MorphReceipt::Eip7702(receipt) => {
                    MorphReceiptEnvelope::Eip7702(map_logs(receipt.inner).into_with_bloom())
                }
                MorphReceipt::L1Msg(receipt) => {
                    MorphReceiptEnvelope::L1Message(map_logs(receipt).into_with_bloom())
                }
                MorphReceipt::Morph(receipt) => {
                    MorphReceiptEnvelope::Morph(map_logs(receipt.inner).into_with_bloom())
                }
            }
        });

        let receipt = MorphRpcReceipt {
            inner: core_receipt,
            l1_fee: tx_receipt_fields.l1_fee,
            version: tx_receipt_fields.version,
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
/// L1 message receipts return zero/None for all fee fields.
fn morph_tx_receipt_fields(receipt: &MorphReceipt) -> MorphTxReceiptFields {
    match receipt {
        MorphReceipt::Legacy(r)
        | MorphReceipt::Eip2930(r)
        | MorphReceipt::Eip1559(r)
        | MorphReceipt::Eip7702(r)
        | MorphReceipt::Morph(r) => MorphTxReceiptFields {
            l1_fee: r.l1_fee,
            version: r.version,
            fee_token_id: r.fee_token_id,
            fee_rate: r.fee_rate,
            token_scale: r.token_scale,
            fee_limit: r.fee_limit,
            reference: r.reference,
            memo: r.memo.clone(),
        },
        MorphReceipt::L1Msg(_) => MorphTxReceiptFields::default(),
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
        assert!(fields.version.is_none());
        assert!(fields.fee_token_id.is_none());
        assert!(fields.fee_rate.is_none());
        assert!(fields.token_scale.is_none());
        assert!(fields.fee_limit.is_none());
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
}
