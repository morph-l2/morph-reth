use alloy_consensus::{Sealed, Signed, TransactionEnvelope, TxEip1559, TxEip2930, TxEip7702, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{B256, Bytes};
use alloy_rlp::BytesMut;

use crate::{TxAltFee, TxL1Msg};

#[derive(Debug, Clone, TransactionEnvelope)]
#[envelope(tx_type_name = MorphTxType)]
pub enum MorphTxEnvelope {
    /// Legacy transaction (type 0x00)
    #[envelope(ty = 0)]
    Legacy(Signed<TxLegacy>),

    /// EIP-2930 access list transaction (type 0x01)
    #[envelope(ty = 1)]
    Eip2930(Signed<TxEip2930>),

    /// EIP-1559 dynamic fee transaction (type 0x02)
    #[envelope(ty = 2)]
    Eip1559(Signed<TxEip1559>),

    /// EIP-7702 authorization list transaction (type 0x04)
    #[envelope(ty = 4)]
    Eip7702(Signed<TxEip7702>),

    /// Layer1 Message Transaction
    #[envelope(ty = 0x7e)]
    L1Msg(Sealed<TxL1Msg>),

    /// Alt Fee Transaction
    #[envelope(ty = 0x7f)]
    AltFee(Signed<TxAltFee>),
}

impl MorphTxEnvelope {
    /// Return the [`MorphTxType`] of the inner txn.
    pub const fn tx_type(&self) -> MorphTxType {
        match self {
            Self::Legacy(_) => MorphTxType::Legacy,
            Self::Eip2930(_) => MorphTxType::Eip2930,
            Self::Eip1559(_) => MorphTxType::Eip1559,
            Self::Eip7702(_) => MorphTxType::Eip7702,
            Self::L1Msg(_) => MorphTxType::L1Msg,
            Self::AltFee(_) => MorphTxType::AltFee,
        }
    }

    /// Recovers the signer of the transaction without validating the signature.
    ///
    /// This is faster than validating the signature first, but should only be used
    /// when the signature is already known to be valid.
    pub fn signer_unchecked(
        &self,
    ) -> Result<alloy_primitives::Address, alloy_consensus::crypto::RecoveryError> {
        alloy_consensus::transaction::SignerRecoverable::recover_signer_unchecked(self)
    }

    pub fn is_l1_msg(&self) -> bool {
        self.tx_type() == MorphTxType::L1Msg
    }

    pub fn queue_index(&self) -> Option<u64> {
        match self {
            Self::Legacy(_)
            | Self::Eip2930(_)
            | Self::Eip1559(_)
            | Self::Eip7702(_)
            | Self::AltFee(_) => None,
            Self::L1Msg(tx) => Some(tx.queue_index),
        }
    }

    /// Encode the transaction according to [EIP-2718] rules. First a 1-byte
    /// type flag in the range 0x0-0x7f, then the body of the transaction.
    pub fn rlp(&self) -> Bytes {
        let mut bytes = BytesMut::new();
        match self {
            Self::Legacy(tx) => tx.encode_2718(&mut bytes),
            Self::Eip2930(tx) => tx.encode_2718(&mut bytes),
            Self::Eip1559(tx) => tx.encode_2718(&mut bytes),
            Self::Eip7702(tx) => tx.encode_2718(&mut bytes),
            Self::L1Msg(tx) => tx.encode_2718(&mut bytes),
            Self::AltFee(tx) => tx.encode_2718(&mut bytes),
        }
        Bytes(bytes.freeze())
    }
}

impl reth_primitives_traits::InMemorySize for MorphTxEnvelope {
    fn size(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.size(),
            Self::Eip2930(tx) => tx.size(),
            Self::Eip1559(tx) => tx.size(),
            Self::Eip7702(tx) => tx.size(),
            Self::L1Msg(tx) => tx.size(),
            Self::AltFee(tx) => tx.size(),
        }
    }
}

impl reth_primitives_traits::InMemorySize for MorphTxType {
    fn size(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl MorphTxType {
    /// Returns `true` if this is a legacy transaction.
    pub const fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy)
    }

    /// Decodes the transaction type from the buffer.
    pub fn rlp_decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        use alloy_rlp::Decodable;
        let ty = u8::decode(buf)?;
        Self::try_from(ty).map_err(|_| alloy_rlp::Error::Custom("unknown tx type"))
    }
}

impl alloy_consensus::transaction::TxHashRef for MorphTxEnvelope {
    fn tx_hash(&self) -> &B256 {
        match self {
            Self::Legacy(tx) => tx.hash(),
            Self::Eip2930(tx) => tx.hash(),
            Self::Eip1559(tx) => tx.hash(),
            Self::Eip7702(tx) => tx.hash(),
            Self::L1Msg(tx) => tx.hash_ref(),
            Self::AltFee(tx) => tx.hash(),
        }
    }
}

impl alloy_consensus::transaction::SignerRecoverable for MorphTxEnvelope {
    fn recover_signer(
        &self,
    ) -> Result<alloy_primitives::Address, alloy_consensus::crypto::RecoveryError> {
        match self {
            Self::Legacy(tx) => alloy_consensus::transaction::SignerRecoverable::recover_signer(tx),
            Self::Eip2930(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_signer(tx)
            }
            Self::Eip1559(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_signer(tx)
            }
            Self::Eip7702(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_signer(tx)
            }
            // L1 messages have no signature - the sender is stored in the transaction itself
            Self::L1Msg(tx) => Ok(tx.sender),
            Self::AltFee(tx) => alloy_consensus::transaction::SignerRecoverable::recover_signer(tx),
        }
    }

    fn recover_signer_unchecked(
        &self,
    ) -> Result<alloy_primitives::Address, alloy_consensus::crypto::RecoveryError> {
        match self {
            Self::Legacy(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_signer_unchecked(tx)
            }
            Self::Eip2930(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_signer_unchecked(tx)
            }
            Self::Eip1559(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_signer_unchecked(tx)
            }
            Self::Eip7702(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_signer_unchecked(tx)
            }
            // L1 messages have no signature - the sender is stored in the transaction itself
            Self::L1Msg(tx) => Ok(tx.sender),
            Self::AltFee(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_signer_unchecked(tx)
            }
        }
    }
}

impl reth_primitives_traits::SignedTransaction for MorphTxEnvelope {}

#[cfg(feature = "reth-codec")]
mod codec {
    use crate::ALT_FEE_TX_TYPE_ID;
    use crate::L1_TX_TYPE_ID;
    use crate::TxAltFee;
    use crate::TxL1Msg;

    use super::*;
    use alloy_eips::eip2718::EIP7702_TX_TYPE_ID;
    use alloy_primitives::{
        Signature,
        bytes::{self, BufMut},
    };
    use reth_codecs::{
        Compact,
        alloy::transaction::{CompactEnvelope, Envelope},
        txtype::{
            COMPACT_EXTENDED_IDENTIFIER_FLAG, COMPACT_IDENTIFIER_EIP1559,
            COMPACT_IDENTIFIER_EIP2930, COMPACT_IDENTIFIER_LEGACY,
        },
    };

    impl reth_codecs::alloy::transaction::FromTxCompact for MorphTxEnvelope {
        type TxType = MorphTxType;

        fn from_tx_compact(
            buf: &[u8],
            tx_type: Self::TxType,
            signature: Signature,
        ) -> (Self, &[u8]) {
            use alloy_consensus::Signed;
            use reth_codecs::Compact;

            match tx_type {
                MorphTxType::Legacy => {
                    let (tx, buf) = TxLegacy::from_compact(buf, buf.len());
                    let tx = Signed::new_unhashed(tx, signature);
                    (Self::Legacy(tx), buf)
                }
                MorphTxType::Eip2930 => {
                    let (tx, buf) = TxEip2930::from_compact(buf, buf.len());
                    let tx = Signed::new_unhashed(tx, signature);
                    (Self::Eip2930(tx), buf)
                }
                MorphTxType::Eip1559 => {
                    let (tx, buf) = TxEip1559::from_compact(buf, buf.len());
                    let tx = Signed::new_unhashed(tx, signature);
                    (Self::Eip1559(tx), buf)
                }
                MorphTxType::Eip7702 => {
                    let (tx, buf) = TxEip7702::from_compact(buf, buf.len());
                    let tx = Signed::new_unhashed(tx, signature);
                    (Self::Eip7702(tx), buf)
                }
                MorphTxType::L1Msg => {
                    let (tx, buf) = TxL1Msg::from_compact(buf, buf.len());
                    // L1 messages have no signature - use Sealed instead of Signed
                    let tx = Sealed::new(tx);
                    (Self::L1Msg(tx), buf)
                }
                MorphTxType::AltFee => {
                    let (tx, buf) = TxAltFee::from_compact(buf, buf.len());
                    let tx = Signed::new_unhashed(tx, signature);
                    (Self::AltFee(tx), buf)
                }
            }
        }
    }

    impl reth_codecs::alloy::transaction::ToTxCompact for MorphTxEnvelope {
        fn to_tx_compact(&self, buf: &mut (impl BufMut + AsMut<[u8]>)) {
            match self {
                Self::Legacy(tx) => tx.tx().to_compact(buf),
                Self::Eip2930(tx) => tx.tx().to_compact(buf),
                Self::Eip1559(tx) => tx.tx().to_compact(buf),
                Self::Eip7702(tx) => tx.tx().to_compact(buf),
                Self::L1Msg(tx) => tx.to_compact(buf),
                Self::AltFee(tx) => tx.tx().to_compact(buf),
            };
        }
    }

    /// Dummy signature for L1 messages (which have no signature).
    const L1_MSG_SIGNATURE: Signature = Signature::new(
        alloy_primitives::U256::ZERO,
        alloy_primitives::U256::ZERO,
        false,
    );

    impl Envelope for MorphTxEnvelope {
        fn signature(&self) -> &Signature {
            match self {
                Self::Legacy(tx) => tx.signature(),
                Self::Eip2930(tx) => tx.signature(),
                Self::Eip1559(tx) => tx.signature(),
                Self::Eip7702(tx) => tx.signature(),
                Self::L1Msg(_) => &L1_MSG_SIGNATURE,
                Self::AltFee(tx) => tx.signature(),
            }
        }

        fn tx_type(&self) -> Self::TxType {
            Self::tx_type(self)
        }
    }

    impl Compact for MorphTxType {
        fn to_compact<B>(&self, buf: &mut B) -> usize
        where
            B: BufMut + AsMut<[u8]>,
        {
            match self {
                Self::Legacy => COMPACT_IDENTIFIER_LEGACY,
                Self::Eip2930 => COMPACT_IDENTIFIER_EIP2930,
                Self::Eip1559 => COMPACT_IDENTIFIER_EIP1559,
                Self::Eip7702 => {
                    buf.put_u8(EIP7702_TX_TYPE_ID);
                    COMPACT_EXTENDED_IDENTIFIER_FLAG
                }
                Self::L1Msg => {
                    buf.put_u8(L1_TX_TYPE_ID);
                    COMPACT_EXTENDED_IDENTIFIER_FLAG
                }
                Self::AltFee => {
                    buf.put_u8(ALT_FEE_TX_TYPE_ID);
                    COMPACT_EXTENDED_IDENTIFIER_FLAG
                }
            }
        }

        // For backwards compatibility purposes only 2 bits of the type are encoded in the identifier
        // parameter. In the case of a [`COMPACT_EXTENDED_IDENTIFIER_FLAG`], the full transaction type
        // is read from the buffer as a single byte.
        fn from_compact(mut buf: &[u8], identifier: usize) -> (Self, &[u8]) {
            use bytes::Buf;
            (
                match identifier {
                    COMPACT_IDENTIFIER_LEGACY => Self::Legacy,
                    COMPACT_IDENTIFIER_EIP2930 => Self::Eip2930,
                    COMPACT_IDENTIFIER_EIP1559 => Self::Eip1559,
                    COMPACT_EXTENDED_IDENTIFIER_FLAG => {
                        let extended_identifier = buf.get_u8();
                        match extended_identifier {
                            EIP7702_TX_TYPE_ID => Self::Eip7702,
                            crate::transaction::L1_TX_TYPE_ID => Self::L1Msg,
                            crate::transaction::ALT_FEE_TX_TYPE_ID => Self::AltFee,
                            _ => panic!("Unsupported TxType identifier: {extended_identifier}"),
                        }
                    }
                    _ => panic!("Unknown identifier for TxType: {identifier}"),
                },
                buf,
            )
        }
    }

    impl Compact for MorphTxEnvelope {
        fn to_compact<B>(&self, buf: &mut B) -> usize
        where
            B: BufMut + AsMut<[u8]>,
        {
            CompactEnvelope::to_compact(self, buf)
        }

        fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
            CompactEnvelope::from_compact(buf, len)
        }
    }

    impl reth_db_api::table::Compress for MorphTxEnvelope {
        type Compressed = Vec<u8>;

        fn compress_to_buf<B: alloy_primitives::bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
            let _ = Compact::to_compact(self, buf);
        }
    }

    impl reth_db_api::table::Decompress for MorphTxEnvelope {
        fn decompress(value: &[u8]) -> Result<Self, reth_db_api::DatabaseError> {
            let (obj, _) = Compact::from_compact(value, value.len());
            Ok(obj)
        }
    }
}
