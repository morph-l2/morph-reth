//! Receipt envelope types for Morph.

use crate::transaction::envelope::MorphTxType;
use std::vec::Vec;

use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom, TxReceipt};
use alloy_eips::{
    eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718},
    Typed2718,
};
use alloy_primitives::{logs_bloom, Bloom, Log};
use alloy_rlp::{length_of_length, BufMut, Decodable, Encodable};

/// Receipt envelope, as defined in [EIP-2718], modified for Morph chains.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type"))]
#[non_exhaustive]
pub enum MorphReceiptEnvelope<T = Log> {
    /// Receipt envelope with no type flag.
    #[cfg_attr(feature = "serde", serde(rename = "0x0", alias = "0x00"))]
    Legacy(ReceiptWithBloom<Receipt<T>>),
    /// Receipt envelope with type flag 1, containing a [EIP-2930] receipt.
    #[cfg_attr(feature = "serde", serde(rename = "0x1", alias = "0x01"))]
    Eip2930(ReceiptWithBloom<Receipt<T>>),
    /// Receipt envelope with type flag 2, containing a [EIP-1559] receipt.
    #[cfg_attr(feature = "serde", serde(rename = "0x2", alias = "0x02"))]
    Eip1559(ReceiptWithBloom<Receipt<T>>),
    /// Receipt envelope with type flag 4, containing a [EIP-7702] receipt.
    #[cfg_attr(feature = "serde", serde(rename = "0x4", alias = "0x04"))]
    Eip7702(ReceiptWithBloom<Receipt<T>>),
    /// Receipt envelope with type flag 126, containing a Morph L1 message receipt.
    #[cfg_attr(feature = "serde", serde(rename = "0x7e", alias = "0x7E"))]
    L1Message(ReceiptWithBloom<Receipt<T>>),
    /// Receipt envelope with type flag 127, containing a Morph transaction receipt.
    #[cfg_attr(feature = "serde", serde(rename = "0x7f", alias = "0x7F"))]
    Morph(ReceiptWithBloom<Receipt<T>>),
}

impl MorphReceiptEnvelope<Log> {
    /// Creates a new [`MorphReceiptEnvelope`] from the given parts.
    pub fn from_parts<'a>(
        status: bool,
        cumulative_gas_used: u64,
        logs: impl IntoIterator<Item = &'a Log>,
        tx_type: MorphTxType,
    ) -> Self {
        let logs = logs.into_iter().cloned().collect::<Vec<_>>();
        let logs_bloom = logs_bloom(&logs);
        let inner_receipt =
            Receipt { status: Eip658Value::Eip658(status), cumulative_gas_used, logs };
        let with_bloom = ReceiptWithBloom { receipt: inner_receipt, logs_bloom };
        match tx_type {
            MorphTxType::Legacy => Self::Legacy(with_bloom),
            MorphTxType::Eip2930 => Self::Eip2930(with_bloom),
            MorphTxType::Eip1559 => Self::Eip1559(with_bloom),
            MorphTxType::Eip7702 => Self::Eip7702(with_bloom),
            MorphTxType::L1Msg => Self::L1Message(with_bloom),
            MorphTxType::Morph => Self::Morph(with_bloom),
        }
    }
}

impl<T> MorphReceiptEnvelope<T> {
    /// Return the [`MorphTxType`] of the inner receipt.
    pub const fn tx_type(&self) -> MorphTxType {
        match self {
            Self::Legacy(_) => MorphTxType::Legacy,
            Self::Eip2930(_) => MorphTxType::Eip2930,
            Self::Eip1559(_) => MorphTxType::Eip1559,
            Self::Eip7702(_) => MorphTxType::Eip7702,
            Self::L1Message(_) => MorphTxType::L1Msg,
            Self::Morph(_) => MorphTxType::Morph,
        }
    }

    /// Returns the success status of the receipt's transaction.
    pub const fn status(&self) -> bool {
        self.as_receipt().unwrap().status.coerce_status()
    }

    /// Return true if the transaction was successful.
    pub const fn is_success(&self) -> bool {
        self.status()
    }

    /// Returns the cumulative gas used at this receipt.
    pub const fn cumulative_gas_used(&self) -> u64 {
        self.as_receipt().unwrap().cumulative_gas_used
    }

    /// Return the receipt logs.
    pub fn logs(&self) -> &[T] {
        &self.as_receipt().unwrap().logs
    }

    /// Return the receipt's bloom.
    pub const fn logs_bloom(&self) -> &Bloom {
        match self {
            Self::Legacy(t)
            | Self::Eip2930(t)
            | Self::Eip1559(t)
            | Self::Eip7702(t)
            | Self::L1Message(t)
            | Self::Morph(t) => &t.logs_bloom,
        }
    }

    /// Returns the L1 message receipt if it is a deposit receipt.
    pub const fn as_l1_message_receipt_with_bloom(
        &self,
    ) -> Option<&ReceiptWithBloom<Receipt<T>>> {
        match self {
            Self::L1Message(t) => Some(t),
            _ => None,
        }
    }

    /// Returns the L1 message receipt if it is a deposit receipt.
    pub const fn as_l1_message_receipt(&self) -> Option<&Receipt<T>> {
        match self {
            Self::L1Message(t) => Some(&t.receipt),
            _ => None,
        }
    }

    /// Return the inner receipt. Currently this is infallible, however, future
    /// receipt types may be added.
    pub const fn as_receipt(&self) -> Option<&Receipt<T>> {
        match self {
            Self::Legacy(t)
            | Self::Eip2930(t)
            | Self::Eip1559(t)
            | Self::Eip7702(t)
            | Self::L1Message(t)
            | Self::Morph(t) => Some(&t.receipt),
        }
    }
}

impl MorphReceiptEnvelope {
    /// Get the length of the inner receipt in the 2718 encoding.
    pub fn inner_length(&self) -> usize {
        match self {
            Self::Legacy(t)
            | Self::Eip2930(t)
            | Self::Eip1559(t)
            | Self::Eip7702(t)
            | Self::L1Message(t)
            | Self::Morph(t) => t.length(),
        }
    }

    /// Calculate the length of the rlp payload of the network encoded receipt.
    pub fn rlp_payload_length(&self) -> usize {
        let length = self.inner_length();
        match self {
            Self::Legacy(_) => length,
            _ => length + 1,
        }
    }
}

impl<T> TxReceipt for MorphReceiptEnvelope<T>
where
    T: Clone + core::fmt::Debug + PartialEq + Eq + Send + Sync,
{
    type Log = T;

    fn status_or_post_state(&self) -> Eip658Value {
        self.as_receipt().unwrap().status
    }

    fn status(&self) -> bool {
        self.as_receipt().unwrap().status.coerce_status()
    }

    fn bloom(&self) -> Bloom {
        *self.logs_bloom()
    }

    fn bloom_cheap(&self) -> Option<Bloom> {
        Some(self.bloom())
    }

    fn cumulative_gas_used(&self) -> u64 {
        self.as_receipt().unwrap().cumulative_gas_used
    }

    fn logs(&self) -> &[T] {
        &self.as_receipt().unwrap().logs
    }
}

impl Encodable for MorphReceiptEnvelope {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.network_encode(out)
    }

    fn length(&self) -> usize {
        let mut payload_length = self.rlp_payload_length();
        if !self.is_legacy() {
            payload_length += length_of_length(payload_length);
        }
        payload_length
    }
}

impl Decodable for MorphReceiptEnvelope {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::network_decode(buf)
            .map_or_else(|_| Err(alloy_rlp::Error::Custom("Unexpected type")), Ok)
    }
}

impl Encodable2718 for MorphReceiptEnvelope {
    fn type_flag(&self) -> Option<u8> {
        match self {
            Self::Legacy(_) => None,
            Self::Eip2930(_) => Some(MorphTxType::Eip2930 as u8),
            Self::Eip1559(_) => Some(MorphTxType::Eip1559 as u8),
            Self::Eip7702(_) => Some(MorphTxType::Eip7702 as u8),
            Self::L1Message(_) => Some(MorphTxType::L1Msg as u8),
            Self::Morph(_) => Some(MorphTxType::Morph as u8),
        }
    }

    fn encode_2718_len(&self) -> usize {
        self.inner_length() + !self.is_legacy() as usize
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        if let Some(ty) = self.type_flag() {
            out.put_u8(ty);
        }
        match self {
            Self::Legacy(t)
            | Self::Eip2930(t)
            | Self::Eip1559(t)
            | Self::Eip7702(t)
            | Self::L1Message(t)
            | Self::Morph(t) => t.encode(out),
        }
    }
}

impl Typed2718 for MorphReceiptEnvelope {
    fn ty(&self) -> u8 {
        let ty = match self {
            Self::Legacy(_) => MorphTxType::Legacy,
            Self::Eip2930(_) => MorphTxType::Eip2930,
            Self::Eip1559(_) => MorphTxType::Eip1559,
            Self::Eip7702(_) => MorphTxType::Eip7702,
            Self::L1Message(_) => MorphTxType::L1Msg,
            Self::Morph(_) => MorphTxType::Morph,
        };
        ty as u8
    }
}

impl Decodable2718 for MorphReceiptEnvelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        match ty.try_into().map_err(|_| Eip2718Error::UnexpectedType(ty))? {
            MorphTxType::Legacy => Err(
                alloy_rlp::Error::Custom("type-0 eip2718 receipts are not supported").into(),
            ),
            MorphTxType::Eip2930 => Ok(Self::Eip2930(Decodable::decode(buf)?)),
            MorphTxType::Eip1559 => Ok(Self::Eip1559(Decodable::decode(buf)?)),
            MorphTxType::Eip7702 => Ok(Self::Eip7702(Decodable::decode(buf)?)),
            MorphTxType::L1Msg => Ok(Self::L1Message(Decodable::decode(buf)?)),
            MorphTxType::Morph => Ok(Self::Morph(Decodable::decode(buf)?)),
        }
    }

    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(Self::Legacy(Decodable::decode(buf)?))
    }
}

impl From<crate::receipt::MorphReceipt> for MorphReceiptEnvelope<Log> {
    fn from(value: crate::receipt::MorphReceipt) -> Self {
        let (tx_type, inner) = match value {
            crate::receipt::MorphReceipt::Legacy(receipt) => (MorphTxType::Legacy, receipt.inner),
            crate::receipt::MorphReceipt::Eip2930(receipt) => (MorphTxType::Eip2930, receipt.inner),
            crate::receipt::MorphReceipt::Eip1559(receipt) => (MorphTxType::Eip1559, receipt.inner),
            crate::receipt::MorphReceipt::Eip7702(receipt) => (MorphTxType::Eip7702, receipt.inner),
            crate::receipt::MorphReceipt::Morph(receipt) => (MorphTxType::Morph, receipt.inner),
            crate::receipt::MorphReceipt::L1Msg(receipt) => (MorphTxType::L1Msg, receipt),
        };

        let logs_bloom = logs_bloom(&inner.logs);
        let with_bloom = ReceiptWithBloom { receipt: inner, logs_bloom };
        match tx_type {
            MorphTxType::Legacy => Self::Legacy(with_bloom),
            MorphTxType::Eip2930 => Self::Eip2930(with_bloom),
            MorphTxType::Eip1559 => Self::Eip1559(with_bloom),
            MorphTxType::Eip7702 => Self::Eip7702(with_bloom),
            MorphTxType::L1Msg => Self::L1Message(with_bloom),
            MorphTxType::Morph => Self::Morph(with_bloom),
        }
    }
}
