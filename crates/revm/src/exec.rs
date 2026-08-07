use crate::{
    MorphBlockEnv, MorphInvalidTransaction, MorphTxEnv,
    error::MorphHaltReason,
    evm::{MorphContext, MorphEvm},
    handler::MorphEvmHandler,
};
use alloy_evm::{Database, TransactionEnvMut as _};
use revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
    context::{ContextSetters, TxEnv, result::ExecResultAndState},
    context_interface::{
        ContextTr, JournalTr,
        result::{EVMError, ExecutionResult},
    },
    handler::{Handler, SystemCallTx, system_call::SystemCallEvm},
    inspector::{InspectCommitEvm, InspectEvm, InspectSystemCallEvm, Inspector, InspectorHandler},
    primitives::{Address, Bytes},
    state::EvmState,
};

/// Total gas system transactions are allowed to use.
const SYSTEM_CALL_GAS_LIMIT: u64 = 200_000;

impl<DB: Database, I> MorphEvm<DB, I> {
    /// Clears the per-transaction caches after the handler rejected a transaction.
    ///
    /// The handler's `catch_error` already discarded the journal, the frame stack
    /// and the local context, so only Morph's own side-channel caches — the fee
    /// logs and the sweep state that live outside the journal — are left to reset.
    fn discard_failed_transaction(&mut self) {
        self.set_sweep_execution_mode(crate::sweep::SweepExecutionMode::Disabled);
        self.sweep_outcome = None;
        self.cached_token_fee_info = None;
        self.cached_l1_data_fee = Default::default();
        self.pre_fee_logs.clear();
        self.post_fee_logs.clear();
        self.tx_checkpoint = None;
    }
}

impl<DB, I> ExecuteEvm for MorphEvm<DB, I>
where
    DB: Database,
{
    type Tx = MorphTxEnv;
    type Block = MorphBlockEnv;
    type State = EvmState;
    type Error = EVMError<DB::Error, MorphInvalidTransaction>;
    type ExecutionResult = ExecutionResult<MorphHaltReason>;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.ctx.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        self.sweep_outcome = None;
        // The sweep phase runs inside `MorphEvmHandler::execution_result`, before
        // `commit_tx()`, so that `SweepOutOfGas` can still reach the main call.
        let mut h = MorphEvmHandler::new();
        match h.run(self) {
            Ok(result) => Ok(result),
            Err(error) => {
                self.discard_failed_transaction();
                Err(error)
            }
        }
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx.journal_mut().finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        self.sweep_outcome = None;
        let mut h = MorphEvmHandler::new();
        let result = match h.run(self) {
            Ok(result) => result,
            Err(error) => {
                self.discard_failed_transaction();
                return Err(error);
            }
        };
        let state = self.finalize();
        Ok(ExecResultAndState::new(result, state))
    }
}

impl<DB, I> ExecuteCommitEvm for MorphEvm<DB, I>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, I> InspectEvm for MorphEvm<DB, I>
where
    DB: Database,
    I: Inspector<MorphContext<DB>>,
{
    type Inspector = I;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        self.sweep_outcome = None;
        // Sweep system calls intentionally bypass inspector frame callbacks, so
        // traces may hide them; the handler still runs them so replay state is
        // canonical.
        let mut h = MorphEvmHandler::new();
        match h.inspect_run(self) {
            Ok(result) => Ok(result),
            Err(error) => {
                self.discard_failed_transaction();
                Err(error)
            }
        }
    }
}

impl<DB, I> InspectCommitEvm for MorphEvm<DB, I>
where
    DB: Database + DatabaseCommit,
    I: Inspector<MorphContext<DB>>,
{
}

impl<DB, I> SystemCallEvm for MorphEvm<DB, I>
where
    DB: Database,
{
    fn system_call_one_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        let mut tx = TxEnv::new_system_tx_with_caller(caller, system_contract_address, data);
        tx.set_gas_limit(SYSTEM_CALL_GAS_LIMIT);
        self.inner.ctx.set_tx(tx.into());
        let mut h = MorphEvmHandler::new();
        h.run_system_call(self)
    }
}

impl<DB, I> InspectSystemCallEvm for MorphEvm<DB, I>
where
    DB: Database,
    I: Inspector<MorphContext<DB>>,
{
    fn inspect_one_system_call_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        let mut tx = TxEnv::new_system_tx_with_caller(caller, system_contract_address, data);
        tx.set_gas_limit(SYSTEM_CALL_GAS_LIMIT);
        self.inner.ctx.set_tx(tx.into());
        let mut h = MorphEvmHandler::new();
        h.inspect_run_system_call(self)
    }
}
