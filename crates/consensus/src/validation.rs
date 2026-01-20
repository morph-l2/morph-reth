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
//! - Base fee must be set after Curie hardfork
//!
//! ## L1 Message Rules
//!
//! - All L1 messages must be at the beginning of the block
//! - L1 messages must have strictly sequential `queue_index`
//! - No gaps allowed in the queue index sequence
//!
//! ## Block Body Validation
//!
//! - No uncle blocks allowed
//! - Withdrawals must be empty
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
use morph_chainspec::{MorphChainSpec, hardfork::MorphHardforks};
use morph_primitives::{Block, BlockBody, MorphReceipt, MorphTxEnvelope};
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
#[derive(Debug, Clone)]
pub struct MorphConsensus {
    /// Chain specification containing hardfork information and chain config.
    chain_spec: Arc<MorphChainSpec>,
}

impl MorphConsensus {
    /// Creates a new [`MorphConsensus`] instance.
    pub const fn new(chain_spec: Arc<MorphChainSpec>) -> Self {
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

impl HeaderValidator<alloy_consensus::Header> for MorphConsensus {
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
    /// 9. **Base Fee**: Must be set after Curie hardfork and <= 10 Gwei
    fn validate_header(
        &self,
        header: &SealedHeader<alloy_consensus::Header>,
    ) -> Result<(), ConsensusError> {
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

        // Validate the EIP1559 fee is set if the header is after Curie
        // Note: Curie uses block-based activation
        if self.chain_spec.is_curie_active_at_block(header.number()) {
            let base_fee = header
                .base_fee_per_gas()
                .ok_or(ConsensusError::BaseFeeMissing)?;
            if base_fee > MORPH_MAXIMUM_BASE_FEE {
                return Err(ConsensusError::Other(
                    MorphConsensusError::BaseFeeOverLimit(base_fee).to_string(),
                ));
            }
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
        header: &SealedHeader<alloy_consensus::Header>,
        parent: &SealedHeader<alloy_consensus::Header>,
    ) -> Result<(), ConsensusError> {
        // Validate parent hash and block number
        validate_against_parent_hash_number(header.header(), parent)?;

        // Validate timestamp against parent
        validate_against_parent_timestamp(header.header(), parent.header())?;

        // Validate gas limit change
        validate_against_parent_gas_limit(header.header(), parent.header())?;

        Ok(())
    }
}

// ============================================================================
// Consensus Implementation
// ============================================================================

impl Consensus<Block> for MorphConsensus {
    type Error = ConsensusError;

    /// Validates the block body against the header.
    ///
    /// Checks that the body's computed transaction root matches the header's.
    fn validate_body_against_header(
        &self,
        body: &BlockBody,
        header: &SealedHeader<alloy_consensus::Header>,
    ) -> Result<(), Self::Error> {
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
    fn validate_block_pre_execution(&self, block: &SealedBlock<Block>) -> Result<(), Self::Error> {
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

        // Validate L1 messages ordering
        let txs: Vec<_> = block.body().transactions().collect();
        validate_l1_messages(&txs)?;

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

/// Validates that the header's timestamp is not before the parent's timestamp.
///
/// # Errors
///
/// Returns [`ConsensusError::TimestampIsInPast`] if the header's timestamp
/// is less than the parent's timestamp.
///
/// # Note
///
/// Equal timestamps are allowed - only strictly less than is rejected.
#[inline]
fn validate_against_parent_timestamp<H: BlockHeader>(
    header: &H,
    parent: &H,
) -> Result<(), ConsensusError> {
    if header.timestamp() < parent.timestamp() {
        return Err(ConsensusError::TimestampIsInPast {
            parent_timestamp: parent.timestamp(),
            timestamp: header.timestamp(),
        });
    }
    Ok(())
}

/// Validates gas limit change against parent.
///
/// The gas limit change between consecutive blocks must not exceed
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
    if diff > limit {
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

/// Validates L1 message ordering in a block's transactions.
///
/// L1 messages are special transactions that originate from L1 (deposits, etc.).
/// They must follow strict ordering rules to ensure deterministic block execution.
///
/// # Rules
///
/// 1. **Position**: All L1 messages must appear at the beginning of the block.
///    Once a regular (L2) transaction appears, no more L1 messages are allowed.
///
/// 2. **Sequential Queue Index**: L1 messages must have strictly sequential
///    `queue_index` values. If the first L1 message has `queue_index = N`,
///    the next must have `queue_index = N+1`, and so on.
///
/// # Errors
///
/// - [`MorphConsensusError::MalformedL1Message`] if an L1 message is missing its queue_index
/// - [`MorphConsensusError::L1MessagesNotInOrder`] if queue indices are not sequential
/// - [`MorphConsensusError::InvalidL1MessageOrder`] if L1 message appears after L2 transaction
///
/// # Example (Valid)
///
/// ```text
/// [L1Msg(queue=0), L1Msg(queue=1), L1Msg(queue=2), RegularTx, RegularTx]
/// ```
///
/// # Example (Invalid - L1 after L2)
///
/// ```text
/// [L1Msg(queue=0), RegularTx, L1Msg(queue=1)]  // ❌ L1 after L2
/// ```
#[inline]
fn validate_l1_messages(txs: &[&MorphTxEnvelope]) -> Result<(), ConsensusError> {
    // Find the starting queue index from the first L1 message
    let mut queue_index = txs
        .iter()
        .find(|tx| tx.is_l1_msg())
        .and_then(|tx| tx.queue_index())
        .unwrap_or_default();

    let mut saw_l2_transaction = false;

    for tx in txs {
        // Check queue index is strictly sequential
        if tx.is_l1_msg() {
            let tx_queue_index = tx.queue_index().ok_or_else(|| {
                ConsensusError::Other(MorphConsensusError::MalformedL1Message.to_string())
            })?;
            if tx_queue_index != queue_index {
                return Err(ConsensusError::Other(
                    MorphConsensusError::L1MessagesNotInOrder {
                        expected: queue_index,
                        actual: tx_queue_index,
                    }
                    .to_string(),
                ));
            }
            queue_index = tx_queue_index + 1;
        }

        // Check L1 messages are only at the start of the block
        if tx.is_l1_msg() && saw_l2_transaction {
            return Err(ConsensusError::Other(
                MorphConsensusError::InvalidL1MessageOrder.to_string(),
            ));
        }
        saw_l2_transaction = !tx.is_l1_msg();
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
    use alloy_primitives::{Address, B64, B256, Bytes, Signature, TxKind, U256};
    use morph_primitives::transaction::TxL1Msg;

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
        let tx = TxL1Msg {
            queue_index,
            from: Address::ZERO,
            nonce: queue_index, // nonce is used as queue index for L1 messages
            gas_limit: 21000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::default(),
        };
        let sig = Signature::new(U256::ZERO, U256::ZERO, false);
        MorphTxEnvelope::L1Msg(Signed::new_unchecked(tx, sig, B256::ZERO))
    }

    fn create_regular_tx() -> MorphTxEnvelope {
        use alloy_consensus::TxLegacy;
        let tx = TxLegacy::default();
        let sig = Signature::new(U256::ZERO, U256::ZERO, false);
        MorphTxEnvelope::Legacy(Signed::new_unchecked(tx, sig, B256::ZERO))
    }

    #[test]
    fn test_morph_consensus_creation() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        assert_eq!(consensus.chain_spec().inner.chain.id(), 1337);
    }

    #[test]
    fn test_validate_l1_messages_valid() {
        let txs = [
            create_l1_msg_tx(0),
            create_l1_msg_tx(1),
            create_regular_tx(),
        ];
        let txs_refs: Vec<_> = txs.iter().collect();
        assert!(validate_l1_messages(&txs_refs).is_ok());
    }

    #[test]
    fn test_validate_l1_messages_after_regular() {
        let txs = [
            create_l1_msg_tx(0),
            create_regular_tx(),
            create_l1_msg_tx(1),
        ];
        let txs_refs: Vec<_> = txs.iter().collect();
        assert!(validate_l1_messages(&txs_refs).is_err());
    }

    #[test]
    fn test_validate_header_extra_data_not_empty() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let header = Header {
            extra_data: Bytes::from([1, 2, 3].as_slice()),
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            ..Default::default()
        };
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
        let header = Header {
            difficulty: U256::from(1),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            nonce: B64::ZERO,
            ..Default::default()
        };
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
        let header = Header {
            nonce: B64::from(1u64),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            ..Default::default()
        };
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
        let header = Header {
            nonce: B64::ZERO,
            ommers_hash: B256::ZERO, // not EMPTY_OMMER_ROOT_HASH
            ..Default::default()
        };
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
        let header = Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 1000,
            gas_used: 2000, // exceeds gas_limit
            ..Default::default()
        };
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
        let header = Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 30_000_000,
            gas_used: 21_000,
            timestamp: now - 10,               // 10 seconds ago
            base_fee_per_gas: Some(1_000_000), // 0.001 Gwei (after Curie)
            ..Default::default()
        };
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(result.is_ok());
    }

    // ========================================================================
    // L1 Message Validation Tests
    // ========================================================================

    #[test]
    fn test_validate_l1_messages_empty_block() {
        let txs: [MorphTxEnvelope; 0] = [];
        let txs_refs: Vec<_> = txs.iter().collect();
        assert!(validate_l1_messages(&txs_refs).is_ok());
    }

    #[test]
    fn test_validate_l1_messages_only_l1_messages() {
        let txs = [
            create_l1_msg_tx(0),
            create_l1_msg_tx(1),
            create_l1_msg_tx(2),
        ];
        let txs_refs: Vec<_> = txs.iter().collect();
        assert!(validate_l1_messages(&txs_refs).is_ok());
    }

    #[test]
    fn test_validate_l1_messages_only_regular_txs() {
        let txs = [
            create_regular_tx(),
            create_regular_tx(),
            create_regular_tx(),
        ];
        let txs_refs: Vec<_> = txs.iter().collect();
        assert!(validate_l1_messages(&txs_refs).is_ok());
    }

    #[test]
    fn test_validate_l1_messages_skipped_index() {
        // Skip index 1: 0, 2
        let txs = [create_l1_msg_tx(0), create_l1_msg_tx(2)];
        let txs_refs: Vec<_> = txs.iter().collect();
        let result = validate_l1_messages(&txs_refs);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("expected 1"));
        assert!(err_str.contains("got 2"));
    }

    #[test]
    fn test_validate_l1_messages_non_zero_start_index() {
        // Starting from index 100 is valid
        let txs = [
            create_l1_msg_tx(100),
            create_l1_msg_tx(101),
            create_regular_tx(),
        ];
        let txs_refs: Vec<_> = txs.iter().collect();
        assert!(validate_l1_messages(&txs_refs).is_ok());
    }

    #[test]
    fn test_validate_l1_messages_duplicate_index() {
        // Duplicate index: 0, 0
        let txs = [create_l1_msg_tx(0), create_l1_msg_tx(0)];
        let txs_refs: Vec<_> = txs.iter().collect();
        let result = validate_l1_messages(&txs_refs);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("expected 1"));
        assert!(err_str.contains("got 0"));
    }

    #[test]
    fn test_validate_l1_messages_out_of_order() {
        // Reversed order: 1, 0
        let txs = [create_l1_msg_tx(1), create_l1_msg_tx(0)];
        let txs_refs: Vec<_> = txs.iter().collect();
        let result = validate_l1_messages(&txs_refs);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_l1_messages_multiple_l1_after_regular() {
        // Multiple L1 messages after regular tx
        let txs = [
            create_l1_msg_tx(0),
            create_regular_tx(),
            create_l1_msg_tx(1),
            create_l1_msg_tx(2),
        ];
        let txs_refs: Vec<_> = txs.iter().collect();
        assert!(validate_l1_messages(&txs_refs).is_err());
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

        let header = Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 30_000_000,
            timestamp: future_ts,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        };
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

        let header = Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: MAX_GAS_LIMIT + 1, // Exceeds max
            timestamp: now - 10,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        };
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

        let header = Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 30_000_000,
            timestamp: now - 10,
            base_fee_per_gas: Some(MORPH_MAXIMUM_BASE_FEE + 1), // Over limit
            ..Default::default()
        };
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("over limit"));
    }

    #[test]
    fn test_validate_header_base_fee_missing_after_curie() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let header = Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 30_000_000,
            timestamp: now - 10,
            base_fee_per_gas: None, // Missing after Curie
            ..Default::default()
        };
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

        let header = Header {
            nonce: B64::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            gas_limit: 30_000_000,
            timestamp: now - 10,
            base_fee_per_gas: Some(MORPH_MAXIMUM_BASE_FEE), // Exactly at max (valid)
            ..Default::default()
        };
        let sealed = SealedHeader::seal_slow(header);
        let result = consensus.validate_header(&sealed);
        assert!(result.is_ok());
    }

    // ========================================================================
    // Header Against Parent Validation Tests
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
    fn test_validate_header_against_parent_valid() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        let parent = create_valid_header(1000, 30_000_000, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_header(1001, 30_000_000, 101);
        child.parent_hash = parent_sealed.hash();
        let child_sealed = SealedHeader::seal_slow(child);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_header_against_parent_timestamp_less_than_parent() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        let parent = create_valid_header(1000, 30_000_000, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_header(999, 30_000_000, 101); // timestamp < parent
        child.parent_hash = parent_sealed.hash();
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

        let parent = create_valid_header(1000, 30_000_000, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_header(1000, 30_000_000, 101); // timestamp == parent (valid)
        child.parent_hash = parent_sealed.hash();
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

        let parent = create_valid_header(1000, parent_gas_limit, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        // Increase by more than allowed
        let mut child = create_valid_header(1001, parent_gas_limit + max_increase + 1, 101);
        child.parent_hash = parent_sealed.hash();
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

        let parent = create_valid_header(1000, parent_gas_limit, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        // Decrease by more than allowed
        let mut child = create_valid_header(1001, parent_gas_limit - max_decrease - 1, 101);
        child.parent_hash = parent_sealed.hash();
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

        let parent = create_valid_header(1000, parent_gas_limit, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        // Increase by exactly the allowed amount (valid)
        let mut child = create_valid_header(1001, parent_gas_limit + max_change, 101);
        child.parent_hash = parent_sealed.hash();
        let child_sealed = SealedHeader::seal_slow(child);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_header_against_parent_gas_limit_below_minimum() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        // Use a parent gas limit that allows decreasing to below minimum within bounds
        // Parent = MINIMUM_GAS_LIMIT, so max decrease = MINIMUM_GAS_LIMIT / 1024 = 4
        // Child = MINIMUM_GAS_LIMIT - 1 = 4999, change = 1 which is < 4 (within bounds)
        let parent = create_valid_header(1000, MINIMUM_GAS_LIMIT, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_header(1001, MINIMUM_GAS_LIMIT - 1, 101);
        child.parent_hash = parent_sealed.hash();
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

        let parent = create_valid_header(1000, 30_000_000, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_header(1001, 30_000_000, 101);
        child.parent_hash = B256::random(); // Wrong parent hash
        let child_sealed = SealedHeader::seal_slow(child);

        let result = consensus.validate_header_against_parent(&child_sealed, &parent_sealed);
        assert!(matches!(result, Err(ConsensusError::ParentHashMismatch(_))));
    }

    #[test]
    fn test_validate_header_against_parent_wrong_block_number() {
        let chain_spec = create_test_chainspec();
        let consensus = MorphConsensus::new(chain_spec);

        let parent = create_valid_header(1000, 30_000_000, 100);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let mut child = create_valid_header(1001, 30_000_000, 102); // Should be 101
        child.parent_hash = parent_sealed.hash();
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
        let expected_root = alloy_consensus::proofs::calculate_receipt_root::<
            alloy_consensus::ReceiptWithBloom<&MorphReceipt>,
        >(&[]);
        let expected_bloom = Bloom::ZERO;

        let result = verify_receipts(expected_root, expected_bloom, &receipts);
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
    // ========================================================================

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

        let result = validate_against_parent_timestamp(&child, &parent);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_against_parent_timestamp_equal() {
        let parent = create_valid_header(1000, 30_000_000, 100);
        let child = create_valid_header(1000, 30_000_000, 101); // Same timestamp

        let result = validate_against_parent_timestamp(&child, &parent);
        assert!(result.is_ok()); // Equal timestamp is allowed
    }

    #[test]
    fn test_validate_against_parent_timestamp_past() {
        let parent = create_valid_header(1000, 30_000_000, 100);
        let child = create_valid_header(999, 30_000_000, 101); // Earlier timestamp

        let result = validate_against_parent_timestamp(&child, &parent);
        assert!(matches!(
            result,
            Err(ConsensusError::TimestampIsInPast { .. })
        ));
    }
}
