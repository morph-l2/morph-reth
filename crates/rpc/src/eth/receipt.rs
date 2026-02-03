//! Morph receipt conversion for `eth_` RPC responses.

use crate::types::receipt::MorphRpcReceipt;
use alloy_consensus::{Receipt, Transaction, TxReceipt};
use alloy_primitives::{Address, TxKind, U256, U64};
use alloy_rpc_types_eth::{Log, TransactionReceipt};
use morph_primitives::{MorphReceipt, MorphReceiptEnvelope};
use reth_primitives_traits::NodePrimitives;
use reth_rpc_convert::transaction::{ConvertReceiptInput, ReceiptConverter};
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
        let ConvertReceiptInput { tx, meta, receipt, gas_used, next_log_index } = input;

        let from = tx.signer();
        let (contract_address, to) = match tx.kind() {
            TxKind::Create => (Some(from.create(tx.nonce())), None),
            TxKind::Call(addr) => (None, Some(Address(*addr))),
        };

        let (l1_fee, fee_token_id, fee_rate, token_scale, fee_limit) =
            morph_fee_fields(&receipt);

        let map_logs = |receipt: Receipt| {
            let Receipt { status, cumulative_gas_used, logs } = receipt;
            let logs = Log::collect_for_receipt(next_log_index, meta, logs);
            Receipt { status, cumulative_gas_used, logs }
        };

        let receipt_envelope = match receipt {
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
        };

        let inner = TransactionReceipt {
            inner: receipt_envelope,
            transaction_hash: meta.tx_hash,
            transaction_index: Some(meta.index),
            block_hash: Some(meta.block_hash),
            block_number: Some(meta.block_number),
            gas_used,
            effective_gas_price: tx.effective_gas_price(meta.base_fee),
            blob_gas_used: None,
            blob_gas_price: None,
            from,
            to,
            contract_address,
        };

        let receipt = MorphRpcReceipt {
            inner,
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

