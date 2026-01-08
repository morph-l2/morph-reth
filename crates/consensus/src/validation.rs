//! Morph L2 consensus validation.
//!
//! This module provides consensus validation for Morph L2 blocks, including:
//! - Header validation
//! - Body validation (L1 messages ordering)
//! - Block pre/post execution validation
//!
use crate::MorphConsensusError;
use alloy_consensus::{BlockHeader as _, EMPTY_OMMER_ROOT_HASH, TxReceipt};
use alloy_evm::block::BlockExecutionResult;
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
        if self
            .chain_spec
            .is_curie_active_at_timestamp(header.timestamp())
        {
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

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<alloy_consensus::Header>,
        parent: &SealedHeader<alloy_consensus::Header>,
    ) -> Result<(), ConsensusError> {
        // Validate parent hash and block number
        validate_against_parent_hash_number(header.header(), parent)?;

        // Validate timestamp against parent
        validate_against_parent_timestamp(header.header(), parent.header())?;

        // Validate gas limit change (before Curie only)
        validate_against_parent_gas_limit(header.header(), parent.header())?;

        Ok(())
    }
}

// ============================================================================
// Consensus Implementation
// ============================================================================

impl Consensus<Block> for MorphConsensus {
    type Error = ConsensusError;

    fn validate_body_against_header(
        &self,
        body: &BlockBody,
        header: &SealedHeader<alloy_consensus::Header>,
    ) -> Result<(), Self::Error> {
        validate_body_against_header(body, header.header())
    }

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

        Ok(())
    }
}

//
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
/// - Gas limit change must be within bounds (parent / GAS_LIMIT_BOUND_DIVISOR)
/// - Only checked before Curie hardfork
///
/// Note: After Curie, gas limit verification is part of EIP-1559 header validation
/// which Morph doesn't strictly enforce (sequencer can set values).
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
/// - All L1 messages must be at the beginning of the block
/// - L1 messages must have strictly sequential queue indices
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
            let tx_queue_index = tx.queue_index().expect("is_l1_msg");
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
                "bernoulliTime": 0,
                "curieTime": 0,
                "morph203Time": 0,
                "viridianTime": 0,
                "emeraldTime": 0
            },
            "alloc": {}
        });

        let genesis: Genesis = serde_json::from_value(genesis_json).unwrap();
        Arc::new(MorphChainSpec::from(genesis))
    }

    fn create_l1_msg_tx(queue_index: u64) -> MorphTxEnvelope {
        let tx = TxL1Msg {
            queue_index,
            tx_hash: B256::ZERO,
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
        let txs = vec![
            create_l1_msg_tx(0),
            create_l1_msg_tx(1),
            create_regular_tx(),
        ];
        let txs_refs: Vec<_> = txs.iter().collect();
        assert!(validate_l1_messages(&txs_refs).is_ok());
    }

    #[test]
    fn test_validate_l1_messages_after_regular() {
        let txs = vec![
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
            extra_data: Bytes::from(vec![1, 2, 3]),
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
}
