//! Morph node integration tests.
//!
//! These are real E2E tests that spin up ephemeral Morph nodes with in-memory
//! databases, produce blocks via the Engine API, and verify the chain advances
//! correctly under various conditions.

mod helpers;

mod block_building;
mod consensus;
mod engine;
mod evm;
mod hardfork;
mod l1_messages;
mod morph_tx;
mod rpc;
mod sync;
mod txpool;
