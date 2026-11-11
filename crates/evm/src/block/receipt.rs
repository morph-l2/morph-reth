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
//! The standard `EthBlockExecutor` builds receipts generically over the EVM type family, so it
//! cannot read Morph's result extension. This module provides a custom builder that does.
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
use alloy_primitives::{B256, Bytes, U256};
use evm2::TxResult;
use morph_primitives::{MorphReceipt, MorphTransactionReceipt, MorphTxType};
use morph_revm::MorphEvmTypes;
use tracing::warn;

/// Context for building a Morph receipt.
///
/// This struct aggregates all information needed to construct a receipt for
/// an executed transaction. It is populated by the block executor after
/// transaction execution.
///
/// # Fields
/// - `tx_type`: The type of the executed transaction (selects the receipt variant)
/// - `result`: EVM execution result (status, logs, L1 data fee)
/// - `cumulative_gas_used`: Running total of gas used in the block
/// - `morph_tx_fields`: MorphTx-specific fields (token fee info, version, reference, memo)
#[derive(Debug)]
pub(crate) struct MorphReceiptBuilderCtx {
    /// The type of the executed transaction
    pub tx_type: MorphTxType,
    /// Result of transaction execution
    pub result: TxResult<MorphEvmTypes>,
    /// Cumulative gas used in the block up to and including this transaction
    pub cumulative_gas_used: u64,
    /// MorphTx-specific fields (token fee info, version, reference, memo)
    pub morph_tx_fields: Option<MorphReceiptTxFields>,
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
pub(crate) struct MorphReceiptTxFields {
    /// Version of the Morph transaction format
    pub version: u8,
    /// Token ID for fee payment
    pub fee_token_id: u16,
    /// Exchange rate for the fee token, absent when paying with ETH
    pub fee_rate: Option<U256>,
    /// Scale factor for the token, absent when paying with ETH
    pub token_scale: Option<U256>,
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
    fn build_receipt(&self, ctx: MorphReceiptBuilderCtx) -> MorphReceipt;
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
    fn build_receipt(&self, ctx: MorphReceiptBuilderCtx) -> MorphReceipt {
        let MorphReceiptBuilderCtx {
            tx_type,
            result,
            cumulative_gas_used,
            morph_tx_fields,
        } = ctx;
        let l1_fee = result.ext.l1_data_fee;

        // The EVM reports logs in chronological order, matching go-ethereum:
        //   [deduct Transfer] + [main tx logs] + [refund Transfer]
        // A reverted main frame drops only its own logs: the fee logs were emitted outside
        // its checkpoint, as they sit outside go-ethereum's snapshot/revert mechanism.
        let inner = Receipt {
            status: result.status.into(),
            cumulative_gas_used,
            logs: result.logs,
        };

        // Create the appropriate receipt variant based on transaction type
        match tx_type {
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
                        target: "morph::evm",
                        "MorphTx missing token fee fields - receipt will not include fee token info. \
                         This may indicate an unregistered/inactive token or a bug."
                    );
                    MorphReceipt::Morph(MorphTransactionReceipt::with_l1_fee(inner, l1_fee))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxReceipt;
    use alloy_primitives::{Address, Log};
    use evm2::{TxResultExt, interpreter::InstrStop};
    use morph_revm::MorphTxResultExt;

    fn make_result(
        status: bool,
        gas_used: u64,
        logs: Vec<Log>,
        l1_fee: U256,
    ) -> TxResult<MorphEvmTypes> {
        TxResultExt {
            status,
            total_gas_spent: gas_used,
            stop: if status {
                InstrStop::Stop
            } else {
                InstrStop::Revert
            },
            logs,
            ext: MorphTxResultExt {
                l1_data_fee: l1_fee,
                token_fee_info: None,
            },
            ..Default::default()
        }
    }

    fn make_success_result(gas_used: u64, l1_fee: U256) -> TxResult<MorphEvmTypes> {
        make_result(true, gas_used, vec![], l1_fee)
    }

    #[test]
    fn test_build_legacy_receipt() {
        let builder = DefaultMorphReceiptBuilder;
        let l1_fee = U256::from(5000u64);

        let ctx = MorphReceiptBuilderCtx {
            tx_type: MorphTxType::Legacy,
            result: make_success_result(21000, l1_fee),
            cumulative_gas_used: 21000,
            morph_tx_fields: None,
        };

        let receipt = builder.build_receipt(ctx);
        assert!(matches!(receipt, MorphReceipt::Legacy(_)));
        assert_eq!(receipt.l1_fee(), l1_fee);
        assert_eq!(receipt.cumulative_gas_used(), 21000);
        assert!(receipt.status());
    }

    #[test]
    fn test_build_eip1559_receipt() {
        let builder = DefaultMorphReceiptBuilder;
        let l1_fee = U256::from(8000u64);

        let ctx = MorphReceiptBuilderCtx {
            tx_type: MorphTxType::Eip1559,
            result: make_success_result(21000, l1_fee),
            cumulative_gas_used: 42000,
            morph_tx_fields: None,
        };

        let receipt = builder.build_receipt(ctx);
        assert!(matches!(receipt, MorphReceipt::Eip1559(_)));
        assert_eq!(receipt.l1_fee(), l1_fee);
        assert_eq!(receipt.cumulative_gas_used(), 42000);
    }

    #[test]
    fn test_build_l1_msg_receipt_no_l1_fee() {
        let builder = DefaultMorphReceiptBuilder;

        let ctx = MorphReceiptBuilderCtx {
            tx_type: MorphTxType::L1Msg,
            // Pass a non-zero l1_fee to verify the builder ignores it for L1 messages.
            // L1 message gas is prepaid on L1, so no L1 fee should appear in the receipt.
            result: make_success_result(21000, U256::from(999_999)),
            cumulative_gas_used: 21000,
            morph_tx_fields: None,
        };

        let receipt = builder.build_receipt(ctx);
        assert!(matches!(receipt, MorphReceipt::L1Msg(_)));
        // L1 messages return ZERO for l1_fee regardless of what was passed in
        assert_eq!(receipt.l1_fee(), U256::ZERO);
    }

    #[test]
    fn test_build_morph_tx_receipt_with_fields() {
        let builder = DefaultMorphReceiptBuilder;
        let l1_fee = U256::from(3000u64);

        let fields = MorphReceiptTxFields {
            version: 0,
            fee_token_id: 1,
            fee_rate: Some(U256::from(2_000_000_000u64)),
            token_scale: Some(U256::from(10u64).pow(U256::from(18u64))),
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
        };

        let ctx = MorphReceiptBuilderCtx {
            tx_type: MorphTxType::Morph,
            result: make_success_result(21000, l1_fee),
            cumulative_gas_used: 21000,
            morph_tx_fields: Some(fields),
        };

        let receipt = builder.build_receipt(ctx);
        assert_eq!(receipt.l1_fee(), l1_fee);

        // Destructure the Morph variant and verify all MorphTx-specific fields
        let MorphReceipt::Morph(morph_receipt) = &receipt else {
            panic!("expected MorphReceipt::Morph, got {:?}", receipt.tx_type());
        };
        assert_eq!(morph_receipt.version, Some(0));
        assert_eq!(morph_receipt.fee_token_id, Some(1));
        assert_eq!(morph_receipt.fee_rate, Some(U256::from(2_000_000_000u64)));
        assert_eq!(
            morph_receipt.token_scale,
            Some(U256::from(10u64).pow(U256::from(18u64)))
        );
        assert_eq!(morph_receipt.fee_limit, Some(U256::from(1000u64)));
        assert_eq!(morph_receipt.reference, None);
        assert_eq!(morph_receipt.memo, None);
    }

    #[test]
    fn test_build_morph_tx_receipt_without_fields_fallback() {
        let builder = DefaultMorphReceiptBuilder;
        let l1_fee = U256::from(3000u64);

        // Missing morph_tx_fields => should fallback to with_l1_fee
        let ctx = MorphReceiptBuilderCtx {
            tx_type: MorphTxType::Morph,
            result: make_success_result(21000, l1_fee),
            cumulative_gas_used: 21000,
            morph_tx_fields: None,
        };

        let receipt = builder.build_receipt(ctx);
        // Should still be MorphReceipt::Morph variant, just without token fields
        assert_eq!(receipt.l1_fee(), l1_fee);

        // Destructure and verify fields are None (fallback path uses with_l1_fee)
        let MorphReceipt::Morph(morph_receipt) = &receipt else {
            panic!("expected MorphReceipt::Morph, got {:?}", receipt.tx_type());
        };
        assert_eq!(morph_receipt.l1_fee, l1_fee);
        assert_eq!(morph_receipt.version, None);
        assert_eq!(morph_receipt.fee_token_id, None);
        assert_eq!(morph_receipt.fee_rate, None);
        assert_eq!(morph_receipt.token_scale, None);
        assert_eq!(morph_receipt.fee_limit, None);
        assert_eq!(morph_receipt.reference, None);
        assert_eq!(morph_receipt.memo, None);
    }

    #[test]
    fn test_build_receipt_reverted_tx() {
        let builder = DefaultMorphReceiptBuilder;

        let ctx = MorphReceiptBuilderCtx {
            tx_type: MorphTxType::Legacy,
            result: make_result(false, 15000, vec![], U256::from(100u64)),
            cumulative_gas_used: 15000,
            morph_tx_fields: None,
        };

        let receipt = builder.build_receipt(ctx);
        assert!(
            !TxReceipt::status(&receipt),
            "reverted tx should have status=false"
        );
    }

    #[test]
    fn test_build_receipt_with_logs() {
        let builder = DefaultMorphReceiptBuilder;
        let log = Log::new(
            Address::repeat_byte(0x01),
            vec![B256::repeat_byte(0x02)],
            alloy_primitives::Bytes::from_static(b"log-data"),
        )
        .unwrap();

        let ctx = MorphReceiptBuilderCtx {
            tx_type: MorphTxType::Legacy,
            result: make_result(true, 21000, vec![log], U256::ZERO),
            cumulative_gas_used: 21000,
            morph_tx_fields: None,
        };

        let receipt = builder.build_receipt(ctx);
        assert_eq!(TxReceipt::logs(&receipt).len(), 1);
    }

    /// A reverted token-fee transaction still reports the fee transfers the EVM kept.
    #[test]
    fn test_build_receipt_keeps_logs_of_a_failed_transaction() {
        let builder = DefaultMorphReceiptBuilder;
        let log = Log::new(Address::repeat_byte(0x01), vec![], Bytes::new()).unwrap();

        let ctx = MorphReceiptBuilderCtx {
            tx_type: MorphTxType::Morph,
            result: make_result(false, 15000, vec![log.clone(), log], U256::ZERO),
            cumulative_gas_used: 15000,
            morph_tx_fields: None,
        };

        let receipt = builder.build_receipt(ctx);
        assert!(!TxReceipt::status(&receipt));
        assert_eq!(TxReceipt::logs(&receipt).len(), 2);
    }
}
