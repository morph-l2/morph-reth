//! Receipt builder for Morph block execution.
//!
//! This module provides the receipt building infrastructure for Morph L2 blocks.
//! Receipts contain essential information about transaction execution results,
//! including Morph-specific fields like L1 data fees and token fee information.
//!
//! # Why Custom Receipt Builder?
//!
//! Unlike standard Ethereum receipts, Morph receipts include:
//! - **L1 Data Fee**: The cost charged for posting transaction data to L1
//! - **Token Fee Info**: For MorphTx (0x7F), includes exchange rate, fee limit, reference, and memo
//!
//! The standard `EthBlockExecutor` doesn't have access to L1 fee information
//! during receipt building. This module provides a custom builder that receives
//! the pre-calculated L1 fee as part of its context.
//!
//! # Receipt Types
//!
//! | Transaction Type | Receipt Content |
//! |-----------------|-----------------|
//! | Legacy (0x00) | inner + l1_fee |
//! | EIP-2930 (0x01) | inner + l1_fee |
//! | EIP-1559 (0x02) | inner + l1_fee |
//! | EIP-7702 (0x04) | inner + l1_fee |
//! | L1Message (0x7E) | inner only (no L1 fee) |
//! | MorphTx (0x7F) | inner + l1_fee + token_fee_info + reference + memo |

use alloy_consensus::Receipt;
use alloy_consensus::transaction::TxHashRef;
use alloy_evm::Evm;
use alloy_primitives::{B256, Bytes, U256};
use morph_primitives::{MorphReceipt, MorphTransactionReceipt, MorphTxEnvelope, MorphTxType};
use revm::context::result::ExecutionResult;
use tracing::warn;

/// Context for building a Morph receipt.
///
/// This struct aggregates all information needed to construct a receipt for
/// an executed transaction. It is populated by the block executor after
/// transaction execution and L1 fee calculation.
///
/// # Fields
/// - `tx`: The original transaction (needed for determining receipt type)
/// - `result`: EVM execution result (success/failure, logs, gas used)
/// - `cumulative_gas_used`: Running total of gas used in the block
/// - `l1_fee`: Pre-calculated L1 data fee for this transaction
/// - `token_fee_info`: Token fee details for MorphTx transactions
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
    /// MorphTx-specific fields (token fee info, version, reference, memo)
    pub morph_tx_fields: Option<MorphTxFields>,
}

/// MorphTx (0x7F) specific fields for receipts.
///
/// This struct aggregates all Morph-specific transaction fields that need to be
/// included in the receipt, including:
/// - Token fee information (when using ERC20 for gas payment)
/// - Transaction metadata (version, reference, memo)
///
/// # Token Fee Calculation Formula
/// ```text
/// token_fee = eth_fee * fee_rate / token_scale
/// ```
///
/// # Fields
/// - `version`: The version of the Morph transaction format (0 = legacy, 1 = with reference/memo)
/// - `fee_token_id`: ID of the ERC20 token registered in L2TokenRegistry
/// - `fee_rate`: Exchange rate from L2TokenRegistry (token per ETH)
/// - `token_scale`: Decimal scale factor for the token (e.g., 10^18)
/// - `fee_limit`: Maximum tokens the user agreed to pay
/// - `reference`: 32-byte key for transaction indexing by external systems
/// - `memo`: Arbitrary data field (up to 64 bytes)
#[derive(Debug, Clone)]
pub(crate) struct MorphTxFields {
    /// Version of the Morph transaction format
    pub version: u8,
    /// Token ID for fee payment
    pub fee_token_id: u16,
    /// Exchange rate for the fee token
    pub fee_rate: U256,
    /// Scale factor for the token
    pub token_scale: U256,
    /// Fee limit specified in the transaction
    pub fee_limit: U256,
    /// Reference key for transaction indexing
    pub reference: Option<B256>,
    /// Memo field for arbitrary data
    pub memo: Option<Bytes>,
}

/// Trait for building Morph receipts from execution context.
///
/// This trait abstracts receipt construction to allow different implementations
/// (e.g., for testing or custom receipt formats). The default implementation
/// is [`DefaultMorphReceiptBuilder`].
///
/// # Thread Safety
/// Implementations must be `Send + Sync` as the builder is shared across
/// the block executor and may be accessed concurrently.
pub(crate) trait MorphReceiptBuilder: Send + Sync {
    /// Builds a receipt from the execution context.
    ///
    /// # Arguments
    /// * `ctx` - Context containing transaction, execution result, and fee info
    ///
    /// # Returns
    /// A [`MorphReceipt`] variant appropriate for the transaction type.
    fn build_receipt<E: Evm>(&self, ctx: MorphReceiptBuilderCtx<'_, E>) -> MorphReceipt;
}

/// Default builder for [`MorphReceipt`].
///
/// This builder creates the appropriate receipt variant based on transaction type:
///
/// ## Standard Transactions (Legacy, EIP-2930, EIP-1559, EIP-7702)
/// - Wraps the base receipt with L1 fee using `with_l1_fee()`
/// - L1 fee is non-zero for all L2-originated transactions
///
/// ## L1 Message Transactions (0x7E)
/// - Uses base receipt without L1 fee
/// - These transactions originate from L1 and don't pay L1 data fees
///
/// ## MorphTx Transactions (0x7F)
/// - Includes L1 fee plus MorphTx-specific fields
/// - Uses `with_morph_tx_v1()` to populate all MorphTx fields
/// - Falls back to `with_l1_fee()` if MorphTx fields are unexpectedly missing
///
/// # Note
/// The builder is stateless and can be reused across multiple receipts.
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
            morph_tx_fields,
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
                // MorphTx transactions should always have MorphTx-specific fields.
                // If fields are missing, it indicates one of the following:
                // 1. The fee token is not registered in L2TokenRegistry
                // 2. TokenFeeInfo::fetch returned None (token inactive or query failed)
                // 3. A bug in get_morph_tx_fields logic
                //
                // We log a warning and fallback to L1-fee-only receipt to avoid
                // blocking block execution, but this should be investigated.
                if let Some(fields) = morph_tx_fields {
                    MorphReceipt::Morph(MorphTransactionReceipt::with_morph_tx_v1(
                        inner,
                        l1_fee,
                        fields.version,
                        fields.fee_token_id,
                        fields.fee_rate,
                        fields.token_scale,
                        fields.fee_limit,
                        fields.reference,
                        fields.memo,
                    ))
                } else {
                    warn!(
                        target: "morph::receipt",
                        tx_hash = ?tx.tx_hash(),
                        "MorphTx missing token fee fields - receipt will not include fee token info. \
                         This may indicate an unregistered/inactive token or a bug."
                    );
                    MorphReceipt::Morph(MorphTransactionReceipt::with_l1_fee(inner, l1_fee))
                }
            }
        }
    }
}
