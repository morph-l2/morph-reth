//! Built payload types for Morph.

use crate::ExecutableL2Data;
use alloy_consensus::BlockHeader;
use alloy_eips::eip7685::Requests;
use alloy_primitives::U256;
use alloy_rpc_types_engine::PayloadId;
use morph_primitives::{Block, MorphPrimitives};
use reth_payload_primitives::BuiltPayload;
use reth_primitives_traits::SealedBlock;
use std::sync::Arc;

/// A built Morph payload.
///
/// This contains the result of payload building, including the sealed block,
/// collected fees, and the L2-specific executable data for API compatibility.
#[derive(Debug, Clone)]
pub struct MorphBuiltPayload {
    /// Payload ID.
    pub id: PayloadId,

    /// The built and sealed block.
    pub block: Arc<SealedBlock<Block>>,

    /// Total fees collected from transactions.
    pub fees: U256,

    /// L2 executable data for compatibility with L2 Engine API.
    pub executable_data: ExecutableL2Data,
}

impl MorphBuiltPayload {
    /// Create a new [`MorphBuiltPayload`].
    pub fn new(
        id: PayloadId,
        block: Arc<SealedBlock<Block>>,
        fees: U256,
        executable_data: ExecutableL2Data,
    ) -> Self {
        Self {
            id,
            block,
            fees,
            executable_data,
        }
    }

    /// Returns the block hash.
    pub fn block_hash(&self) -> alloy_primitives::B256 {
        self.block.hash()
    }

    /// Returns the block number.
    pub fn block_number(&self) -> u64 {
        self.block.header().number()
    }

    /// Returns the gas used by the block.
    pub fn gas_used(&self) -> u64 {
        self.executable_data.gas_used
    }

    /// Returns a reference to the executable L2 data.
    pub fn executable_data(&self) -> &ExecutableL2Data {
        &self.executable_data
    }
}

impl BuiltPayload for MorphBuiltPayload {
    type Primitives = MorphPrimitives;

    fn block(&self) -> &SealedBlock<Block> {
        &self.block
    }

    fn fees(&self) -> U256 {
        self.fees
    }

    fn requests(&self) -> Option<Requests> {
        // L2 blocks don't have EIP-7685 requests
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use morph_primitives::{BlockBody, MorphHeader};
    use reth_primitives_traits::Block as _;

    fn create_test_block() -> SealedBlock<Block> {
        let header: MorphHeader = Header::default().into();
        let body = BlockBody::default();
        let block = Block::new(header, body);
        block.seal_slow()
    }

    #[test]
    fn test_built_payload_new() {
        let id = PayloadId::new([0u8; 8]);
        let block = Arc::new(create_test_block());
        let fees = U256::from(1000);
        let executable_data = ExecutableL2Data::default();

        let payload = MorphBuiltPayload::new(id, block.clone(), fees, executable_data);

        assert_eq!(payload.id, id);
        assert_eq!(payload.fees(), fees);
        assert_eq!(payload.block_hash(), block.hash());
    }

    #[test]
    fn test_built_payload_accessors() {
        let id = PayloadId::new([1u8; 8]);
        let block = Arc::new(create_test_block());
        let executable_data = ExecutableL2Data {
            gas_used: 21000,
            ..Default::default()
        };

        let payload = MorphBuiltPayload::new(id, block, U256::ZERO, executable_data);

        assert_eq!(payload.gas_used(), 21000);
        assert_eq!(payload.block_number(), 0);
    }
}
