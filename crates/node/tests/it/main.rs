//! Morph node integration tests.
//!
//! These are real E2E tests that spin up an ephemeral Morph node with an
//! in-memory database, produce blocks via the Engine API, and verify the
//! chain advances correctly.

mod sync;
