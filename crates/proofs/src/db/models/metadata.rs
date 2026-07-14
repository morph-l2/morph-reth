use bytes::BufMut;
use reth_codecs::DecompressError;
use reth_db::{
    DatabaseError,
    table::{Compress, Decode, Decompress, Encode},
};
use serde::{Deserialize, Serialize};

/// Durable proof-database metadata keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ProofMetadataKey {
    /// EIP-155 chain identifier.
    ChainId = 0,
    /// Canonical genesis block hash.
    GenesisHash = 1,
    /// Proof-database schema version.
    SchemaVersion = 2,
}

impl Encode for ProofMetadataKey {
    type Encoded = [u8; 1];

    fn encode(self) -> Self::Encoded {
        [self as u8]
    }
}

impl Decode for ProofMetadataKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        match value.first() {
            Some(0) => Ok(Self::ChainId),
            Some(1) => Ok(Self::GenesisHash),
            Some(2) => Ok(Self::SchemaVersion),
            _ => Err(DatabaseError::Decode),
        }
    }
}

/// Opaque metadata bytes whose shape is validated by the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofMetadataValue(pub Vec<u8>);

impl Compress for ProofMetadataValue {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        buf.put_slice(&self.0);
    }
}

impl Decompress for ProofMetadataValue {
    fn decompress(value: &[u8]) -> Result<Self, DecompressError> {
        Ok(Self(value.to_vec()))
    }
}
