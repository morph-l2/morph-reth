//! Receipt builder for Morph block execution.
//!
//! This module provides the [`MorphReceiptBuilder`] which constructs receipts
//! for executed transactions based on their type, including L1 fee calculation.

use alloy_consensus::Receipt;
use alloy_evm::{
    Evm,
    eth::receipt_builder::{ReceiptBuilder, ReceiptBuilderCtx as AlloyReceiptBuilderCtx},
};
use alloy_primitives::U256;
use morph_primitives::{MorphReceipt, MorphTransactionReceipt, MorphTxEnvelope, MorphTxType};
use revm::context::result::ExecutionResult;

/// Context for building a Morph receipt.
///
/// Contains all the information needed to construct a receipt for an executed transaction.
#[derive(Debug)]
pub(crate) struct MorphReceiptBuilderCtx<'a, E: Evm> {
    /// The executed transaction
    pub tx: &'a MorphTxEnvelope,
    /// Result of transaction execution
    pub result: ExecutionResult<E::HaltReason>,
    /// Cumulative gas used in the block up to and including this transaction
    pub cumulative_gas_used: u64,
    /// L1 data fee for this transaction
    pub l1_fee: U256,
    /// Token fee information for MorphTx (0x7F) transactions
    pub token_fee_info: Option<MorphTxTokenFeeInfo>,
}

/// Token fee information for MorphTx transactions.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MorphTxTokenFeeInfo {
    /// Token ID for fee payment
    pub fee_token_id: u16,
    /// Exchange rate for the fee token
    pub fee_rate: U256,
    /// Scale factor for the token
    pub token_scale: U256,
    /// Fee limit specified in the transaction
    pub fee_limit: U256,
}

/// Trait for building Morph receipts.
pub(crate) trait MorphReceiptBuilder: Send + Sync {
    /// Build a receipt from the execution context.
    fn build_receipt<E: Evm>(&self, ctx: MorphReceiptBuilderCtx<'_, E>) -> MorphReceipt;
}

/// Default builder for [`MorphReceipt`].
///
/// Creates the appropriate receipt variant based on transaction type and includes:
/// - L1 data fee for all non-L1Message transactions
/// - Token fee information for MorphTx (0x7F) transactions
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub(crate) struct DefaultMorphReceiptBuilder;

impl MorphReceiptBuilder for DefaultMorphReceiptBuilder {
    fn build_receipt<E: Evm>(&self, ctx: MorphReceiptBuilderCtx<'_, E>) -> MorphReceipt {
        let MorphReceiptBuilderCtx {
            tx,
            result,
            cumulative_gas_used,
            l1_fee,
            token_fee_info,
        } = ctx;

        let inner = Receipt {
            status: result.is_success().into(),
            cumulative_gas_used,
            logs: result.into_logs(),
        };

        // Create the appropriate receipt variant based on transaction type
        match tx.tx_type() {
            MorphTxType::Legacy => {
                MorphReceipt::Legacy(MorphTransactionReceipt::with_l1_fee(inner, l1_fee))
            }
            MorphTxType::Eip2930 => {
                MorphReceipt::Eip2930(MorphTransactionReceipt::with_l1_fee(inner, l1_fee))
            }
            MorphTxType::Eip1559 => {
                MorphReceipt::Eip1559(MorphTransactionReceipt::with_l1_fee(inner, l1_fee))
            }
            MorphTxType::Eip7702 => {
                MorphReceipt::Eip7702(MorphTransactionReceipt::with_l1_fee(inner, l1_fee))
            }
            MorphTxType::L1Msg => {
                // L1 messages don't pay L1 fees
                MorphReceipt::L1Msg(inner)
            }
            MorphTxType::Morph => {
                // MorphTx transactions include token fee information
                if let Some(token_info) = token_fee_info {
                    MorphReceipt::Morph(MorphTransactionReceipt::with_morph_tx(
                        inner,
                        l1_fee,
                        token_info.fee_token_id,
                        token_info.fee_rate,
                        token_info.token_scale,
                        token_info.fee_limit,
                    ))
                } else {
                    // Fallback: just include L1 fee if token info is missing
                    MorphReceipt::Morph(MorphTransactionReceipt::with_l1_fee(inner, l1_fee))
                }
            }
        }
    }
}

/// Implementation of alloy-evm's `ReceiptBuilder` trait.
///
/// This implementation is primarily needed to satisfy type bounds in `EthBlockExecutorFactory`.
/// The actual receipt building during block execution is done via `MorphReceiptBuilder`
/// in `MorphBlockExecutor::commit_transaction`.
///
/// Note: This implementation builds receipts without L1 fee and token fee information,
/// since the alloy-evm `ReceiptBuilderCtx` doesn't provide these fields.
impl ReceiptBuilder for DefaultMorphReceiptBuilder {
    type Transaction = MorphTxEnvelope;
    type Receipt = MorphReceipt;

    fn build_receipt<E: Evm>(
        &self,
        ctx: AlloyReceiptBuilderCtx<'_, Self::Transaction, E>,
    ) -> Self::Receipt {
        let AlloyReceiptBuilderCtx { tx, result, cumulative_gas_used, .. } = ctx;

        let inner = Receipt {
            status: result.is_success().into(),
            cumulative_gas_used,
            logs: result.into_logs(),
        };

        // Build receipts without L1 fee since it's not available in the context.
        // In practice, this code path is not used during execution - receipts are built
        // via MorphBlockExecutor which has full context including L1 fees.
        match tx.tx_type() {
            MorphTxType::Legacy => {
                MorphReceipt::Legacy(MorphTransactionReceipt::with_l1_fee(inner, U256::ZERO))
            }
            MorphTxType::Eip2930 => {
                MorphReceipt::Eip2930(MorphTransactionReceipt::with_l1_fee(inner, U256::ZERO))
            }
            MorphTxType::Eip1559 => {
                MorphReceipt::Eip1559(MorphTransactionReceipt::with_l1_fee(inner, U256::ZERO))
            }
            MorphTxType::Eip7702 => {
                MorphReceipt::Eip7702(MorphTransactionReceipt::with_l1_fee(inner, U256::ZERO))
            }
            MorphTxType::L1Msg => MorphReceipt::L1Msg(inner),
            MorphTxType::Morph => {
                MorphReceipt::Morph(MorphTransactionReceipt::with_l1_fee(inner, U256::ZERO))
            }
        }
    }
}
