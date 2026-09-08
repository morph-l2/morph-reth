//! Morph node integration tests.
//!
//! These are real E2E tests that spin up ephemeral Morph nodes with in-memory
//! databases, produce blocks via the Engine API, and verify the chain advances
//! correctly under various conditions.

mod helpers;

mod block_building;
mod consensus;
mod dual_node_reorg;
mod engine;
mod evm;
mod hardfork;
mod invalid_payload_recovery;
mod l1_messages;
mod mixed_block_pressure;
mod morph_tx;
mod proof_history;
mod reference_index;
mod rpc;
mod txpool;
