//! Morph L2 consensus validation.
//!
//! This module provides consensus validation for Morph L2 blocks, implementing
//! reth's `Consensus`, `HeaderValidator`, and `FullConsensus` traits.
//!
//! # Validation Rules
//!
//! ## Header Validation
//!
//! - Extra data must be empty (Morph L2 specific)
//! - Nonce must be 0 (post-merge)
//! - Ommers hash must be empty (post-merge)
//! - Difficulty must be 0 (post-merge)
//! - Coinbase must be zero when FeeVault is enabled
//! - Timestamp cannot be in the future
//! - Gas limit must be within bounds
//! - Base fee must always be set (EIP-1559 is always active)
//!
//! ## L1 Message Rules
//!
//! - All L1 messages must be at the beginning of the block
//! - Within a block, L1 messages must have strictly sequential `queue_index`
//! - Cross-block gaps are allowed (the sequencer may skip queue indices)
//!
//! ## Block Body Validation
//!
//! - No uncle blocks allowed
//! - Withdrawals field must not be present
//! - Transaction root must be valid
//!
//! ## Post-Execution Validation
//!
//! - Gas used must match cumulative gas from receipts
//! - Receipts root must be valid
//! - Logs bloom must be valid
//!
use crate::MorphConsensusError;
use alloy_consensus::{BlockHeader as _, EMPTY_OMMER_ROOT_HASH, TxReceipt};
use alloy_evm::block::BlockExecutionResult;
use alloy_primitives::{B256, Bloom};
use morph_chainspec::{MorphChainSpec, MorphHardforks};
use morph_primitives::{
    Block, BlockBody, MorphHeader, MorphReceipt, MorphTxEnvelope,
    transaction::morph_transaction::MORPH_TX_VERSION_1,
};
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator};
use reth_consensus_common::validation::{
    validate_against_parent_hash_number, validate_body_against_header,
};
use reth_primitives_traits::{
    BlockBody as BlockBodyTrait, BlockHeader, GotExpected, RecoveredBlock, SealedBlock,
    SealedHeader,
};
use std::sync::Arc;

// ============================================================================
// Constants
// ============================================================================

/// Maximum allowed base fee (10 Gwei)
const MORPH_MAXIMUM_BASE_FEE: u64 = 10_000_000_000;

/// Maximum gas limit (2^63 - 1)
const MAX_GAS_LIMIT: u64 = 0x7fffffffffffffff;

/// Minimum gas limit allowed for transactions.
const MINIMUM_GAS_LIMIT: u64 = 5000;

/// The bound divisor of the gas limit, used in update calculations.
const GAS_LIMIT_BOUND_DIVISOR: u64 = 1024;

// ============================================================================
// MorphConsensus
// ============================================================================

/// Morph L2 consensus engine.
///
/// Validates Morph L2 blocks according to the L2 consensus rules.
/// See module-level documentation for detailed validation rules.
///
/// # L1 Message Validation Architecture
///
/// L1 message ordering requires both body data (transactions) and parent header data.
/// Since reth's `Consensus` trait methods provide these separately — `validate_block_pre_execution`
/// has the block body but not the parent header, while `validate_header_against_parent` has
/// both headers but not the body — the validation is split into two independent checks:
///
/// 1. **Internal consistency** (`validate_block_pre_execution`): L1 messages are at the block
///    start, have sequential queue indices, and are consistent with `header.next_l1_msg_index`.
/// 2. **Cross-block monotonicity** (`validate_header_against_parent`): `header.next_l1_msg_index`
///    is monotonically non-decreasing relative to the parent.
///
/// These two methods have no ordering dependency and share no mutable state. The strict
/// cross-block equality check (`header.next == parent.next + l1_count`) requires simultaneous
/// access to both parent header and block body, which reth's trait API does not provide in
/// any single method. In Morph's single-sequencer model, the remaining gap (queue index
/// skipping) is prevented by the trusted sequencer and verified by the L1 message queue
/// contract.
#[derive(Debug, Clone)]
pub struct MorphConsensus {
    /// Chain specification containing hardfork information and chain config.
    chain_spec: Arc<MorphChainSpec>,
}

impl MorphConsensus {
    /// Creates a new [`MorphConsensus`] instance.
    pub fn new(chain_spec: Arc<MorphChainSpec>) -> Self {
        Self { chain_spec }
    }

    /// Returns a reference to the chain specification.
    pub fn chain_spec(&self) -> &MorphChainSpec {
        &self.chain_spec
    }
}

// ============================================================================
// HeaderValidator Implementation
// ============================================================================

impl HeaderValidator<MorphHeader> for MorphConsensus {
    /// Validates a block header according to Morph L2 consensus rules.
    ///
    /// # Validation Steps
    ///
    /// 1. **Extra Data**: Must be empty (Morph L2 specific)
    /// 2. **Nonce**: Must be 0 (post-merge Ethereum)
    /// 3. **Ommers Hash**: Must be empty ommer root hash (post-merge)
    /// 4. **Difficulty**: Must be 0 (post-merge)
    /// 5. **Coinbase**: Must be zero address if FeeVault is enabled
    /// 6. **Timestamp**: Must not be in the future
    /// 7. **Gas Limit**: Must be <= MAX_GAS_LIMIT
    /// 8. **Gas Used**: Must be <= gas limit
    /// 9. **Base Fee**: Must always be set (EIP-1559 is always active) and <= 10 Gwei
    fn validate_header(&self, header: &SealedHeader<MorphHeader>) -> Result<(), ConsensusError> {
        // Extra data must be empty (Morph L2 specific - stricter than max length)
        if !header.extra_data().is_empty() {
            return Err(ConsensusError::ExtraDataExceedsMax {
                len: header.extra_data().len(),
            });
        }

        // Nonce must be 0 (same as post-merge Ethereum)
        if !header.nonce().is_some_and(|nonce| nonce.is_zero()) {
            return Err(ConsensusError::TheMergeNonceIsNotZero);
        }

        // Ommers hash must be empty (same as post-merge Ethereum)
        if header.ommers_hash() != EMPTY_OMMER_ROOT_HASH {
            return Err(ConsensusError::TheMergeOmmerRootIsNotEmpty);
        }

        // Difficulty must be 0 (same as post-merge Ethereum)
        if !header.difficulty().is_zero() {
            return Err(ConsensusError::TheMergeDifficultyIsNotZero);
        }

        // Coinbase must be zero if FeeVault is enabled (Morph L2 specific)
        if self.chain_spec.is_fee_vault_enabled()
            && header.beneficiary() != alloy_primitives::Address::ZERO
        {
            return Err(ConsensusError::Other(
                MorphConsensusError::InvalidCoinbase(header.beneficiary()).to_string(),
            ));
        }

        // Check timestamp is not in the future
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("system time should never be before UNIX EPOCH")
            .as_secs();

        if header.timestamp() > now {
            return Err(ConsensusError::TimestampIsInFuture {
                timestamp: header.timestamp(),
                present_timestamp: now,
            });
        }

        // Gas limit must be <= MAX_GAS_LIMIT
        if header.gas_limit() > MAX_GAS_LIMIT {
            return Err(ConsensusError::HeaderGasLimitExceedsMax {
                gas_limit: header.gas_limit(),
            });
        }

        // Gas used must be <= gas limit
        if header.gas_used() > header.gas_limit() {
            return Err(ConsensusError::HeaderGasUsedExceedsGasLimit {
                gas_used: header.gas_used(),
                gas_limit: header.gas_limit(),
            });
        }

        // Validate base fee (always required, EIP-1559 is always active)
        let base_fee = header
            .base_fee_per_gas()
            .ok_or(ConsensusError::BaseFeeMissing)?;
        if base_fee > MORPH_MAXIMUM_BASE_FEE {
            return Err(ConsensusError::Other(
                MorphConsensusError::BaseFeeOverLimit(base_fee).to_string(),
            ));
        }
        Ok(())
    }

    /// Validates a block header against its parent header.
    ///
    /// # Validation Steps
    ///
    /// 1. **Parent Hash**: Header's parent_hash must match parent's hash
    /// 2. **Block Number**: Header's number must be parent's number + 1
    /// 3. **Timestamp**: Header's timestamp must be >= parent's timestamp
    /// 4. **Gas Limit**: Change must be within 1/1024 of parent's limit
    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<MorphHeader>,
        parent: &SealedHeader<MorphHeader>,
    ) -> Result<(), ConsensusError> {
        // Validate parent hash and block number
        validate_against_parent_hash_number(header.header(), parent)?;

        // Validate timestamp against parent (pre-Emerald: strict >, post-Emerald: >=)
        let is_emerald = self
            .chain_spec
            .is_emerald_active_at_timestamp(header.timestamp());
        validate_against_parent_timestamp(header.header(), parent.header(), is_emerald)?;

        // Validate gas limit change
        validate_against_parent_gas_limit(header.header(), parent.header())?;

        // Cross-block L1 message index monotonicity: next_l1_msg_index must never
        // decrease across blocks. This is the header-only half of L1 message
        // validation; the body-level half is in validate_block_pre_execution.
        if header.next_l1_msg_index < parent.next_l1_msg_index {
            return Err(ConsensusError::Other(
                MorphConsensusError::InvalidNextL1MessageIndex {
                    expected: parent.next_l1_msg_index,
                    actual: header.next_l1_msg_index,
                }
                .to_string(),
            ));
        }

        Ok(())
    }
}

// ============================================================================
// Consensus Implementation
// ============================================================================

impl Consensus<Block> for MorphConsensus {
    /// Validates the block body against the header.
    ///
    /// Checks that the body's computed transaction root matches the header's.
    fn validate_body_against_header(
        &self,
        body: &BlockBody,
        header: &SealedHeader<MorphHeader>,
    ) -> Result<(), ConsensusError> {
        validate_body_against_header(body, header.header())
    }

    /// Validates the block before execution.
    ///
    /// # Validation Steps
    ///
    /// 1. **No Uncle Blocks**: Morph L2 doesn't support uncle blocks
    /// 2. **Ommers Hash**: Must be the empty ommer root hash
    /// 3. **Transaction Root**: Must be valid
    /// 4. **Withdrawals**: Must be empty (Morph L2 doesn't support withdrawals)
    /// 5. **L1 Messages**: Must be ordered correctly (sequential queue indices, L1 before L2)
    fn validate_block_pre_execution(
        &self,
        block: &SealedBlock<Block>,
    ) -> Result<(), ConsensusError> {
        // Check no uncles allowed (Morph L2 has no uncle blocks)
        let ommers_len = block.body().ommers().map(|o| o.len()).unwrap_or_default();
        if ommers_len > 0 {
            return Err(ConsensusError::Other("uncles not allowed".to_string()));
        }

        // Check ommers hash must be empty root hash
        if block.ommers_hash() != EMPTY_OMMER_ROOT_HASH {
            return Err(ConsensusError::BodyOmmersHashDiff(
                GotExpected {
                    got: block.ommers_hash(),
                    expected: EMPTY_OMMER_ROOT_HASH,
                }
                .into(),
            ));
        }

        // Check transaction root
        if let Err(error) = block.ensure_transaction_root_valid() {
            return Err(ConsensusError::BodyTransactionRootDiff(error.into()));
        }

        // Check withdrawals are empty
        if block.body().withdrawals().is_some() {
            return Err(ConsensusError::Other(
                MorphConsensusError::WithdrawalsNonEmpty.to_string(),
            ));
        }

        // Validate MorphTx version and field constraints.
        // Matches go-ethereum's BlockValidator.ValidateBody() → ValidateMorphTxVersion().
        let is_jade = self
            .chain_spec
            .is_jade_active_at_timestamp(block.header().timestamp());
        validate_morph_txs(&block.body().transactions, is_jade)?;

        // Validate L1 messages ordering and internal consistency with header.
        // This is the body-level half of L1 validation; it verifies that the L1
        // messages within this block are internally consistent with the header's
        // next_l1_msg_index. The cross-block monotonicity check (ensuring
        // next_l1_msg_index >= parent's value) is in validate_header_against_parent.
        validate_l1_messages_in_block(
            &block.body().transactions,
            block.header().next_l1_msg_index,
        )?;

        Ok(())
    }
}

// ============================================================================
// FullConsensus Implementation
// ============================================================================

impl FullConsensus<morph_primitives::MorphPrimitives> for MorphConsensus {
    /// Validates the block after execution.
    ///
    /// This is called after all transactions have been executed and compares
    /// the execution results against the block header.
    ///
    /// # Validation Steps
    ///
    /// 1. **Gas Used**: The cumulative gas used from the last receipt must match
    ///    the header's `gas_used` field.
    /// 2. **Receipts Root**: The computed receipts root must match the header's.
    /// 3. **Logs Bloom**: The combined bloom filter of all receipts must match
    ///    the header's `logs_bloom` field.
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<Block>,
        result: &BlockExecutionResult<MorphReceipt>,
    ) -> Result<(), ConsensusError> {
        // Verify the block gas used
        let cumulative_gas_used = result
            .receipts
            .last()
            .map(|r| r.cumulative_gas_used())
            .unwrap_or(0);

        if block.gas_used() != cumulative_gas_used {
            return Err(ConsensusError::BlockGasUsed {
                gas: GotExpected {
                    got: cumulative_gas_used,
                    expected: block.gas_used(),
                },
                gas_spent_by_tx: reth_primitives_traits::receipt::gas_spent_by_transactions(
                    &result.receipts,
                ),
            });
        }

        // Verify the receipts logs bloom and root
        verify_receipts(block.receipts_root(), block.logs_bloom(), &result.receipts)?;

        Ok(())
    }
}

/// Validates that the header's timestamp is valid relative to the parent's timestamp.
///
/// # Hardfork Behavior
///
/// - **Pre-Emerald**: Timestamp must be strictly greater than parent's timestamp.
/// - **Post-Emerald**: Timestamp must be greater than or equal to parent's timestamp.
///
/// This matches go-ethereum's `consensus/l2/consensus.go:155-157`.
///
/// # Errors
///
/// Returns [`ConsensusError::TimestampIsInPast`] if the header's timestamp
/// violates the hardfork-specific constraint.
#[inline]
fn validate_against_parent_timestamp<H: BlockHeader>(
    header: &H,
    parent: &H,
    is_emerald: bool,
) -> Result<(), ConsensusError> {
    if header.timestamp() < parent.timestamp()
        || (header.timestamp() == parent.timestamp() && !is_emerald)
    {
        return Err(ConsensusError::TimestampIsInPast {
            parent_timestamp: parent.timestamp(),
            timestamp: header.timestamp(),
        });
    }
    Ok(())
}

/// Validates gas limit change against parent.
///
/// The gas limit change between consecutive blocks must be strictly less than
/// `parent_gas_limit / GAS_LIMIT_BOUND_DIVISOR` (1/1024 of parent's limit).
///
/// Additionally, the gas limit must be at least [`MINIMUM_GAS_LIMIT`] (5000).
///
/// # Errors
///
/// - [`ConsensusError::GasLimitInvalidIncrease`] if gas limit increased too much
/// - [`ConsensusError::GasLimitInvalidDecrease`] if gas limit decreased too much
/// - [`ConsensusError::GasLimitInvalidMinimum`] if gas limit is below minimum
#[inline]
fn validate_against_parent_gas_limit<H: BlockHeader>(
    header: &H,
    parent: &H,
) -> Result<(), ConsensusError> {
    let diff = header.gas_limit().abs_diff(parent.gas_limit());
    let limit = parent.gas_limit() / GAS_LIMIT_BOUND_DIVISOR;
    if diff >= limit {
        return if header.gas_limit() > parent.gas_limit() {
            Err(ConsensusError::GasLimitInvalidIncrease {
                parent_gas_limit: parent.gas_limit(),
                child_gas_limit: header.gas_limit(),
            })
        } else {
            Err(ConsensusError::GasLimitInvalidDecrease {
                parent_gas_limit: parent.gas_limit(),
                child_gas_limit: header.gas_limit(),
            })
        };
    }
    // Check that the gas limit is above the minimum allowed gas limit.
    if header.gas_limit() < MINIMUM_GAS_LIMIT {
        return Err(ConsensusError::GasLimitInvalidMinimum {
            child_gas_limit: header.gas_limit(),
        });
    }

    Ok(())
}

// ============================================================================
// L1 Message Validation
// ============================================================================

/// Validates L1 message ordering and internal consistency within a single block.
///
/// This is a **stateless** validation that uses only the block's own transactions
/// and header — it does not require the parent header or any shared mutable state.
///
/// # Checks Performed
///
/// 1. **Position**: All L1 messages must appear at the beginning of the block.
///    Once a regular (L2) transaction appears, no more L1 messages are allowed.
///
/// 2. **Sequential Queue Index**: L1 messages must have strictly sequential
///    `queue_index` values (each = previous + 1).
///
/// 3. **Header Consistency**: If L1 messages are present,
///    `header.next_l1_msg_index` must be >= `last_queue_index + 1`. It may be
///    strictly greater because Morph allows L1 messages to be "skipped" — the
///    sequencer can advance past queue indices not included in the block body.
///
/// # Cross-Block Validation
///
/// The cross-block check (ensuring `next_l1_msg_index >= parent.next_l1_msg_index`)
/// is performed separately in `validate_header_against_parent`, which has access to
/// the parent header. See the [`MorphConsensus`] doc comment for the full architecture.
///
/// # Example (Valid)
///
/// ```text
/// [L1Msg(queue=5), L1Msg(queue=6), L1Msg(queue=7), RegularTx]
/// // header.next_l1_msg_index = 8  ✓ (exact match)
/// // header.next_l1_msg_index = 10 ✓ (skipped queue indices 8, 9)
/// ```
///
/// # Example (Invalid - L1 after L2)
///
/// ```text
/// [L1Msg(queue=0), RegularTx, L1Msg(queue=1)]  // ❌ L1 after L2
/// ```
#[inline]
fn validate_l1_messages_in_block(
    txs: &[MorphTxEnvelope],
    header_next_l1_msg_index: u64,
) -> Result<(), ConsensusError> {
    let mut l1_msg_count = 0u64;
    let mut saw_l2_transaction = false;
    let mut prev_queue_index: Option<u64> = None;

    for tx in txs {
        if tx.is_l1_msg() {
            // Check L1 messages are only at the start of the block (before any L2 tx)
            if saw_l2_transaction {
                return Err(ConsensusError::Other(
                    MorphConsensusError::InvalidL1MessageOrder.to_string(),
                ));
            }

            let tx_queue_index = tx.queue_index().ok_or_else(|| {
                ConsensusError::Other(MorphConsensusError::MalformedL1Message.to_string())
            })?;

            // Check queue indices are strictly sequential (each = previous + 1).
            // Use checked_add to prevent overflow at u64::MAX.
            if let Some(prev) = prev_queue_index {
                let expected = prev.checked_add(1).ok_or_else(|| {
                    ConsensusError::Other(
                        MorphConsensusError::L1MessagesNotInOrder {
                            expected: u64::MAX,
                            actual: tx_queue_index,
                        }
                        .to_string(),
                    )
                })?;
                if tx_queue_index != expected {
                    return Err(ConsensusError::Other(
                        MorphConsensusError::L1MessagesNotInOrder {
                            expected,
                            actual: tx_queue_index,
                        }
                        .to_string(),
                    ));
                }
            }

            prev_queue_index = Some(tx_queue_index);
            l1_msg_count += 1;
        } else {
            saw_l2_transaction = true;
        }
    }

    // Validate header consistency: header.next_l1_msg_index must be at least
    // last_queue_index + 1 (cannot go backwards relative to included messages).
    // It may be strictly greater because Morph allows L1 messages to be
    // "skipped" — the sequencer can advance past queue indices that are not
    // included in the block body (e.g., messages that failed on L1 relay).
    // go-eth's NumL1MessagesProcessed() comment: "This count includes both
    // skipped and included messages."
    // For blocks with no L1 messages, this check is skipped — the cross-block
    // monotonicity check in validate_header_against_parent handles that case.
    if l1_msg_count > 0 {
        let last_queue_index = prev_queue_index.ok_or_else(|| {
            ConsensusError::Other(
                "internal error: l1_msg_count > 0 but prev_queue_index is None".to_string(),
            )
        })?;
        let min_expected = last_queue_index.checked_add(1).ok_or_else(|| {
            ConsensusError::Other(
                MorphConsensusError::InvalidNextL1MessageIndex {
                    expected: u64::MAX,
                    actual: header_next_l1_msg_index,
                }
                .to_string(),
            )
        })?;
        if header_next_l1_msg_index < min_expected {
            return Err(ConsensusError::Other(
                MorphConsensusError::InvalidNextL1MessageIndex {
                    expected: min_expected,
                    actual: header_next_l1_msg_index,
                }
                .to_string(),
            ));
        }
    }

    Ok(())
}

/// Validates all MorphTx (0x7F) transactions in a block.
///
/// Performs two checks per MorphTx:
/// 1. **Hardfork gate**: rejects V1 transactions before the Jade fork is active
/// 2. **Field validation**: delegates to [`TxMorph::validate()`] for version-specific
///    field constraints, memo length, and gas price ordering
///
/// See [`TxMorph::validate()`] for the detailed per-version rules.
fn validate_morph_txs(txs: &[MorphTxEnvelope], is_jade: bool) -> Result<(), ConsensusError> {
    for tx in txs {
        let morph_tx = match tx {
            MorphTxEnvelope::Morph(signed) => signed.tx(),
            _ => continue,
        };

        // Reject MorphTx V1 before Jade fork (hardfork-gated, consensus-only check).
        if !is_jade && morph_tx.version == MORPH_TX_VERSION_1 {
            return Err(ConsensusError::Other(
                MorphConsensusError::InvalidBody(
                    "MorphTx version 1 is not yet active (jade fork not reached)".into(),
                )
                .to_string(),
            ));
        }

        // Reuse primitive-layer validation (version, fee_token_id, reference,
        // memo length, fee_limit constraints, gas price ordering).
        if let Err(reason) = morph_tx.validate() {
            return Err(ConsensusError::Other(
                MorphConsensusError::InvalidBody(reason.to_string()).to_string(),
            ));
        }
    }

    Ok(())
}

// ============================================================================
// Receipts Validation
// ============================================================================

/// Verifies the receipts root and logs bloom against the expected values.
///
/// This function:
/// 1. Calculates the receipts root from the provided receipts
/// 2. Calculates the logs bloom by combining all receipt blooms
/// 3. Compares both against the expected values from the block header
#[inline]
fn verify_receipts(
    expected_receipts_root: B256,
    expected_logs_bloom: Bloom,
    receipts: &[MorphReceipt],
) -> Result<(), ConsensusError> {
    // Calculate receipts root
    let receipts_with_bloom: Vec<_> = receipts.iter().map(TxReceipt::with_bloom_ref).collect();
    let receipts_root = alloy_consensus::proofs::calculate_receipt_root(&receipts_with_bloom);

    // Calculate logs bloom by combining all receipt blooms
    let logs_bloom = receipts_with_bloom
        .iter()
        .fold(Bloom::ZERO, |bloom, r| bloom | r.bloom_ref());

    // Compare receipts root
    if receipts_root != expected_receipts_root {
        return Err(ConsensusError::BodyReceiptRootDiff(
            GotExpected {
                got: receipts_root,
                expected: expected_receipts_root,
            }
            .into(),
        ));
    }

    // Compare logs bloom
    if logs_bloom != expected_logs_bloom {
        return Err(ConsensusError::BodyBloomLogDiff(
            GotExpected {
                got: logs_bloom,
                expected: expected_logs_bloom,
            }
            .into(),
        ));
    }

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Header, Signed};
    use alloy_genesis::Genesis;
    use alloy_primitives::{Address, B64, B256, Bytes, Signature, U256};
    use morph_primitives::transaction::{MAX_MEMO_LENGTH, MORPH_TX_VERSION_0, TxL1Msg};

    fn create_test_chainspec() -> Arc<MorphChainSpec> {
        let genesis_json = serde_json::json!({
            "config": {
                "chainId": 1337,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph203Time": 0,
                "viridianTime": 0,
                "emeraldTime": 0,
                "morph": {}
            },
            "alloc": {}
        });

        let genesis: Genesis = serde_json::from_value(genesis_json).unwrap();
        Arc::new(MorphChainSpec::from(genesis))
    }

    fn create_l1_msg_tx(queue_index: u64) -> MorphTxEnvelope {
        use alloy_consensus::Sealed;
        let tx = TxL1Msg {
            queue_index,
            gas_limit: 21000,
            to: Address::ZERO,
            value: U256::ZERO,
            input: Bytes::default(),
            sender: Address::ZERO,
        };
        // L1 messages have no signature - use Sealed instead of Signed
        MorphTxEnvelope::L1Msg(Sealed::new(tx))
    }

    fn create_regular_tx() -> MorphTxEnvelope {
        use alloy_consensus::TxLegacy;
        let tx = TxLegacy::default();
        let sig = Signature::new(U256::ZERO, U256::ZERO, false);
        MorphTxEnvelope::Legacy(Signed::new_unchecked(tx, sig, B256::ZERO))
    }

    /// Create a MorphHeader from a standard Header
    fn create_morph_header(inner: Header) -> MorphHeader {
        inner.into()
    }

    #[test]
    fn test_morph_consensus_creation() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        assert_eq!(consensus.chain_spec().inner.chain.id(), 1337);
    }

    #[test]
    fn test_validate_l1_messages_in_block_valid() {
        let txs = [
            create_l1_msg_tx(0),
            create_l1_msg_tx(1),
            create_regular_tx(),
        ];

        // L1 msgs: 0, 1 → last+1=2==header_next
        assert!(validate_l1_messages_in_block(&txs, 2).is_ok());
    }

    #[test]
    fn test_validate_l1_messages_in_block_after_regular() {
        let txs = [
            create_l1_msg_tx(0),
            create_regular_tx(),
            create_l1_msg_tx(1),
        ];

        assert!(validate_l1_messages_in_block(&txs, 2).is_err());
    }

    #[test]
    fn test_validate_header_extra_data_not_empty() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let header = create_morph_header(Header {
            extra_data: Bytes::from([1, 2, 3].as_slice()),
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::ExtraDataExceedsMax { .. })
        ));
    }

    #[test]
    fn test_validate_header_invalid_difficulty() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let header = create_morph_header(Header {
            difficulty: U256::from(1),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            nonce: B64::ZERO,
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::TheMergeDifficultyIsNotZero)
        ));
    }

    #[test]
    fn test_validate_header_invalid_nonce() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let header = create_morph_header(Header {
            nonce: B64::from(1u64),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::TheMergeNonceIsNotZero)
        ));
    }

    #[test]
    fn test_validate_header_invalid_ommers() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let header = create_morph_header(Header {
            nonce: B64::ZERO,
            ommers_hash: B256::ZERO, // not EMPTY_OMMER_ROOT_HASH
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::TheMergeOmmerRootIsNotEmpty)
        ));
    }

    #[test]
    fn test_validate_header_gas_used_exceeds_limit() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let header = create_morph_header(Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 1000,
            gas_used: 2000, // exceeds gas_limit
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::HeaderGasUsedExceedsGasLimit { .. })
        ));
    }

    #[test]
    fn test_validate_header_valid() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        // Create a valid header with timestamp not in the future
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let header = create_morph_header(Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 30_000_000,
            gas_used: 21_000,
            timestamp: now - 10,               // 10 seconds ago
            base_fee_per_gas: Some(1_000_000), // 0.001 Gwei (after Curie)
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(result.is_ok());
    }

    // ========================================================================
    // L1 Message Validation Tests
    // ========================================================================

    #[test]
    fn test_validate_l1_messages_in_block_empty_block() {
        let txs: [MorphTxEnvelope; 0] = [];

        // Empty block: no L1 messages → internal check always passes.
        // Any header_next value is accepted because the cross-block
        // monotonicity check is in validate_header_against_parent.
        assert!(validate_l1_messages_in_block(&txs, 0).is_ok());
        assert!(validate_l1_messages_in_block(&txs, 5).is_ok());
        assert!(validate_l1_messages_in_block(&txs, 100).is_ok());
    }

    #[test]
    fn test_validate_l1_messages_in_block_only_l1_messages() {
        let txs = [
            create_l1_msg_tx(0),
            create_l1_msg_tx(1),
            create_l1_msg_tx(2),
        ];

        // last=2, 2+1=3==header_next
        assert!(validate_l1_messages_in_block(&txs, 3).is_ok());
    }

    #[test]
    fn test_validate_l1_messages_in_block_only_regular_txs() {
        let txs = [
            create_regular_tx(),
            create_regular_tx(),
            create_regular_tx(),
        ];

        // No L1 messages → internal check passes (header_next not checked)
        assert!(validate_l1_messages_in_block(&txs, 0).is_ok());
    }

    #[test]
    fn test_validate_l1_messages_in_block_skipped_index() {
        // Block has 0 then 2 (skipping 1) — caught by sequential check
        let txs = [create_l1_msg_tx(0), create_l1_msg_tx(2)];

        let result = validate_l1_messages_in_block(&txs, 3);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("expected 1"));
        assert!(err_str.contains("got 2"));
    }

    #[test]
    fn test_validate_l1_messages_in_block_non_zero_start_index() {
        // Block starts L1 messages at queue_index 100
        let txs = [
            create_l1_msg_tx(100),
            create_l1_msg_tx(101),
            create_regular_tx(),
        ];

        // last=101, 101+1=102==header_next
        assert!(validate_l1_messages_in_block(&txs, 102).is_ok());
    }

    #[test]
    fn test_validate_l1_messages_in_block_duplicate_index() {
        // Duplicate index: 0, 0 — caught by sequential check (prev=0, expected 1, got 0)
        let txs = [create_l1_msg_tx(0), create_l1_msg_tx(0)];

        let result = validate_l1_messages_in_block(&txs, 1);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("expected 1"));
        assert!(err_str.contains("got 0"));
    }

    #[test]
    fn test_validate_l1_messages_in_block_out_of_order() {
        // Block has 1 then 0 — caught by sequential check (prev=1, expected 2, got 0)
        let txs = [create_l1_msg_tx(1), create_l1_msg_tx(0)];

        let result = validate_l1_messages_in_block(&txs, 2);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_l1_messages_in_block_wrong_next_l1_msg_index() {
        // Valid sequential L1 messages (0, 1, 2) but wrong next_l1_msg_index in header
        let txs = [
            create_l1_msg_tx(0),
            create_l1_msg_tx(1),
            create_l1_msg_tx(2),
            create_regular_tx(),
        ];

        // Header says 2 but should be 3 (last=2, 2+1=3). Value < min_expected triggers error.
        let result = validate_l1_messages_in_block(&txs, 2);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("expected 3"));
        assert!(err_str.contains("got 2"));
    }

    #[test]
    fn test_validate_l1_messages_in_block_multiple_l1_after_regular() {
        // Multiple L1 messages after regular tx
        let txs = [
            create_l1_msg_tx(0),
            create_regular_tx(),
            create_l1_msg_tx(1),
            create_l1_msg_tx(2),
        ];

        assert!(validate_l1_messages_in_block(&txs, 3).is_err());
    }

    // ========================================================================
    // Header Validation Tests (Additional)
    // ========================================================================

    #[test]
    fn test_validate_header_timestamp_in_future() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let future_ts = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600; // 1 hour in the future

        let header = create_morph_header(Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 30_000_000,
            timestamp: future_ts,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::TimestampIsInFuture { .. })
        ));
    }

    #[test]
    fn test_validate_header_gas_limit_exceeds_max() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let header = create_morph_header(Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: MAX_GAS_LIMIT + 1, // Exceeds max
            timestamp: now - 10,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::HeaderGasLimitExceedsMax { .. })
        ));
    }

    #[test]
    fn test_validate_header_base_fee_over_limit() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let header = create_morph_header(Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 30_000_000,
            timestamp: now - 10,
            base_fee_per_gas: Some(MORPH_MAXIMUM_BASE_FEE + 1), // Over limit
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("over limit"));
    }

    #[test]
    fn test_validate_header_base_fee_missing() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let header = create_morph_header(Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 30_000_000,
            timestamp: now - 10,
            base_fee_per_gas: None, // Missing (required)
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(matches!(result, Err(ConsensusError::BaseFeeMissing)));
    }

    #[test]
    fn test_validate_header_base_fee_at_max() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let header = create_morph_header(Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 30_000_000,
            timestamp: now - 10,
            base_fee_per_gas: Some(MORPH_MAXIMUM_BASE_FEE), // Exactly at max (valid)
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(result.is_ok());
    }

    // ========================================================================
    // Header Against Parent Validation Tests
    // ========================================================================

    fn create_valid_morph_header(timestamp: u64, gas_limit: u64, number: u64) -> MorphHeader {
        create_morph_header(Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit,
            timestamp,
            number,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        })
    }

    #[test]
    fn test_validate_header_against_parent_valid() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        let parent = create_valid_morph_header(1000, 30_000_000, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_morph_header(1001, 30_000_000, 101);
        child.inner.parent_hash = parent_sealed.hash();
        let child_sealed = SealedHeader::seal_slow(child);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_header_against_parent_l1_msg_index_monotonicity() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        // Parent has next_l1_msg_index = 10
        let mut parent = create_valid_morph_header(1000, 30_000_000, 100);
        parent.next_l1_msg_index = 10;
        let parent_sealed = SealedHeader::seal_slow(parent);

        // Child with next_l1_msg_index = 15 (increased, valid)
        let mut child = create_valid_morph_header(1001, 30_000_000, 101);
        child.inner.parent_hash = parent_sealed.hash();
        child.next_l1_msg_index = 15;
        let child_sealed = SealedHeader::seal_slow(child);
        assert!(
            consensus
                .validate_header_against_parent(&child_sealed, &parent_sealed)
                .is_ok()
        );

        // Child with next_l1_msg_index = 10 (unchanged, valid — no L1 msgs in block)
        let mut child_same = create_valid_morph_header(1001, 30_000_000, 101);
        child_same.inner.parent_hash = parent_sealed.hash();
        child_same.next_l1_msg_index = 10;
        let child_same_sealed = SealedHeader::seal_slow(child_same);
        assert!(
            consensus
                .validate_header_against_parent(&child_same_sealed, &parent_sealed)
                .is_ok()
        );

        // Child with next_l1_msg_index = 5 (decreased, INVALID)
        let mut child_dec = create_valid_morph_header(1001, 30_000_000, 101);
        child_dec.inner.parent_hash = parent_sealed.hash();
        child_dec.next_l1_msg_index = 5;
        let child_dec_sealed = SealedHeader::seal_slow(child_dec);
        let result = consensus.validate_header_against_parent(&child_dec_sealed, &parent_sealed);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("expected 10"));
        assert!(err_str.contains("got 5"));
    }

    #[test]
    fn test_validate_header_against_parent_timestamp_less_than_parent() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        let parent = create_valid_morph_header(1000, 30_000_000, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_morph_header(999, 30_000_000, 101); // timestamp < parent
        child.inner.parent_hash = parent_sealed.hash();
        let child_sealed = SealedHeader::seal_slow(child);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::TimestampIsInPast { .. })
        ));
    }

    #[test]
    fn test_validate_header_against_parent_timestamp_equal_to_parent() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        let parent = create_valid_morph_header(1000, 30_000_000, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_morph_header(1000, 30_000_000, 101); // timestamp == parent (valid)
        child.inner.parent_hash = parent_sealed.hash();
        let child_sealed = SealedHeader::seal_slow(child);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        // timestamp >= parent is valid
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_header_against_parent_gas_limit_increase_too_much() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        let parent_gas_limit = 30_000_000u64;
        let max_increase = parent_gas_limit / GAS_LIMIT_BOUND_DIVISOR;

        let parent = create_valid_morph_header(1000, parent_gas_limit, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        // Increase by more than allowed
        let mut child = create_valid_morph_header(1001, parent_gas_limit + max_increase + 1, 101);
        child.inner.parent_hash = parent_sealed.hash();
        let child_sealed = SealedHeader::seal_slow(child);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::GasLimitInvalidIncrease { .. })
        ));
    }

    #[test]
    fn test_validate_header_against_parent_gas_limit_decrease_too_much() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        let parent_gas_limit = 30_000_000u64;
        let max_decrease = parent_gas_limit / GAS_LIMIT_BOUND_DIVISOR;

        let parent = create_valid_morph_header(1000, parent_gas_limit, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        // Decrease by more than allowed
        let mut child = create_valid_morph_header(1001, parent_gas_limit - max_decrease - 1, 101);
        child.inner.parent_hash = parent_sealed.hash();
        let child_sealed = SealedHeader::seal_slow(child);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::GasLimitInvalidDecrease { .. })
        ));
    }

    #[test]
    fn test_validate_header_against_parent_gas_limit_at_boundary() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        let parent_gas_limit = 30_000_000u64;
        let max_change = parent_gas_limit / GAS_LIMIT_BOUND_DIVISOR;

        let parent = create_valid_morph_header(1000, parent_gas_limit, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        // Increase by exactly the boundary (diff == limit) should be REJECTED,
        // matching go-ethereum's `diff >= limit` check.
        let mut child_at_boundary =
            create_valid_morph_header(1001, parent_gas_limit + max_change, 101);
        child_at_boundary.inner.parent_hash = parent_sealed.hash();
        let child_sealed = SealedHeader::seal_slow(child_at_boundary);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(
            matches!(result, Err(ConsensusError::GasLimitInvalidIncrease { .. })),
            "gas limit change exactly at boundary should be rejected"
        );

        // Increase by one less than the boundary should be ACCEPTED
        let mut child_within =
            create_valid_morph_header(1001, parent_gas_limit + max_change - 1, 101);
        child_within.inner.parent_hash = parent_sealed.hash();
        let child_sealed = SealedHeader::seal_slow(child_within);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(
            result.is_ok(),
            "gas limit change within boundary should be accepted"
        );
    }

    #[test]
    fn test_validate_header_against_parent_gas_limit_below_minimum() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        // Use a parent gas limit that allows decreasing to below minimum within bounds
        // Parent = MINIMUM_GAS_LIMIT, so max decrease = MINIMUM_GAS_LIMIT / 1024 = 4
        // Child = MINIMUM_GAS_LIMIT - 1 = 4999, change = 1 which is < 4 (within bounds)
        let parent = create_valid_morph_header(1000, MINIMUM_GAS_LIMIT, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_morph_header(1001, MINIMUM_GAS_LIMIT - 1, 101);
        child.inner.parent_hash = parent_sealed.hash();
        let child_sealed = SealedHeader::seal_slow(child);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::GasLimitInvalidMinimum { .. })
        ));
    }

    #[test]
    fn test_validate_header_against_parent_wrong_parent_hash() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        let parent = create_valid_morph_header(1000, 30_000_000, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_morph_header(1001, 30_000_000, 101);
        child.inner.parent_hash = B256::random(); // Wrong parent hash
        let child_sealed = SealedHeader::seal_slow(child);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(matches!(result, Err(ConsensusError::ParentHashMismatch(_))));
    }

    #[test]
    fn test_validate_header_against_parent_wrong_block_number() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        let parent = create_valid_morph_header(1000, 30_000_000, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_morph_header(1001, 30_000_000, 102); // Should be 101
        child.inner.parent_hash = parent_sealed.hash();
        let child_sealed = SealedHeader::seal_slow(child);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(matches!(
            result,
            Err(ConsensusError::ParentBlockNumberMismatch { .. })
        ));
    }

    // ========================================================================
    // Receipts Validation Tests
    // ========================================================================

    #[test]
    fn test_verify_receipts_empty() {
        let receipts: [MorphReceipt; 0] = [];
        // Well-known Ethereum empty-trie root (keccak256 of RLP-encoded empty string).
        // Using a hardcoded constant instead of calculate_receipt_root(&[]) to avoid
        // a circular test that would pass even if the root computation is wrong.
        let empty_root: B256 = "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
            .parse()
            .unwrap();
        let expected_bloom = Bloom::ZERO;

        let result = verify_receipts(empty_root, expected_bloom, &receipts);
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_receipts_root_mismatch() {
        let receipts: [MorphReceipt; 0] = [];
        let wrong_root = B256::random(); // Wrong root
        let expected_bloom = Bloom::ZERO;

        let result = verify_receipts(wrong_root, expected_bloom, &receipts);
        assert!(matches!(
            result,
            Err(ConsensusError::BodyReceiptRootDiff(_))
        ));
    }

    #[test]
    fn test_verify_receipts_bloom_mismatch() {
        let receipts: [MorphReceipt; 0] = [];
        let expected_root = alloy_consensus::proofs::calculate_receipt_root::<
            alloy_consensus::ReceiptWithBloom<&MorphReceipt>,
        >(&[]);
        let wrong_bloom = Bloom::repeat_byte(0xff); // Wrong bloom

        let result = verify_receipts(expected_root, wrong_bloom, &receipts);
        assert!(matches!(result, Err(ConsensusError::BodyBloomLogDiff(_))));
    }

    // ========================================================================
    // Gas Limit Validation Helper Tests
    // These use Header directly since the generic helper functions work
    // on any type implementing BlockHeader trait.
    // ========================================================================

    fn create_valid_header(timestamp: u64, gas_limit: u64, number: u64) -> Header {
        Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit,
            timestamp,
            number,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        }
    }

    #[test]
    fn test_validate_against_parent_gas_limit_no_change() {
        let parent = create_valid_header(1000, 30_000_000, 100);
        let child = create_valid_header(1001, 30_000_000, 101);

        let result = validate_against_parent_gas_limit(&child, &parent);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_against_parent_timestamp_valid() {
        let parent = create_valid_header(1000, 30_000_000, 100);
        let child = create_valid_header(1001, 30_000_000, 101);

        // Both pre-Emerald and post-Emerald: strictly greater is always ok
        assert!(validate_against_parent_timestamp(&child, &parent, false).is_ok());
        assert!(validate_against_parent_timestamp(&child, &parent, true).is_ok());
    }

    #[test]
    fn test_validate_against_parent_timestamp_equal_pre_emerald() {
        let parent = create_valid_header(1000, 30_000_000, 100);
        let child = create_valid_header(1000, 30_000_000, 101); // Same timestamp

        // Pre-Emerald: equal timestamp is rejected
        let result = validate_against_parent_timestamp(&child, &parent, false);
        assert!(matches!(
            result,
            Err(ConsensusError::TimestampIsInPast { .. })
        ));
    }

    #[test]
    fn test_validate_against_parent_timestamp_equal_post_emerald() {
        let parent = create_valid_header(1000, 30_000_000, 100);
        let child = create_valid_header(1000, 30_000_000, 101); // Same timestamp

        // Post-Emerald: equal timestamp is allowed
        let result = validate_against_parent_timestamp(&child, &parent, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_against_parent_timestamp_past() {
        let parent = create_valid_header(1000, 30_000_000, 100);
        let child = create_valid_header(999, 30_000_000, 101); // Earlier timestamp

        // Both pre-Emerald and post-Emerald: strictly less is always rejected
        assert!(matches!(
            validate_against_parent_timestamp(&child, &parent, false),
            Err(ConsensusError::TimestampIsInPast { .. })
        ));
        assert!(matches!(
            validate_against_parent_timestamp(&child, &parent, true),
            Err(ConsensusError::TimestampIsInPast { .. })
        ));
    }

    // ========================================================================
    // Coinbase / FeeVault Validation Tests
    // ========================================================================

    #[test]
    fn test_validate_header_coinbase_non_zero_with_fee_vault() {
        // Create a chainspec with FeeVault explicitly enabled
        let genesis_json = serde_json::json!({
            "config": {
                "chainId": 1337,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph203Time": 0,
                "viridianTime": 0,
                "emeraldTime": 0,
                "morph": {
                    "feeVaultAddress": "0x530000000000000000000000000000000000000a"
                }
            },
            "alloc": {}
        });
        let genesis: Genesis = serde_json::from_value(genesis_json).unwrap();
        let chain_spec = Arc::new(MorphChainSpec::from(genesis));
        assert!(
            chain_spec.is_fee_vault_enabled(),
            "test chainspec must have FeeVault enabled"
        );
        let consensus = MorphConsensus::new(chain_spec);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Non-zero coinbase with fee vault enabled should fail
        let header = create_morph_header(Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: Address::repeat_byte(0x01),
            gas_limit: 30_000_000,
            timestamp: now - 10,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        });
        let sealed = SealedHeader::seal_slow(header);

        let result = consensus.validate_header(&sealed);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("coinbase"));
    }

    // ========================================================================
    // MorphTx Version Validation Tests
    // ========================================================================

    fn create_morph_tx_v0(fee_token_id: u16) -> MorphTxEnvelope {
        use alloy_consensus::Signed;
        use morph_primitives::TxMorph;

        let tx = TxMorph {
            chain_id: 1337,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: alloy_primitives::TxKind::Call(Address::repeat_byte(0x01)),
            value: U256::ZERO,
            access_list: Default::default(),
            version: MORPH_TX_VERSION_0,
            fee_token_id,
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
            input: Bytes::new(),
        };
        MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::new(U256::ZERO, U256::ZERO, false),
            B256::ZERO,
        ))
    }

    fn create_morph_tx_v1(fee_token_id: u16) -> MorphTxEnvelope {
        use alloy_consensus::Signed;
        use morph_primitives::TxMorph;

        let tx = TxMorph {
            chain_id: 1337,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: alloy_primitives::TxKind::Call(Address::repeat_byte(0x01)),
            value: U256::ZERO,
            access_list: Default::default(),
            version: MORPH_TX_VERSION_1,
            fee_token_id,
            fee_limit: U256::ZERO,
            reference: Some(B256::repeat_byte(0xab)),
            memo: Some(Bytes::from_static(b"test-memo")),
            input: Bytes::new(),
        };
        MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::new(U256::ZERO, U256::ZERO, false),
            B256::ZERO,
        ))
    }

    #[test]
    fn test_validate_morph_tx_v0_valid() {
        // V0 with fee_token_id > 0 and no reference/memo
        let txs = [create_morph_tx_v0(1)];
        let result = validate_morph_txs(&txs, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_morph_tx_v0_zero_fee_token_rejected() {
        // V0 with fee_token_id == 0 should be rejected
        let txs = [create_morph_tx_v0(0)];
        let result = validate_morph_txs(&txs, false);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("version 0 MorphTx requires FeeTokenID > 0")
        );
    }

    #[test]
    fn test_validate_morph_tx_v0_with_reference_rejected() {
        use alloy_consensus::Signed;
        use morph_primitives::TxMorph;

        let tx = TxMorph {
            chain_id: 1337,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: alloy_primitives::TxKind::Call(Address::repeat_byte(0x01)),
            value: U256::ZERO,
            access_list: Default::default(),
            version: MORPH_TX_VERSION_0,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: Some(B256::repeat_byte(0x01)), // V0 should not have reference
            memo: None,
            input: Bytes::new(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::new(U256::ZERO, U256::ZERO, false),
            B256::ZERO,
        ));

        let txs = [envelope];
        let result = validate_morph_txs(&txs, false);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("version 0 MorphTx does not support Reference field")
        );
    }

    #[test]
    fn test_validate_morph_tx_v1_before_jade_rejected() {
        // V1 before jade fork should be rejected
        let txs = [create_morph_tx_v1(1)];
        let result = validate_morph_txs(&txs, false);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("jade fork not reached")
        );
    }

    #[test]
    fn test_validate_morph_tx_v1_after_jade_valid() {
        // V1 after jade fork should pass
        let txs = [create_morph_tx_v1(1)];
        let result = validate_morph_txs(&txs, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_morph_tx_v1_fee_token_0_with_fee_limit_rejected() {
        use alloy_consensus::Signed;
        use morph_primitives::TxMorph;

        // V1 with fee_token_id == 0 and non-zero fee_limit is invalid
        let tx = TxMorph {
            chain_id: 1337,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: alloy_primitives::TxKind::Call(Address::repeat_byte(0x01)),
            value: U256::ZERO,
            access_list: Default::default(),
            version: MORPH_TX_VERSION_1,
            fee_token_id: 0,
            fee_limit: U256::from(100u64), // non-zero with fee_token_id=0
            reference: None,
            memo: None,
            input: Bytes::new(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::new(U256::ZERO, U256::ZERO, false),
            B256::ZERO,
        ));

        let txs = [envelope];
        let result = validate_morph_txs(&txs, true);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("version 1 MorphTx cannot have FeeLimit when FeeTokenID is 0")
        );
    }

    #[test]
    fn test_validate_morph_tx_v1_memo_too_long_rejected() {
        use alloy_consensus::Signed;
        use morph_primitives::TxMorph;

        let tx = TxMorph {
            chain_id: 1337,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: alloy_primitives::TxKind::Call(Address::repeat_byte(0x01)),
            value: U256::ZERO,
            access_list: Default::default(),
            version: MORPH_TX_VERSION_1,
            fee_token_id: 1,
            fee_limit: U256::from(100u64),
            reference: None,
            memo: Some(Bytes::from(vec![0xab; MAX_MEMO_LENGTH + 1])), // too long
            input: Bytes::new(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::new(U256::ZERO, U256::ZERO, false),
            B256::ZERO,
        ));

        let txs = [envelope];
        let result = validate_morph_txs(&txs, true);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("memo exceeds maximum length")
        );
    }

    #[test]
    fn test_validate_morph_tx_unsupported_version_rejected() {
        use alloy_consensus::Signed;
        use morph_primitives::TxMorph;

        let tx = TxMorph {
            chain_id: 1337,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: alloy_primitives::TxKind::Call(Address::repeat_byte(0x01)),
            value: U256::ZERO,
            access_list: Default::default(),
            version: 99, // Unsupported version
            fee_token_id: 1,
            fee_limit: U256::from(100u64),
            reference: None,
            memo: None,
            input: Bytes::new(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::new(U256::ZERO, U256::ZERO, false),
            B256::ZERO,
        ));

        let txs = [envelope];
        let result = validate_morph_txs(&txs, true);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsupported MorphTx version")
        );
    }

    #[test]
    fn test_validate_morph_txs_skips_non_morph_tx() {
        // Regular transactions should be skipped entirely
        let txs = [create_regular_tx(), create_l1_msg_tx(0)];
        let result = validate_morph_txs(&txs, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_morph_txs_mixed_block() {
        // Mixed block with valid V0 MorphTx and regular txs
        let txs = [
            create_l1_msg_tx(0),
            create_regular_tx(),
            create_morph_tx_v0(1),
        ];
        let result = validate_morph_txs(&txs, false);
        assert!(result.is_ok());
    }

    // ========================================================================
    // L1 Message Queue Index Overflow Tests
    // ========================================================================

    #[test]
    fn test_validate_l1_messages_in_block_queue_index_overflow() {
        // When prev_queue_index is u64::MAX, checked_add should fail
        let txs = [create_l1_msg_tx(u64::MAX - 1), create_l1_msg_tx(u64::MAX)];

        // last=MAX, MAX+1 overflows
        let result = validate_l1_messages_in_block(&txs, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_l1_messages_in_block_single_l1() {
        let txs = [create_l1_msg_tx(42)];
        // last=42, 42+1=43==header_next
        assert!(validate_l1_messages_in_block(&txs, 43).is_ok());
        // Wrong header_next
        assert!(validate_l1_messages_in_block(&txs, 42).is_err());
    }

    // ========================================================================
    // Post-Execution Validation Tests
    // ========================================================================

    #[test]
    fn test_validate_block_post_execution_gas_mismatch() {
        use alloy_consensus::Receipt;
        use morph_primitives::{MorphReceipt, MorphTransactionReceipt};

        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        // Create a receipt with cumulative_gas_used = 21000
        let receipt = MorphReceipt::Legacy(MorphTransactionReceipt::with_l1_fee(
            Receipt {
                status: true.into(),
                cumulative_gas_used: 21000,
                logs: vec![],
            },
            U256::ZERO,
        ));

        let result = alloy_evm::block::BlockExecutionResult {
            receipts: vec![receipt],
            requests: Default::default(),
            gas_used: 21000,
            blob_gas_used: 0,
        };

        // Create a block header with gas_used = 50000 (mismatch!)
        let header = create_morph_header(Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 30_000_000,
            gas_used: 50000, // Does not match receipt
            timestamp: 1000,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        });
        let body = morph_primitives::BlockBody {
            transactions: vec![create_regular_tx()],
            ommers: vec![],
            withdrawals: None,
        };
        let block = morph_primitives::Block { header, body };
        let recovered =
            reth_primitives_traits::RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);

        let post_result = consensus.validate_block_post_execution(&recovered, &result);
        assert!(matches!(
            post_result,
            Err(ConsensusError::BlockGasUsed { .. })
        ));
    }
}
