//! Engine EVM configuration for Morph.
//!
//! This module provides the [`ConfigureEngineEvm`] implementation for [`MorphEvmConfig`],
//! enabling execution of engine payloads with proper transaction iteration and context setup.

use crate::MorphEvmConfig;
use alloy_consensus::crypto::RecoveryError;
use alloy_primitives::Address;
use morph_payload_types::MorphExecutionData;
use morph_primitives::{Block, MorphTxEnvelope};
use morph_revm::MorphTxEnv;
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
    RecoveredTx, ToTxEnv,
};
use reth_primitives_traits::{SealedBlock, SignedTransaction};
use std::sync::Arc;

impl ConfigureEngineEvm<MorphExecutionData> for MorphEvmConfig {
    fn evm_env_for_payload(
        &self,
        payload: &MorphExecutionData,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.evm_env(&payload.block)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a MorphExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.context_for_block(&payload.block)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &MorphExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        let block = payload.block.clone();
        // Create a collection of (block, index) pairs.
        // The engine consumes this via IntoParallelIterator.
        let transactions = (0..payload.block.body().transactions.len())
            .map(move |i| (block.clone(), i))
            .collect::<Vec<_>>();

        Ok((transactions, RecoveredInBlock::new))
    }
}

/// A [`reth_evm::execute::ExecutableTxFor`] implementation that contains a pointer to the
/// block and the transaction index, allowing to prepare a [`MorphTxEnv`] without having to
/// clone block or transaction.
#[derive(Clone)]
struct RecoveredInBlock {
    /// The sealed block containing the transaction.
    block: Arc<SealedBlock<Block>>,
    /// The index of the transaction in the block.
    index: usize,
    /// The recovered sender address.
    sender: Address,
}

impl RecoveredInBlock {
    /// Creates a new [`RecoveredInBlock`] by recovering the sender from the transaction.
    fn new((block, index): (Arc<SealedBlock<Block>>, usize)) -> Result<Self, RecoveryError> {
        let sender = block.body().transactions[index].try_recover()?;
        Ok(Self {
            block,
            index,
            sender,
        })
    }
}

impl RecoveredTx<MorphTxEnvelope> for RecoveredInBlock {
    fn tx(&self) -> &MorphTxEnvelope {
        &self.block.body().transactions[self.index]
    }

    fn signer(&self) -> &Address {
        &self.sender
    }
}

impl ToTxEnv<MorphTxEnv> for RecoveredInBlock {
    fn to_tx_env(&self) -> MorphTxEnv {
        MorphTxEnv::from_recovered_tx(self.tx(), *self.signer())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{BlockHeader, Signed, TxLegacy};
    use alloy_primitives::{B256, Bytes, Signature, TxKind, U256};
    use morph_chainspec::MorphChainSpec;
    use morph_primitives::{BlockBody, MorphHeader};
    use rayon::prelude::*;
    use reth_evm::ConfigureEngineEvm;

    fn create_legacy_tx() -> MorphTxEnvelope {
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21000,
            to: TxKind::Call(Address::repeat_byte(0x01)),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        MorphTxEnvelope::Legacy(Signed::new_unhashed(tx, Signature::test_signature()))
    }

    fn create_test_block(transactions: Vec<MorphTxEnvelope>) -> Arc<SealedBlock<Block>> {
        let header = MorphHeader {
            inner: alloy_consensus::Header {
                number: 1,
                timestamp: 1000,
                gas_limit: 30_000_000,
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            next_l1_msg_index: 0,
            batch_hash: B256::ZERO,
        };

        let body = BlockBody {
            transactions,
            ommers: vec![],
            withdrawals: None,
        };

        let block = Block { header, body };
        Arc::new(SealedBlock::seal_slow(block))
    }

    fn create_test_chainspec() -> Arc<MorphChainSpec> {
        let genesis_json = serde_json::json!({
            "config": {
                "chainId": 1337,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "mergeNetsplitBlock": 0,
                "terminalTotalDifficulty": 0,
                "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0,
                "cancunTime": 0,
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph203Time": 0,
                "viridianTime": 0,
                "morph": {}
            },
            "alloc": {}
        });
        let genesis: alloy_genesis::Genesis =
            serde_json::from_value(genesis_json).expect("genesis should be valid");
        Arc::new(MorphChainSpec::from(genesis))
    }

    #[test]
    fn test_tx_iterator_for_payload() {
        let chainspec = create_test_chainspec();
        let evm_config = MorphEvmConfig::new_with_default_factory(chainspec);

        let tx1 = create_legacy_tx();
        let tx2 = create_legacy_tx();

        let block = create_test_block(vec![tx1, tx2]);

        let payload = MorphExecutionData::new(block);

        let result = evm_config.tx_iterator_for_payload(&payload);
        assert!(result.is_ok());

        let tuple = result.unwrap();
        let (iter, recover_fn): (_, _) = tuple.into();

        // Collect items and verify we have 2 transactions
        let items: Vec<_> = iter.into_par_iter().collect();
        assert_eq!(items.len(), 2);

        // Test the recovery function works on all items
        for item in items {
            let recovered = recover_fn(item);
            assert!(recovered.is_ok());
        }
    }

    #[test]
    fn test_context_for_payload() {
        let chainspec = create_test_chainspec();
        let evm_config = MorphEvmConfig::new_with_default_factory(chainspec);

        let tx = create_legacy_tx();
        let block = create_test_block(vec![tx]);

        let payload = MorphExecutionData::new(block.clone());

        let result = evm_config.context_for_payload(&payload);
        assert!(result.is_ok());

        let context = result.unwrap();

        // Verify context fields
        assert_eq!(context.parent_hash, block.header().parent_hash());
        assert_eq!(
            context.parent_beacon_block_root,
            block.header().parent_beacon_block_root()
        );
    }

    #[test]
    fn test_evm_env_for_payload() {
        let chainspec = create_test_chainspec();
        let evm_config = MorphEvmConfig::new_with_default_factory(chainspec);

        let tx = create_legacy_tx();
        let block = create_test_block(vec![tx]);

        let payload = MorphExecutionData::new(block.clone());

        let result = evm_config.evm_env_for_payload(&payload);
        assert!(result.is_ok());

        let evm_env = result.unwrap();

        // Verify EVM environment fields
        assert_eq!(evm_env.block_env.inner.number, U256::from(block.number()));
        assert_eq!(
            evm_env.block_env.inner.timestamp,
            U256::from(block.timestamp())
        );
        assert_eq!(
            evm_env.block_env.inner.gas_limit,
            block.header().gas_limit()
        );
    }
}
