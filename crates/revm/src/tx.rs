//! Morph transaction environment.
//!
//! This module defines the Morph-specific transaction environment.

use morph_primitives::L1_TX_TYPE_ID;
use revm::context::TxEnv;

/// Morph transaction environment.
///
/// An alias for [`TxEnv`] for backwards compatibility.
pub type MorphTxEnv = TxEnv;

/// Extension trait for [`TxEnv`] to support Morph-specific functionality.
pub trait MorphTxExt {
    /// Returns whether this transaction is an L1 message transaction (type 0x7E).
    fn is_l1_msg(&self) -> bool;
}

impl MorphTxExt for TxEnv {
    #[inline]
    fn is_l1_msg(&self) -> bool {
        self.tx_type == L1_TX_TYPE_ID
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l1_msg_detection() {
        let mut tx = TxEnv::default();
        tx.tx_type = L1_TX_TYPE_ID;
        assert!(tx.is_l1_msg());

        let regular_tx = TxEnv::default();
        assert!(!regular_tx.is_l1_msg());
    }
}
