//! Morph payload builder.
//!
//! This crate provides the payload building logic for Morph L2.
//!
//! The `MorphPayloadBuilder` will implement reth's `PayloadBuilder` trait
//! to construct L2 blocks with:
//! - L1 message transactions (prioritized)
//! - Sequencer forced transactions
//! - Pool transactions

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![allow(unused)]

// TODO: Implement MorphPayloadBuilder
// See tempo-payload-builder for reference implementation
