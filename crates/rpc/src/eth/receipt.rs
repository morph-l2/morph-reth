//! Morph receipt conversion for `eth_` RPC responses.

use crate::eth::{MorphEthApi, MorphNodeCore};
use crate::types::receipt::MorphRpcReceipt;
use alloy_consensus::{Receipt, TxReceipt};
use alloy_primitives::{U64, U256};
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
    fn new<N>(input: ConvertReceiptInput<'_, N>) -> Self
    where
        N: NodePrimitives<Receipt = MorphReceipt>,
    {
        let (l1_fee, fee_token_id, fee_rate, token_scale, fee_limit) =
            morph_fee_fields(&input.receipt);

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
            l1_fee,
            fee_rate,
            token_scale,
            fee_limit,
            fee_token_id: fee_token_id.map(U64::from),
        };

        Self { receipt }
    }

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

/// Extracts Morph-specific fee fields from a receipt.
///
/// Returns a tuple of (l1_fee, fee_token_id, fee_rate, token_scale, fee_limit).
/// L1 message receipts return zero/None for all fee fields.
fn morph_fee_fields(
    receipt: &MorphReceipt,
) -> (U256, Option<u16>, Option<U256>, Option<U256>, Option<U256>) {
    match receipt {
        MorphReceipt::Legacy(r)
        | MorphReceipt::Eip2930(r)
        | MorphReceipt::Eip1559(r)
        | MorphReceipt::Eip7702(r)
        | MorphReceipt::Morph(r) => (
            r.l1_fee,
            r.fee_token_id,
            r.fee_rate,
            r.token_scale,
            r.fee_limit,
        ),
        MorphReceipt::L1Msg(_) => (U256::ZERO, None, None, None, None),
    }
}
