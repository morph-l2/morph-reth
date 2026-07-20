//! Canonical-chain read seam used by the reference-index runtime.

use crate::{CanonicalTip, ReferenceIndexError};
use alloy_consensus::{BlockHeader, Sealable};
use alloy_primitives::B256;
use morph_primitives::MorphTxEnvelope;
use reth_provider::{
    BlockHashReader, BlockIdReader, BlockReader, BlockReaderIdExt, HeaderProvider,
};
use std::ops::RangeInclusive;

/// Canonical block fields required to build the reference index.
#[derive(Debug, Clone)]
pub struct CanonicalBlock {
    /// Canonical block number.
    pub number: u64,
    /// Header hash observed for this canonical block.
    pub hash: B256,
    /// Header timestamp.
    pub timestamp: u64,
    /// Transactions in their canonical block order.
    pub transactions: Vec<MorphTxEnvelope>,
}

/// Minimal canonical-chain API consumed by the runtime.
pub trait CanonicalChain: Clone + Send + Sync + 'static {
    /// Return one internally consistent canonical head snapshot.
    fn head(&self) -> Result<CanonicalTip, ReferenceIndexError>;

    /// Return the canonical hash at `number`, if the block is retained.
    fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ReferenceIndexError>;

    /// Return the canonical timestamp at `number`, if the header is retained.
    fn block_timestamp(&self, number: u64) -> Result<Option<u64>, ReferenceIndexError>;

    /// Return the L1 finalized canonical block number, if finality is known.
    ///
    /// A finalized block can never be reorged, so this is the true lower bound
    /// for reorg rewind and the pruning floor for reorg breadcrumbs. `None`
    /// means finality is not yet established (fall back to the Jade start).
    fn finalized_block_number(&self) -> Result<Option<u64>, ReferenceIndexError>;

    /// Load an inclusive range of canonical blocks with their transaction bodies.
    fn canonical_blocks(
        &self,
        range: RangeInclusive<u64>,
    ) -> Result<Vec<CanonicalBlock>, ReferenceIndexError>;
}

impl<P> CanonicalChain for P
where
    P: BlockReaderIdExt<Block = morph_primitives::Block, Header = morph_primitives::MorphHeader>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn head(&self) -> Result<CanonicalTip, ReferenceIndexError> {
        let header = BlockReaderIdExt::latest_header(self)?
            .ok_or_else(|| ReferenceIndexError::Other(eyre::eyre!("missing canonical head")))?;
        Ok(CanonicalTip {
            number: header.number(),
            hash: header.hash(),
            timestamp: header.timestamp(),
        })
    }

    fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ReferenceIndexError> {
        Ok(BlockHashReader::block_hash(self, number)?)
    }

    fn block_timestamp(&self, number: u64) -> Result<Option<u64>, ReferenceIndexError> {
        Ok(HeaderProvider::header_by_number(self, number)?.map(|header| header.timestamp()))
    }

    fn finalized_block_number(&self) -> Result<Option<u64>, ReferenceIndexError> {
        Ok(BlockIdReader::finalized_block_number(self)?)
    }

    fn canonical_blocks(
        &self,
        range: RangeInclusive<u64>,
    ) -> Result<Vec<CanonicalBlock>, ReferenceIndexError> {
        BlockReader::block_range(self, range)?
            .into_iter()
            .map(|block| {
                let number = block.header.number();
                let timestamp = block.header.timestamp();
                let hash = block.header.hash_slow();
                Ok(CanonicalBlock {
                    number,
                    hash,
                    timestamp,
                    transactions: block.body.transactions,
                })
            })
            .collect()
    }
}
