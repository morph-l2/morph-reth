//! Morph transaction-scoped runtime state.
//!
//! This module stores per-transaction execution metadata that should not live in
//! the transaction input (`MorphTxEnv`) or fee-oracle data structures.

use alloy_primitives::{Address, U256};
use revm::{primitives::HashMap, state::EvmState};

/// Execution-scoped state for a single Morph transaction.
///
/// The only tracked data today is the set of storage slots that Morph
/// intentionally re-marks cold after token-fee deduction. revm may later warm
/// these slots again and overwrite their `original_value`, so we retain the true
/// tx-original value here and restore it on every affected access path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MorphTxRuntime {
    forced_cold_slot_originals: HashMap<(Address, U256), U256>,
}

impl MorphTxRuntime {
    /// Clears all transaction-scoped state.
    #[inline]
    pub fn reset(&mut self) {
        self.forced_cold_slot_originals.clear();
    }

    /// Records the true tx-original value for a slot that will be marked cold.
    #[inline]
    pub fn track_forced_cold_slot(&mut self, address: Address, slot: U256, original_value: U256) {
        self.forced_cold_slot_originals
            .entry((address, slot))
            .or_insert(original_value);
    }

    /// Reads a slot's original value from the current journal state and tracks it.
    #[inline]
    pub fn track_forced_cold_slot_from_state(
        &mut self,
        state: &EvmState,
        address: Address,
        slot: U256,
    ) -> Option<U256> {
        let original_value = state
            .get(&address)
            .and_then(|account| account.storage.get(&slot))
            .map(|slot_state| slot_state.original_value)?;

        self.track_forced_cold_slot(address, slot, original_value);
        Some(original_value)
    }

    /// Returns the tracked tx-original value for a forced-cold slot, if any.
    #[inline]
    pub fn tracked_original_value(&self, address: Address, slot: U256) -> Option<U256> {
        self.forced_cold_slot_originals
            .get(&(address, slot))
            .copied()
    }

    /// Restores a tracked tx-original value into journal state.
    #[inline]
    pub fn restore_tracked_original_value(
        &self,
        state: &mut EvmState,
        address: Address,
        slot: U256,
    ) -> Option<U256> {
        let original_value = self.tracked_original_value(address, slot)?;
        let slot_state = state
            .get_mut(&address)
            .and_then(|account| account.storage.get_mut(&slot))?;
        slot_state.original_value = original_value;
        Some(original_value)
    }
}
