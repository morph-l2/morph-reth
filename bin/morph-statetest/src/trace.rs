//! EIP-3155 execution tracer for debugging fixtures.
//!
//! Streams one JSON struct log per executed opcode, in the line format of `geth --json`.

use evm2::{
    EvmTypesHost, Inspector,
    interpreter::{Interpreter, Message, MessageResult, opcode::OpCode},
};
use serde_json::json;
use std::io::Write;

use alloy_primitives::U256;

/// Per-opcode state captured in `step`, emitted once its gas cost is known.
#[derive(Debug)]
struct PendingStep {
    pc: usize,
    op: u8,
    gas: u64,
    stack: Vec<U256>,
    depth: u16,
    mem_size: usize,
    refund: i64,
}

/// EIP-3155 struct-log tracer writing to `out`.
#[derive(Debug)]
pub(crate) struct Eip3155Tracer<W> {
    out: W,
    pending: Option<PendingStep>,
}

impl<W: Write> Eip3155Tracer<W> {
    pub(crate) const fn new(out: W) -> Self {
        Self { out, pending: None }
    }

    /// Emits the pending step with the gas spent since it was captured.
    ///
    /// CALL- and CREATE-family opcodes never reach `step_end`: dispatch unwinds into the frame
    /// machinery instead, so their step is flushed from the `call`/`create` hooks.
    fn flush_pending<T: EvmTypesHost>(&mut self, interp: &mut Interpreter<'_, '_, T>) {
        let Some(step) = self.pending.take() else {
            return;
        };
        let gas_cost = step.gas.saturating_sub(interp.gas().remaining());
        let line = json!({
            "pc": step.pc,
            "op": step.op,
            "gas": format!("{:#x}", step.gas),
            "gasCost": format!("{gas_cost:#x}"),
            "stack": step.stack,
            "depth": u64::from(step.depth) + 1,
            "returnData": "0x",
            "refund": format!("{:#x}", step.refund.max(0)),
            "memSize": step.mem_size,
            "opName": OpCode::new_or_unknown(step.op).as_str(),
        });
        // Diagnostic output only: a broken pipe must not abort execution.
        let _ = writeln!(self.out, "{line}");
    }
}

impl<W: Write, T: EvmTypesHost> Inspector<T> for Eip3155Tracer<W> {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, T>) {
        self.pending = Some(PendingStep {
            pc: interp.pc(),
            op: interp.opcode(),
            gas: interp.gas().remaining(),
            stack: interp.stack().as_slice().to_vec(),
            depth: interp.message().depth,
            mem_size: interp.memory().len(),
            refund: interp.gas().refunded(),
        });
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, T>) {
        self.flush_pending(interp);
    }

    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, T>,
        _message: &mut Message<T>,
    ) -> Option<MessageResult<T>> {
        self.flush_pending(interp);
        None
    }

    fn create(
        &mut self,
        interp: &mut Interpreter<'_, '_, T>,
        _message: &mut Message<T>,
    ) -> Option<MessageResult<T>> {
        self.flush_pending(interp);
        None
    }
}
