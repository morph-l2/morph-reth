use alloy_evm::{
    Database, Evm, EvmEnv, EvmFactory,
    precompiles::PrecompilesMap,
    revm::{
        Context, ExecuteEvm, InspectEvm, Inspector, SystemCallEvm,
        context::result::{EVMError, ResultAndState},
        inspector::NoOpInspector,
    },
};
use alloy_primitives::{Address, Bytes};
use morph_chainspec::hardfork::MorphHardfork;
use morph_revm::{MorphHaltReason, MorphInvalidTransaction, MorphTxEnv, evm::MorphContext};
use reth_revm::MainContext;
use std::ops::{Deref, DerefMut};

use crate::MorphBlockEnv;

#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct MorphEvmFactory;

impl EvmFactory for MorphEvmFactory {
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = MorphEvm<DB, I>;
    type Context<DB: Database> = MorphContext<DB>;
    type Tx = MorphTxEnv;
    type Error<DBError: std::error::Error + Send + Sync + 'static> =
        EVMError<DBError, MorphInvalidTransaction>;
    type HaltReason = MorphHaltReason;
    type Spec = MorphHardfork;
    type BlockEnv = MorphBlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector> {
        MorphEvm::new(db, input)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        MorphEvm::new(db, input).with_inspector(inspector)
    }
}

/// Morph EVM implementation.
///
/// This is a wrapper type around the `revm` ethereum evm with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
/// `RevmEvm` type.
#[expect(missing_debug_implementations)]
pub struct MorphEvm<DB: Database, I = NoOpInspector> {
    inner: morph_revm::MorphEvm<DB, I>,
    /// Precompiles wrapped as PrecompilesMap for reth compatibility
    precompiles_map: PrecompilesMap,
    inspect: bool,
}

impl<DB: Database> MorphEvm<DB> {
    /// Create a new [`MorphEvm`] instance.
    pub fn new(db: DB, input: EvmEnv<MorphHardfork, MorphBlockEnv>) -> Self {
        let ctx = Context::mainnet()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .with_tx(Default::default())
            .with_chain(morph_revm::l1block::L1BlockInfo::default());

        // Build the inner MorphEvm which creates precompiles once.
        // Derive the PrecompilesMap from the inner's precompiles to avoid
        // a second MorphPrecompiles::new_with_spec call.
        let inner = morph_revm::MorphEvm::new(ctx, NoOpInspector {});
        let precompiles_map = PrecompilesMap::from_static(inner.precompiles.precompiles());

        Self {
            inner,
            precompiles_map,
            inspect: false,
        }
    }
}

impl<DB: Database, I> MorphEvm<DB, I> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &MorphContext<DB> {
        &self.inner.inner.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut MorphContext<DB> {
        &mut self.inner.inner.ctx
    }

    /// Sets the inspector for the EVM.
    pub fn with_inspector<OINSP>(self, inspector: OINSP) -> MorphEvm<DB, OINSP> {
        MorphEvm {
            inner: self.inner.with_inspector(inspector),
            precompiles_map: self.precompiles_map,
            inspect: true,
        }
    }

    /// Returns the cached token fee info from the handler's validation phase.
    ///
    /// Avoids redundant DB reads when the block executor needs token fee
    /// parameters (e.g., for receipt construction).
    #[inline]
    pub fn cached_token_fee_info(&self) -> Option<morph_revm::TokenFeeInfo> {
        self.inner.cached_token_fee_info()
    }

    /// Returns the L1 data fee cached during handler validation.
    ///
    /// Avoids re-encoding the full transaction RLP in the block executor's
    /// receipt-building path.
    #[inline]
    pub fn cached_l1_data_fee(&self) -> alloy_primitives::U256 {
        self.inner.cached_l1_data_fee()
    }

    /// Takes the cached pre-execution fee logs (token fee deduction Transfer events).
    #[inline]
    pub fn take_pre_fee_logs(&mut self) -> Vec<alloy_primitives::Log> {
        self.inner.take_pre_fee_logs()
    }

    /// Takes the cached post-execution fee logs (token fee reimbursement Transfer events).
    #[inline]
    pub fn take_post_fee_logs(&mut self) -> Vec<alloy_primitives::Log> {
        self.inner.take_post_fee_logs()
    }
}

impl<DB: Database, I> Deref for MorphEvm<DB, I>
where
    DB: Database,
    I: Inspector<MorphContext<DB>>,
{
    type Target = MorphContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for MorphEvm<DB, I>
where
    DB: Database,
    I: Inspector<MorphContext<DB>>,
{
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I> Evm for MorphEvm<DB, I>
where
    DB: Database,
    I: Inspector<MorphContext<DB>>,
{
    type DB = DB;
    type Tx = MorphTxEnv;
    type Error = EVMError<DB::Error, MorphInvalidTransaction>;
    type HaltReason = MorphHaltReason;
    type Spec = MorphHardfork;
    type BlockEnv = MorphBlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &Self::BlockEnv {
        &self.block
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect {
            self.inner.inspect_tx(tx)
        } else {
            self.inner.transact(tx)
        }
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.system_call_with_caller(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
        let Context {
            block: block_env,
            cfg: cfg_env,
            journaled_state,
            ..
        } = self.inner.inner.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (
            &self.inner.inner.ctx.journaled_state.database,
            &self.inner.inner.inspector,
            &self.precompiles_map,
        )
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.inner.ctx.journaled_state.database,
            &mut self.inner.inner.inspector,
            &mut self.precompiles_map,
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use reth_revm::context::BlockEnv;
    use revm::{
        context::TxEnv,
        database::{CacheDB, EmptyDB},
        state::AccountInfo,
    };

    use super::*;

    #[test]
    fn can_execute_tx() {
        // Create a database with the caller having sufficient balance
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            Address::ZERO,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );

        let mut evm = MorphEvm::new(
            db,
            EvmEnv {
                block_env: MorphBlockEnv {
                    inner: BlockEnv {
                        basefee: 1,
                        ..Default::default()
                    },
                },
                ..Default::default()
            },
        );
        let result = evm
            .transact(MorphTxEnv {
                inner: TxEnv {
                    caller: Address::ZERO,
                    gas_price: 1, // Must be >= basefee
                    gas_limit: 21000,
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();

        assert!(result.result.is_success());
    }
}
