//! Morph EVM Handler implementation.

use std::fmt::Debug;

use revm::{
    context::{
        Cfg, ContextTr, JournalTr, Transaction,
        result::{EVMError, ExecutionResult},
    },
    handler::{
        EvmTr, FrameTr, Handler, MainnetHandler,
        pre_execution,
        validation,
    },
    inspector::{Inspector, InspectorHandler},
    interpreter::{
        InitialAndFloorGas,
        interpreter::EthInterpreter,
    },
};

use crate::{
    MorphEvm, MorphInvalidTransaction,
    error::MorphHaltReason,
    evm::MorphContext,
};

/// Morph EVM [`Handler`] implementation.
#[derive(Debug)]
pub struct MorphEvmHandler<DB, I> {
    /// Phantom data to avoid type inference issues.
    _phantom: core::marker::PhantomData<(DB, I)>,
}

impl<DB, I> MorphEvmHandler<DB, I> {
    /// Create a new [`MorphEvmHandler`] handler instance
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB, I> Default for MorphEvmHandler<DB, I> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB, I> Handler for MorphEvmHandler<DB, I>
where
    DB: alloy_evm::Database,
{
    type Evm = MorphEvm<DB, I>;
    type Error = EVMError<DB::Error, MorphInvalidTransaction>;
    type HaltReason = MorphHaltReason;

    #[inline]
    fn run(
        &mut self,
        evm: &mut Self::Evm,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        match self.run_without_catch_error(evm) {
            Ok(output) => Ok(output),
            Err(err) => self.catch_error(evm, err),
        }
    }

    #[inline]
    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        evm.logs.clear();
        if !result.instruction_result().is_ok() {
            evm.logs = evm.journal_mut().take_logs();
        }

        MainnetHandler::default()
            .execution_result(evm, result)
            .map(|result| result.map_haltreason(Into::into))
    }

    #[inline]
    fn apply_eip7702_auth_list(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        pre_execution::apply_eip7702_auth_list(evm.ctx())
    }

    #[inline]
    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<(), Self::Error> {
        let (_, tx, cfg, journal, _, _) = evm.ctx().all_mut();

        // Load caller's account
        let mut caller_account = journal.load_account_with_code_mut(tx.caller())?.data;

        // Validate account nonce and code (EIP-3607) using upstream helper
        pre_execution::validate_account_nonce_and_code(
            &caller_account.info,
            tx.nonce(),
            cfg.is_eip3607_disabled(),
            cfg.is_nonce_check_disabled(),
        )?;

        // Modify account nonce and touch the account.
        caller_account.touch();

        // Bump the nonce for calls. Nonce for CREATE will be bumped in `make_create_frame`.
        if tx.kind().is_call() {
            caller_account.bump_nonce();
        }

        Ok(())
    }

    fn reimburse_caller(
        &self,
        _evm: &mut Self::Evm,
        _exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    fn reward_beneficiary(
        &self,
        _evm: &mut Self::Evm,
        _exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        validation::validate_env::<_, Self::Error>(evm.ctx())?;
        Ok(())
    }

    #[inline]
    fn validate_initial_tx_gas(&self, evm: &Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        let tx = evm.ctx_ref().tx();
        let spec = evm.ctx_ref().cfg().spec().into();
        Ok(
            validation::validate_initial_tx_gas(tx, spec, evm.ctx.cfg.is_eip7623_disabled())
                .map_err(MorphInvalidTransaction::EthInvalidTransaction)?,
        )
    }

    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        MainnetHandler::default()
            .catch_error(evm, error)
            .map(|result| result.map_haltreason(Into::into))
    }
}

impl<DB, I> InspectorHandler for MorphEvmHandler<DB, I>
where
    DB: alloy_evm::Database,
    I: Inspector<MorphContext<DB>>,
{
    type IT = EthInterpreter;

    fn inspect_run(
        &mut self,
        evm: &mut Self::Evm,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        match self.inspect_run_without_catch_error(evm) {
            Ok(output) => Ok(output),
            Err(e) => self.catch_error(evm, e),
        }
    }
}
