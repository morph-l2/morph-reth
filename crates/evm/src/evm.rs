//! Morph EVM instance and factory.

use alloy_consensus::transaction::Recovered;
use alloy_primitives::Address;
use evm2::{
    ExecutedTx, Inspector, TxResult, TxResultWithState,
    evm::{AccountInfo, Cache, DynDatabase, PendingState},
    precompiles::{MovePrecompileError, PrecompileId},
};
use morph_revm::{FeeFrameFilter, MorphEvmEnv, MorphEvmTypes, MorphTxEnv};
use reth_evm::{BlockExecutionError, Evm};
use std::ops::{Deref, DerefMut};

/// Marker for the EVM a [`crate::MorphEvmConfig`] creates.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct MorphEvmFactory;

/// A configured Morph EVM.
///
/// Wraps the evm2 instance so that every inspector installed on it goes through
/// [`FeeFrameFilter`]: tracers must see a token-fee transaction without its fee frames.
#[expect(missing_debug_implementations)]
pub struct MorphEvm<'a> {
    inner: morph_revm::MorphEvm<'a>,
}

impl<'a> MorphEvm<'a> {
    /// Creates a Morph EVM over `database`.
    pub fn new(database: impl DynDatabase + 'a, env: MorphEvmEnv) -> Self {
        Self {
            inner: morph_revm::morph_evm(database, env),
        }
    }
}

impl<'a> Deref for MorphEvm<'a> {
    type Target = morph_revm::MorphEvm<'a>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for MorphEvm<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'a> Evm for MorphEvm<'a> {
    type EvmTypes = MorphEvmTypes;
    type Transaction = MorphTxEnv;

    fn transact(
        &mut self,
        transaction: &Recovered<MorphTxEnv>,
    ) -> Result<TxResultWithState<MorphEvmTypes>, BlockExecutionError> {
        Evm::transact(&mut self.inner, transaction)
    }

    fn transact_commit(
        &mut self,
        transaction: &Recovered<MorphTxEnv>,
    ) -> Result<TxResult<MorphEvmTypes>, BlockExecutionError> {
        Evm::transact_commit(&mut self.inner, transaction)
    }

    fn transact_result(
        &mut self,
        transaction: &Recovered<MorphTxEnv>,
    ) -> Result<TxResult<MorphEvmTypes>, BlockExecutionError> {
        Evm::transact_result(&mut self.inner, transaction)
    }

    fn transact_with_inspector<I>(
        &mut self,
        transaction: &Recovered<MorphTxEnv>,
        inspector: I,
    ) -> Result<(I, TxResultWithState<MorphEvmTypes>), BlockExecutionError>
    where
        I: Inspector<MorphEvmTypes> + 'static,
    {
        let (filter, result) = Evm::transact_with_inspector(
            &mut self.inner,
            transaction,
            FeeFrameFilter::new(inspector),
        )?;
        Ok((filter.into_inner(), result))
    }

    fn set_inspector<I>(&mut self, inspector: I)
    where
        I: Inspector<MorphEvmTypes> + 'static,
    {
        Evm::set_inspector(&mut self.inner, FeeFrameFilter::new(inspector));
    }

    fn state_db_mut(&mut self) -> &mut dyn DynDatabase {
        Evm::state_db_mut(&mut self.inner)
    }

    fn commit_state(&mut self, state: &PendingState) {
        Evm::commit_state(&mut self.inner, state);
    }

    fn take_state_cache(&mut self) -> Cache {
        Evm::take_state_cache(&mut self.inner)
    }

    fn precompile_ids(&self) -> Vec<(Address, PrecompileId)> {
        Evm::precompile_ids(&self.inner)
    }

    fn has_precompile(&self, address: &Address) -> bool {
        Evm::has_precompile(&self.inner, address)
    }

    fn account_info(
        &mut self,
        address: &Address,
    ) -> Result<Option<AccountInfo>, BlockExecutionError> {
        Evm::account_info(&mut self.inner, address)
    }

    fn move_precompiles(
        &mut self,
        moves: impl IntoIterator<Item = (Address, Address)>,
    ) -> Result<(), MovePrecompileError> {
        Evm::move_precompiles(&mut self.inner, moves)
    }

    fn transact_and_discard<R>(
        &mut self,
        transaction: &Recovered<MorphTxEnv>,
        observe: impl FnOnce(&ExecutedTx<'_, '_, MorphEvmTypes>) -> R,
    ) -> Result<R, BlockExecutionError> {
        Evm::transact_and_discard(&mut self.inner, transaction, observe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{TxKind, U256};
    use evm2::evm::InMemoryDB;
    use morph_chainspec::hardfork::MorphHardfork;
    use morph_revm::{MorphBlockEnv, morph_evm_env};

    #[test]
    fn can_execute_tx() {
        // Create a database with the caller having sufficient balance
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            &Address::ZERO,
            AccountInfo::default().with_balance(U256::from(1_000_000)),
        );

        let env = morph_evm_env(
            MorphHardfork::default(),
            1,
            30_000_000,
            MorphBlockEnv {
                basefee: U256::from(1),
                ..Default::default()
            },
        );
        let mut evm = MorphEvm::new(db, env);
        let tx = Recovered::new_unchecked(
            MorphTxEnv {
                gas_price: 1, // Must be >= basefee
                gas_limit: 21000,
                kind: TxKind::Call(Address::with_last_byte(0xff)),
                ..Default::default()
            },
            Address::ZERO,
        );

        let result = Evm::transact(&mut evm, &tx).unwrap();

        assert!(result.result.status);
    }
}
