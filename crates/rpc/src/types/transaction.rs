//! Morph RPC transaction type.

use alloy_rpc_types_eth::Transaction as RpcTransaction;
use morph_primitives::MorphTxEnvelope;

/// Morph RPC transaction representation.
///
/// Morph-specific fields are emitted by the `MorphTxEnvelope` variants' own
/// serde derives. Do not wrap this with extra extension fields: doing so
/// duplicates flattened keys such as `sender`, `queueIndex`, and `feeTokenID`.
pub type MorphRpcTransaction = RpcTransaction<MorphTxEnvelope>;
