//! Receipt builder for Morph block execution.
//!
//! This module provides the [`MorphReceiptBuilder`] which constructs receipts
//! for executed transactions based on their type.

use alloy_consensus::Receipt;
use alloy_evm::{
    Evm,
    eth::receipt_builder::{ReceiptBuilder, ReceiptBuilderCtx},
};
use morph_primitives::{MorphReceipt, MorphTransactionReceipt, MorphTxEnvelope, MorphTxType};

/// Builder for [`MorphReceipt`].
///
/// Creates the appropriate receipt variant based on transaction type.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub(crate) struct MorphReceiptBuilder;

impl ReceiptBuilder for MorphReceiptBuilder {
    type Transaction = MorphTxEnvelope;
    type Receipt = MorphReceipt;

    fn build_receipt<E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'_, Self::Transaction, E>,
    ) -> Self::Receipt {
        let ReceiptBuilderCtx {
            tx,
            result,
            cumulative_gas_used,
            ..
        } = ctx;

        let inner = Receipt {
            status: result.is_success().into(),
            cumulative_gas_used,
            logs: result.into_logs(),
        };

        // Create the appropriate receipt variant based on transaction type
        // TODO: Add L1 fee calculation from execution context
        match tx.tx_type() {
            MorphTxType::Legacy => MorphReceipt::Legacy(MorphTransactionReceipt::new(inner)),
            MorphTxType::Eip2930 => MorphReceipt::Eip2930(MorphTransactionReceipt::new(inner)),
            MorphTxType::Eip1559 => MorphReceipt::Eip1559(MorphTransactionReceipt::new(inner)),
            MorphTxType::Eip7702 => MorphReceipt::Eip7702(MorphTransactionReceipt::new(inner)),
            MorphTxType::L1Msg => MorphReceipt::L1Msg(inner),
            MorphTxType::AltFee => MorphReceipt::AltFee(MorphTransactionReceipt::new(inner)),
        }
    }
}
