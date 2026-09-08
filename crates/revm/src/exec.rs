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
        let mut h = MorphEvmHandler::new();
        h.run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx.journal_mut().finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let mut h = MorphEvmHandler::new();
        h.run(self)
            // An execution error can leave loaded accounts and warm-state data in
            // the journal. Clear it before this EVM is reused, matching revm's
            // mainnet replay implementation.
            .inspect_err(|_| {
                let _ = self.finalize();
            })
            .map(|result| {
                let state = self.finalize();
                ExecResultAndState::new(result, state)
            })
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
        let mut h = MorphEvmHandler::new();
        h.inspect_run(self)
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::TxKind;
    use morph_chainspec::MorphHardfork;
    use revm::{
        context::{BlockEnv, TxEnv},
        context_interface::result::InvalidTransaction,
        database::{CacheDB, EmptyDB},
        handler::EvmTr,
        inspector::NoOpInspector,
    };

    #[test]
    fn replay_clears_journal_after_execution_error() {
        let caller = Address::repeat_byte(0x11);
        let mut evm = MorphEvm::new(
            MorphContext::new(CacheDB::new(EmptyDB::default()), MorphHardfork::default()),
            NoOpInspector,
        );
        evm.block = MorphBlockEnv {
            inner: BlockEnv {
                gas_limit: 30_000_000,
                ..Default::default()
            },
        };
        evm.tx = MorphTxEnv {
            inner: TxEnv {
                caller,
                gas_limit: 21_000,
                gas_price: 1,
                kind: TxKind::Call(Address::ZERO),
                ..Default::default()
            },
            ..Default::default()
        };

        let result = evm.replay();

        assert!(matches!(
            result,
            Err(EVMError::Transaction(
                MorphInvalidTransaction::EthInvalidTransaction(
                    InvalidTransaction::LackOfFundForMaxFee { .. }
                )
            ))
        ));
        assert!(
            evm.ctx_ref().journal().state.is_empty(),
            "failed replay must not leak loaded account state"
        );
        assert!(
            evm.ctx_ref().journal().journal.is_empty(),
            "failed replay must not leak journal entries"
        );
    }
}
