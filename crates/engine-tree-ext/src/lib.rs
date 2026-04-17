//! Morph extension of reth's engine tree.
//!
//! Hosts a local copy of `reth_engine_tree::tree::payload_validator` with a
//! Jade-gated state-root skip for pre-Jade Morph blocks. See
//! `docs/superpowers/specs/2026-04-17-unfork-reth-retroactive-trust-design.md`.

#![cfg_attr(not(feature = "std"), no_std)]
