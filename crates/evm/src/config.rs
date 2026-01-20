use crate::{
    MorphBlockAssembler, MorphBlockExecutionCtx, MorphEvmConfig, MorphEvmError,
    MorphNextBlockEnvAttributes,
};
use alloy_consensus::BlockHeader;
use morph_chainspec::hardfork::MorphHardforks;
use morph_primitives::Block;
use morph_primitives::{MorphHeader, MorphPrimitives};
use morph_revm::MorphBlockEnv;
use reth_chainspec::EthChainSpec;
use reth_evm::{
    ConfigureEvm, EvmEnv, EvmEnvFor,
    eth::{EthBlockExecutionCtx, NextEvmEnvAttributes},
};
use reth_primitives_traits::{SealedBlock, SealedHeader};
use std::borrow::Cow;

impl ConfigureEvm for MorphEvmConfig {
    type Primitives = MorphPrimitives;
    type Error = MorphEvmError;
    type NextBlockEnvCtx = MorphNextBlockEnvAttributes;
    type BlockExecutorFactory = Self;
    type BlockAssembler = MorphBlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        self
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &MorphHeader) -> Result<EvmEnvFor<Self>, Self::Error> {
        let EvmEnv { cfg_env, block_env } = EvmEnv::for_eth_block(
            header,
            self.chain_spec(),
            self.chain_spec().chain().id(),
            self.chain_spec()
                .blob_params_at_timestamp(header.timestamp()),
        );

        let spec = self
            .chain_spec()
            .morph_hardfork_at(header.number(), header.timestamp());

        Ok(EvmEnv {
            cfg_env: cfg_env.with_spec(spec),
            block_env: MorphBlockEnv { inner: block_env },
        })
    }

    fn next_evm_env(
        &self,
        parent: &MorphHeader,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        let EvmEnv { cfg_env, block_env } = EvmEnv::for_eth_next_block(
            parent,
            NextEvmEnvAttributes {
                timestamp: attributes.timestamp,
                suggested_fee_recipient: attributes.suggested_fee_recipient,
                prev_randao: attributes.prev_randao,
                gas_limit: attributes.gas_limit,
            },
            self.chain_spec()
                .next_block_base_fee(parent, attributes.timestamp)
                .unwrap_or_default(),
            self.chain_spec(),
            self.chain_spec().chain().id(),
            self.chain_spec()
                .blob_params_at_timestamp(attributes.timestamp),
        );

        // Next block number is parent + 1
        let spec = self
            .chain_spec()
            .morph_hardfork_at(parent.number() + 1, attributes.timestamp);

        Ok(EvmEnv {
            cfg_env: cfg_env.with_spec(spec),
            block_env: MorphBlockEnv { inner: block_env },
        })
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<Block>,
    ) -> Result<MorphBlockExecutionCtx<'a>, Self::Error> {
        Ok(MorphBlockExecutionCtx {
            inner: EthBlockExecutionCtx {
                parent_hash: block.header().parent_hash(),
                parent_beacon_block_root: block.header().parent_beacon_block_root(),
                ommers: &[],
                withdrawals: block.body().withdrawals.as_ref().map(Cow::Borrowed),
                extra_data: block.extra_data().clone(),
            },
        })
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<MorphHeader>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<MorphBlockExecutionCtx<'_>, Self::Error> {
        Ok(MorphBlockExecutionCtx {
            inner: EthBlockExecutionCtx {
                parent_hash: parent.hash(),
                parent_beacon_block_root: attributes.parent_beacon_block_root,
                ommers: &[],
                withdrawals: attributes.inner.withdrawals.map(Cow::Owned),
                extra_data: attributes.inner.extra_data,
            },
        })
    }
}
