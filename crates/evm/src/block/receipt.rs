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
use alloy_primitives::{B256, Bytes, Log, U256};
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
/// - `morph_tx_fields`: MorphTx-specific fields (token fee info, version, reference, memo)
/// - `pre_fee_logs`: Transfer event logs from token fee deduction (survives tx revert)
/// - `post_fee_logs`: Transfer event logs from token fee reimbursement
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
    pub morph_tx_fields: Option<MorphReceiptTxFields>,
    /// Transfer event logs from token fee deduction (before main tx execution).
    /// Managed separately from the handler pipeline to survive main tx revert.
    pub pre_fee_logs: Vec<Log>,
    /// Transfer event logs from token fee reimbursement (after main tx execution).
    pub post_fee_logs: Vec<Log>,
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
            pre_fee_logs,
            post_fee_logs,
        } = ctx;

        // Assemble logs in chronological order matching go-ethereum:
        //   [deduct Transfer] + [main tx logs] + [refund Transfer]
        // Fee logs are cached separately from the journal so they survive
        // main tx revert (revm's ExecutionResult::Revert carries no logs).
        let is_success = result.is_success();
        let main_logs = result.into_logs();
        let mut logs =
            Vec::with_capacity(pre_fee_logs.len() + main_logs.len() + post_fee_logs.len());
        logs.extend(pre_fee_logs);
        logs.extend(main_logs);
        logs.extend(post_fee_logs);

        let inner = Receipt {
            status: is_success.into(),
            cumulative_gas_used,
            logs,
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
                        target: "morph::evm",
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy, TxReceipt};
    use alloy_primitives::{Address, Log, Signature, TxKind};
    use morph_primitives::transaction::TxL1Msg;
    use revm::context::result::ExecutionResult;

    // We use NoOpInspector-based MorphEvm for the generic E parameter.
    // Since build_receipt only uses E::HaltReason, we can use any concrete Evm type.
    type TestEvm = crate::evm::MorphEvm<revm::database::EmptyDB>;

    fn make_success_result(gas_used: u64) -> ExecutionResult<morph_revm::MorphHaltReason> {
        ExecutionResult::Success {
            reason: revm::context::result::SuccessReason::Stop,
            gas_used,
            gas_refunded: 0,
            logs: vec![],
            output: revm::context::result::Output::Call(alloy_primitives::Bytes::new()),
        }
    }

    fn make_success_with_logs(
        gas_used: u64,
        logs: Vec<Log>,
    ) -> ExecutionResult<morph_revm::MorphHaltReason> {
        ExecutionResult::Success {
            reason: revm::context::result::SuccessReason::Stop,
            gas_used,
            gas_refunded: 0,
            logs,
            output: revm::context::result::Output::Call(alloy_primitives::Bytes::new()),
        }
    }

    fn make_revert_result(gas_used: u64) -> ExecutionResult<morph_revm::MorphHaltReason> {
        ExecutionResult::Revert {
            gas_used,
            output: alloy_primitives::Bytes::new(),
        }
    }

    fn create_legacy_tx() -> MorphTxEnvelope {
        let tx = TxLegacy {
            chain_id: Some(1337),
            nonce: 0,
            gas_price: 1_000_000_000,
            gas_limit: 21000,
            to: TxKind::Call(Address::repeat_byte(0x01)),
            value: U256::ZERO,
            input: alloy_primitives::Bytes::new(),
        };
        MorphTxEnvelope::Legacy(Signed::new_unhashed(tx, Signature::test_signature()))
    }

    fn create_eip1559_tx() -> MorphTxEnvelope {
        use alloy_consensus::TxEip1559;
        let tx = TxEip1559 {
            chain_id: 1337,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(Address::repeat_byte(0x02)),
            value: U256::ZERO,
            input: alloy_primitives::Bytes::new(),
            access_list: Default::default(),
        };
        MorphTxEnvelope::Eip1559(Signed::new_unhashed(tx, Signature::test_signature()))
    }

    fn create_l1_msg_tx() -> MorphTxEnvelope {
        use alloy_consensus::Sealed;
        let tx = TxL1Msg {
            queue_index: 0,
            gas_limit: 21000,
            to: Address::ZERO,
            value: U256::ZERO,
            input: alloy_primitives::Bytes::default(),
            sender: Address::ZERO,
        };
        MorphTxEnvelope::L1Msg(Sealed::new(tx))
    }

    fn create_morph_tx() -> MorphTxEnvelope {
        use morph_primitives::TxMorph;
        use morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_0;
        let tx = TxMorph {
            chain_id: 1337,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(Address::repeat_byte(0x03)),
            value: U256::ZERO,
            access_list: Default::default(),
            version: MORPH_TX_VERSION_0,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
            input: alloy_primitives::Bytes::new(),
        };
        MorphTxEnvelope::Morph(Signed::new_unhashed(tx, Signature::test_signature()))
    }

    #[test]
    fn test_build_legacy_receipt() {
        let builder = DefaultMorphReceiptBuilder;
        let tx = create_legacy_tx();
        let l1_fee = U256::from(5000u64);

        let ctx = MorphReceiptBuilderCtx::<TestEvm> {
            tx: &tx,
            result: make_success_result(21000),
            cumulative_gas_used: 21000,
            l1_fee,
            morph_tx_fields: None,
            pre_fee_logs: vec![],
            post_fee_logs: vec![],
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
        let tx = create_eip1559_tx();
        let l1_fee = U256::from(8000u64);

        let ctx = MorphReceiptBuilderCtx::<TestEvm> {
            tx: &tx,
            result: make_success_result(21000),
            cumulative_gas_used: 42000,
            l1_fee,
            morph_tx_fields: None,
            pre_fee_logs: vec![],
            post_fee_logs: vec![],
        };

        let receipt = builder.build_receipt(ctx);
        assert!(matches!(receipt, MorphReceipt::Eip1559(_)));
        assert_eq!(receipt.l1_fee(), l1_fee);
        assert_eq!(receipt.cumulative_gas_used(), 42000);
    }

    #[test]
    fn test_build_l1_msg_receipt_no_l1_fee() {
        let builder = DefaultMorphReceiptBuilder;
        let tx = create_l1_msg_tx();

        let ctx = MorphReceiptBuilderCtx::<TestEvm> {
            tx: &tx,
            result: make_success_result(21000),
            cumulative_gas_used: 21000,
            // Pass a non-zero l1_fee to verify the builder ignores it for L1 messages.
            // L1 message gas is prepaid on L1, so no L1 fee should appear in the receipt.
            l1_fee: U256::from(999_999),
            morph_tx_fields: None,
            pre_fee_logs: vec![],
            post_fee_logs: vec![],
        };

        let receipt = builder.build_receipt(ctx);
        assert!(matches!(receipt, MorphReceipt::L1Msg(_)));
        // L1 messages return ZERO for l1_fee regardless of what was passed in
        assert_eq!(receipt.l1_fee(), U256::ZERO);
    }

    #[test]
    fn test_build_morph_tx_receipt_with_fields() {
        let builder = DefaultMorphReceiptBuilder;
        let tx = create_morph_tx();
        let l1_fee = U256::from(3000u64);

        let fields = MorphReceiptTxFields {
            version: 0,
            fee_token_id: 1,
            fee_rate: U256::from(2_000_000_000u64),
            token_scale: U256::from(10u64).pow(U256::from(18u64)),
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
        };

        let ctx = MorphReceiptBuilderCtx::<TestEvm> {
            tx: &tx,
            result: make_success_result(21000),
            cumulative_gas_used: 21000,
            l1_fee,
            morph_tx_fields: Some(fields),
            pre_fee_logs: vec![],
            post_fee_logs: vec![],
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
        let tx = create_morph_tx();
        let l1_fee = U256::from(3000u64);

        // Missing morph_tx_fields => should fallback to with_l1_fee
        let ctx = MorphReceiptBuilderCtx::<TestEvm> {
            tx: &tx,
            result: make_success_result(21000),
            cumulative_gas_used: 21000,
            l1_fee,
            morph_tx_fields: None,
            pre_fee_logs: vec![],
            post_fee_logs: vec![],
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
        let tx = create_legacy_tx();

        let ctx = MorphReceiptBuilderCtx::<TestEvm> {
            tx: &tx,
            result: make_revert_result(15000),
            cumulative_gas_used: 15000,
            l1_fee: U256::from(100u64),
            morph_tx_fields: None,
            pre_fee_logs: vec![],
            post_fee_logs: vec![],
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
        let tx = create_legacy_tx();
        let log = Log::new(
            Address::repeat_byte(0x01),
            vec![B256::repeat_byte(0x02)],
            alloy_primitives::Bytes::from_static(b"log-data"),
        )
        .unwrap();

        let ctx = MorphReceiptBuilderCtx::<TestEvm> {
            tx: &tx,
            result: make_success_with_logs(21000, vec![log]),
            cumulative_gas_used: 21000,
            l1_fee: U256::ZERO,
            morph_tx_fields: None,
            pre_fee_logs: vec![],
            post_fee_logs: vec![],
        };

        let receipt = builder.build_receipt(ctx);
        assert_eq!(TxReceipt::logs(&receipt).len(), 1);
    }

    fn make_fee_log(marker: u8) -> Log {
        Log::new(
            Address::repeat_byte(marker),
            vec![B256::repeat_byte(marker)],
            alloy_primitives::Bytes::new(),
        )
        .unwrap()
    }

    /// Fee Transfer logs (pre/post) survive when the main transaction reverts.
    ///
    /// go-ethereum's StateDB.logs is independent of snapshot/revert — fee logs
    /// are always included. revm's ExecutionResult::Revert carries no logs field,
    /// so morph-reth caches fee logs in pre_fee_logs/post_fee_logs and merges
    /// them unconditionally in the receipt builder.
    #[test]
    fn test_fee_logs_survive_main_tx_revert() {
        let builder = DefaultMorphReceiptBuilder;
        let tx = create_legacy_tx();

        let pre_log = make_fee_log(0xAA); // fee deduction Transfer
        let post_log = make_fee_log(0xBB); // fee refund Transfer

        let ctx = MorphReceiptBuilderCtx::<TestEvm> {
            tx: &tx,
            result: make_revert_result(20_000),
            cumulative_gas_used: 20_000,
            l1_fee: U256::ZERO,
            morph_tx_fields: None,
            pre_fee_logs: vec![pre_log.clone()],
            post_fee_logs: vec![post_log.clone()],
        };

        let receipt = builder.build_receipt(ctx);

        assert!(
            !TxReceipt::status(&receipt),
            "reverted tx must have status=false"
        );

        let logs = TxReceipt::logs(&receipt);
        // Main tx logs are absent (revert), but fee logs must still be present.
        assert_eq!(
            logs.len(),
            2,
            "pre_fee_log + post_fee_log must appear despite revert"
        );
        assert_eq!(
            logs[0].address, pre_log.address,
            "first log must be pre_fee_log"
        );
        assert_eq!(
            logs[1].address, post_log.address,
            "second log must be post_fee_log"
        );
    }

    /// Log ordering on successful tx: [pre_fee_log, main_tx_log, post_fee_log].
    ///
    /// Matches go-ethereum's receipt log ordering where fee deduction comes
    /// first (before main tx), and fee refund comes last (after main tx).
    #[test]
    fn test_fee_log_ordering_on_success() {
        let builder = DefaultMorphReceiptBuilder;
        let tx = create_legacy_tx();

        let pre_log = make_fee_log(0xAA);
        let main_log = make_fee_log(0xCC);
        let post_log = make_fee_log(0xBB);

        let ctx = MorphReceiptBuilderCtx::<TestEvm> {
            tx: &tx,
            result: make_success_with_logs(21_000, vec![main_log.clone()]),
            cumulative_gas_used: 21_000,
            l1_fee: U256::ZERO,
            morph_tx_fields: None,
            pre_fee_logs: vec![pre_log.clone()],
            post_fee_logs: vec![post_log.clone()],
        };

        let receipt = builder.build_receipt(ctx);
        assert!(TxReceipt::status(&receipt));

        let logs = TxReceipt::logs(&receipt);
        assert_eq!(logs.len(), 3, "pre_fee + main + post_fee = 3 logs");
        assert_eq!(
            logs[0].address, pre_log.address,
            "pre_fee_log must be first"
        );
        assert_eq!(
            logs[1].address, main_log.address,
            "main_tx_log must be second"
        );
        assert_eq!(
            logs[2].address, post_log.address,
            "post_fee_log must be last"
        );
    }

    /// Fee logs without refund: only pre_fee_log when no gas is refunded.
    ///
    /// If all gas is consumed exactly (no unused gas), the post_fee_log
    /// may be empty. But the pre_fee_log must always appear.
    #[test]
    fn test_pre_fee_log_only_no_post_fee() {
        let builder = DefaultMorphReceiptBuilder;
        let tx = create_legacy_tx();

        let pre_log = make_fee_log(0xAA);

        let ctx = MorphReceiptBuilderCtx::<TestEvm> {
            tx: &tx,
            result: make_revert_result(21_000),
            cumulative_gas_used: 21_000,
            l1_fee: U256::ZERO,
            morph_tx_fields: None,
            pre_fee_logs: vec![pre_log.clone()],
            post_fee_logs: vec![], // no refund
        };

        let receipt = builder.build_receipt(ctx);
        assert!(!TxReceipt::status(&receipt));

        let logs = TxReceipt::logs(&receipt);
        assert_eq!(logs.len(), 1, "only pre_fee_log when there is no refund");
        assert_eq!(logs[0].address, pre_log.address);
    }
}
