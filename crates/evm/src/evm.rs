//! Morph EVM instance and factory.

use alloy_consensus::transaction::Recovered;
use alloy_primitives::{Address, Log, U256};
use core::any::TypeId;
use evm2::{
    ExecutedTx, Inspector, InspectorConfig, TxResult, TxResultWithState,
    evm::{AccountInfo, Cache, DynDatabase, NonStaticAny, PendingState},
    interpreter::{Interpreter, Message, MessageResult},
    precompiles::{MovePrecompileError, PrecompileId},
};
use morph_revm::{FeeFrameFilter, MorphEvmEnv, MorphEvmTypes, MorphTxEnv};
use reth_evm::{BlockExecutionError, Evm};
use std::ops::{Deref, DerefMut};

/// Marker for the EVM a [`crate::MorphEvmConfig`] creates.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct MorphEvmFactory;

/// Private wrapper installed only by [`MorphEvm::set_inspector`] around a `'static` inspector.
///
/// Keeping construction private to this module (and gated on `I: 'static`) makes downcasting a
/// lifetime-erased `dyn Inspector + 'a` back to `InstalledInspector<I>` unconditionally sound even
/// if callers replace the inner inspector via [`DerefMut`].
#[derive(Debug)]
struct InstalledInspector<I>(FeeFrameFilter<I>);

impl<I: Inspector<MorphEvmTypes>> Inspector<MorphEvmTypes> for InstalledInspector<I> {
    #[inline]
    fn inspector_config(&self) -> InspectorConfig {
        self.0.inspector_config()
    }

    fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, MorphEvmTypes>) {
        self.0.initialize_interp(interp);
    }

    fn step(&mut self, interp: &mut Interpreter<'_, '_, MorphEvmTypes>) {
        self.0.step(interp);
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, MorphEvmTypes>) {
        self.0.step_end(interp);
    }

    fn log(&mut self, log: &Log, host: &mut morph_revm::MorphEvm<'_>) {
        self.0.log(log, host);
    }

    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, MorphEvmTypes>,
        message: &mut Message<MorphEvmTypes>,
    ) -> Option<MessageResult<MorphEvmTypes>> {
        self.0.call(interp, message)
    }

    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MorphEvmTypes>,
        message: &Message<MorphEvmTypes>,
        result: &mut MessageResult<MorphEvmTypes>,
    ) {
        self.0.call_end(interp, message, result);
    }

    fn create(
        &mut self,
        interp: &mut Interpreter<'_, '_, MorphEvmTypes>,
        message: &mut Message<MorphEvmTypes>,
    ) -> Option<MessageResult<MorphEvmTypes>> {
        self.0.create(interp, message)
    }

    fn create_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MorphEvmTypes>,
        message: &Message<MorphEvmTypes>,
        result: &mut MessageResult<MorphEvmTypes>,
    ) {
        self.0.create_end(interp, message, result);
    }

    fn selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        value: &U256,
        host: &mut morph_revm::MorphEvm<'_>,
    ) {
        self.0.selfdestruct(contract, target, value, host);
    }
}

type UnwrapInspectorFn<'a> =
    fn(&mut morph_revm::MorphEvm<'a>) -> Option<Box<dyn Inspector<MorphEvmTypes> + 'a>>;

fn unwrap_installed_inspector<'a, I>(
    evm: &mut morph_revm::MorphEvm<'a>,
) -> Option<Box<dyn Inspector<MorphEvmTypes> + 'a>>
where
    I: Inspector<MorphEvmTypes> + 'static,
{
    let inspector = evm.clear_inspector()?;
    let erased: &dyn NonStaticAny = &*inspector;
    if erased.type_id() != TypeId::of::<InstalledInspector<I>>() {
        return Some(inspector);
    }
    // SAFETY: `erased.type_id()` matches `InstalledInspector<I>`, and only
    // `MorphEvm::set_inspector` constructs `InstalledInspector`, always around a `'static`
    // inspector `I`, so the boxed value is an `InstalledInspector<I>` and no borrow is extended.
    let installed =
        unsafe { Box::from_raw(Box::into_raw(inspector).cast::<InstalledInspector<I>>()) };
    Some(Box::new(installed.0.into_inner()))
}

/// A configured Morph EVM.
///
/// Wraps the evm2 instance so that every inspector installed on it goes through
/// [`FeeFrameFilter`]: tracers must see a token-fee transaction without its fee frames.
#[expect(missing_debug_implementations)]
pub struct MorphEvm<'a> {
    inner: morph_revm::MorphEvm<'a>,
    unwrap_inspector: Option<UnwrapInspectorFn<'a>>,
}

impl<'a> MorphEvm<'a> {
    /// Creates a Morph EVM over `database`.
    pub fn new(database: impl DynDatabase + 'a, env: MorphEvmEnv) -> Self {
        Self {
            inner: morph_revm::morph_evm(database, env),
            unwrap_inspector: None,
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

/// Install inspectors through [`reth_evm::Evm::set_inspector`] rather than calling
/// [`evm2::Evm::set_inspector`] on the dereferenced inner EVM directly so [`FeeFrameFilter`] hides
/// internal ERC-20 fee frames.
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
        self.unwrap_inspector = None;
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
        Evm::set_inspector(
            &mut self.inner,
            InstalledInspector(FeeFrameFilter::new(inspector)),
        );
        self.unwrap_inspector = Some(unwrap_installed_inspector::<I>);
    }

    unsafe fn with_inspector<I, R>(&mut self, f: impl FnOnce(&mut I) -> R) -> Option<R>
    where
        I: Inspector<MorphEvmTypes> + 'static,
    {
        let inspector: &mut (dyn NonStaticAny + 'a) = &mut **self.inner.inspector_mut()?;
        let type_id = NonStaticAny::type_id(inspector);
        if type_id == TypeId::of::<InstalledInspector<I>>() {
            // SAFETY: `type_id` matches `InstalledInspector<I>`, and only `MorphEvm::set_inspector`
            // constructs `InstalledInspector`, always around `I: 'static`.
            let installed = unsafe { inspector.downcast_mut_unchecked::<InstalledInspector<I>>() };
            Some(f(installed.0.inner_mut()))
        } else if type_id == TypeId::of::<FeeFrameFilter<I>>() {
            // SAFETY: `type_id` matches `FeeFrameFilter<I>`, and the caller guarantees `I` has no
            // lifetime parameters, so `FeeFrameFilter<I>` has no lifetime parameters either.
            let filter = unsafe { inspector.downcast_mut_unchecked::<FeeFrameFilter<I>>() };
            Some(f(filter.inner_mut()))
        } else if type_id == TypeId::of::<I>() {
            // SAFETY: `type_id` matches `I`, and the caller guarantees `I` has no lifetime
            // parameters.
            Some(f(unsafe { inspector.downcast_mut_unchecked::<I>() }))
        } else {
            None
        }
    }

    fn take_inspector(&mut self) -> Option<Box<dyn Inspector<MorphEvmTypes> + '_>> {
        match self.unwrap_inspector.take() {
            Some(unwrap) => unwrap(&mut self.inner),
            None => Evm::take_inspector(&mut self.inner),
        }
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

    #[test]
    fn set_inspector_supports_with_inspector_and_take_inspector() {
        #[derive(Debug, Default)]
        struct CallCounter(usize);

        impl Inspector<MorphEvmTypes> for CallCounter {
            fn call(
                &mut self,
                _interp: &mut Interpreter<'_, '_, MorphEvmTypes>,
                _message: &mut Message<MorphEvmTypes>,
            ) -> Option<MessageResult<MorphEvmTypes>> {
                self.0 += 1;
                None
            }
        }

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
        Evm::set_inspector(&mut evm, CallCounter(0));

        let tx = Recovered::new_unchecked(
            MorphTxEnv {
                gas_price: 1,
                gas_limit: 21_000,
                kind: TxKind::Call(Address::with_last_byte(0xff)),
                ..Default::default()
            },
            Address::ZERO,
        );
        Evm::transact(&mut evm, &tx).unwrap();

        let count = unsafe { Evm::with_inspector::<CallCounter, _>(&mut evm, |i| i.0) };
        assert_eq!(count, Some(1));
        assert_eq!(
            unsafe { Evm::with_inspector::<evm2::NoopInspector, _>(&mut evm, |_| ()) },
            None
        );

        let taken = Evm::take_inspector(&mut evm).expect("inspector was installed");
        let erased: &dyn NonStaticAny = &*taken;
        assert_eq!(erased.type_id(), TypeId::of::<CallCounter>());
        drop(taken);
        assert!(Evm::take_inspector(&mut evm).is_none());
    }

    #[test]
    fn take_inspector_remains_sound_after_deref_mut_replacement() {
        #[expect(dead_code)]
        struct BorrowedInspector<'a>(&'a usize);

        impl Inspector<MorphEvmTypes> for BorrowedInspector<'_> {}

        let static_val: &'static usize = &42;
        let local_val = 7usize;

        let env = morph_evm_env(
            MorphHardfork::default(),
            1,
            30_000_000,
            MorphBlockEnv::default(),
        );
        let mut evm = MorphEvm::new(InMemoryDB::default(), env);

        Evm::set_inspector(&mut evm, BorrowedInspector(static_val));
        // Replace the inner inspector via DerefMut with a non-'static FeeFrameFilter wrapping the
        // same generic type with a shorter borrow.
        evm.deref_mut()
            .set_inspector(FeeFrameFilter::new(BorrowedInspector(&local_val)));

        let taken = Evm::take_inspector(&mut evm).expect("replacement inspector stays present");
        let erased: &dyn NonStaticAny = &*taken;
        assert_eq!(
            erased.type_id(),
            TypeId::of::<FeeFrameFilter<BorrowedInspector<'static>>>()
        );
    }
}
