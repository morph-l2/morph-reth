use crate::MorphEvmConfig;
use alloy_consensus::{BlockBody, EMPTY_OMMER_ROOT_HASH, Header, TxReceipt, proofs};
use alloy_evm::block::{BlockExecutionError, BlockExecutionResult};
use alloy_primitives::{Address, B64, logs_bloom};
use morph_chainspec::MorphChainSpec;
use morph_primitives::{MorphHeader, receipt::calculate_receipt_root_no_memo};
use reth_chainspec::EthereumHardforks;
use reth_evm::execute::{BlockAssembler, BlockAssemblerInput};
use revm::context::Block;
use std::sync::Arc;

/// Assembler for Morph blocks.
///
/// This assembler builds Morph blocks from the execution output.
/// Unlike `EthBlockAssembler`, it produces `MorphHeader` with proper
/// L2-specific fields (next_l1_msg_index, batch_hash).
#[derive(Debug, Clone)]
pub struct MorphBlockAssembler {
    /// Chain specification
    chain_spec: Arc<MorphChainSpec>,
}

impl MorphBlockAssembler {
    /// Creates a new [`MorphBlockAssembler`] with the given chain specification.
    ///
    /// # Arguments
    /// * `chain_spec` - The Morph chain specification used for hardfork detection
    pub fn new(chain_spec: Arc<MorphChainSpec>) -> Self {
        Self { chain_spec }
    }

    /// Returns the chain spec.
    pub const fn chain_spec(&self) -> &Arc<MorphChainSpec> {
        &self.chain_spec
    }
}

impl BlockAssembler<MorphEvmConfig> for MorphBlockAssembler {
    type Block = morph_primitives::Block;

    /// Assembles a Morph block from execution results.
    ///
    /// This method constructs a complete [`Block`] from the execution output,
    /// including building the proper [`MorphHeader`] with L2-specific fields.
    ///
    /// # Block Assembly Process
    /// 1. **Calculate Merkle Roots**: Computes transaction root and receipt root
    /// 2. **Build Logs Bloom**: Aggregates logs from all receipts
    /// 3. **Check Hardforks**: Determines if EIP-1559 (London) is active for base fee
    /// 4. **Build Header**: Creates standard Ethereum header fields
    /// 5. **Wrap in MorphHeader**: Adds L2-specific fields (next_l1_msg_index, batch_hash)
    /// 6. **Build Block Body**: Combines transactions and empty ommers
    ///
    /// # L2-Specific Fields
    /// - `next_l1_msg_index`: Inherited from parent block
    /// - `batch_hash`: Set to default (will be filled by payload builder)
    ///
    /// # Arguments
    /// * `input` - Contains execution context, transactions, receipts, and state root
    ///
    /// # Returns
    /// A fully assembled Morph block ready for sealing.
    ///
    /// # Errors
    /// Returns error if block assembly fails (should not occur in normal operation).
    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, MorphEvmConfig, MorphHeader>,
    ) -> Result<Self::Block, BlockExecutionError> {
        let BlockAssemblerInput {
            evm_env,
            execution_ctx,
            parent,
            transactions,
            output: BlockExecutionResult {
                receipts, gas_used, ..
            },
            state_root,
            ..
        } = input;

        let timestamp = evm_env.block_env.timestamp();
        let block_number: u64 = evm_env.block_env.number().to();

        // Calculate roots and bloom
        let transactions_root = proofs::calculate_transaction_root(&transactions);
        let receipts_root = calculate_receipt_root_no_memo(receipts);
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));

        // Determine if EIP-1559 is active
        let is_london_active = self.chain_spec.is_london_active_at_block(block_number);

        // Build standard Ethereum header
        let inner = Header {
            parent_hash: execution_ctx.parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: Address::ZERO,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root: None,
            logs_bloom,
            timestamp: timestamp.to(),
            mix_hash: evm_env.block_env.prevrandao().unwrap_or_default(),
            nonce: B64::ZERO,
            // Only include base_fee_per_gas after London (EIP-1559)
            base_fee_per_gas: is_london_active.then_some(evm_env.block_env.basefee()),
            number: block_number,
            gas_limit: evm_env.block_env.gas_limit(),
            difficulty: evm_env.block_env.difficulty(),
            gas_used: *gas_used,
            extra_data: Default::default(),
            parent_beacon_block_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            requests_hash: None,
        };

        // Wrap in MorphHeader with L2-specific fields
        // Note: next_l1_msg_index and batch_hash will be set by the payload builder
        let header = MorphHeader {
            inner,
            next_l1_msg_index: parent.header().next_l1_msg_index,
            batch_hash: Default::default(),
        };

        Ok(alloy_consensus::Block::new(
            header,
            BlockBody {
                transactions,
                ommers: Default::default(),
                withdrawals: None,
            },
        ))
    }
}
