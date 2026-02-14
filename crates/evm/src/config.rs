use crate::{MorphBlockAssembler, MorphEvmConfig, MorphEvmError, MorphNextBlockEnvAttributes};
use alloy_consensus::BlockHeader;
use morph_chainspec::hardfork::{MorphHardfork, MorphHardforks};
use morph_primitives::Block;
use morph_primitives::{MorphHeader, MorphPrimitives};
use morph_revm::MorphBlockEnv;
use reth_chainspec::EthChainSpec;
use reth_evm::{ConfigureEvm, EvmEnv, EvmEnvFor, eth::EthBlockExecutionCtx};
use reth_primitives_traits::{SealedBlock, SealedHeader};
use revm::context::{BlockEnv, CfgEnv};
use revm::context_interface::block::BlobExcessGasAndPrice;
use revm::primitives::U256;
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
        let spec = self
            .chain_spec()
            .morph_hardfork_at(header.number(), header.timestamp());

        let cfg_env = CfgEnv::<MorphHardfork>::default()
            .with_chain_id(self.chain_spec().chain().id())
            .with_spec_and_mainnet_gas_params(spec)
            .with_disable_eip7623(true);

        let fee_recipient = self
            .chain_spec()
            .fee_vault_address()
            .filter(|_| self.chain_spec().is_fee_vault_enabled())
            .unwrap_or(header.beneficiary());

        // Morph doesn't support EIP-4844 blob transactions, but when SpecId >= CANCUN,
        // revm requires `blob_excess_gas_and_price` to be set. We provide a placeholder
        // value (excess_blob_gas = 0, blob_gasprice = 1) to satisfy the validation.
        // This won't affect execution since Morph rejects blob transactions at the
        // transaction pool level.
        let block_env = BlockEnv {
            number: U256::from(header.number()),
            beneficiary: fee_recipient,
            timestamp: U256::from(header.timestamp()),
            difficulty: header.difficulty(),
            prevrandao: header.mix_hash(),
            gas_limit: header.gas_limit(),
            basefee: header.base_fee_per_gas().unwrap_or_default(),
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
                excess_blob_gas: 0,
                blob_gasprice: 1, // minimum blob gas price
            }),
        };

        Ok(EvmEnv {
            cfg_env,
            block_env: MorphBlockEnv { inner: block_env },
        })
    }

    fn next_evm_env(
        &self,
        parent: &MorphHeader,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        // Next block number is parent + 1
        let spec = self
            .chain_spec()
            .morph_hardfork_at(parent.number() + 1, attributes.timestamp);

        let cfg_env = CfgEnv::<MorphHardfork>::default()
            .with_chain_id(self.chain_spec().chain().id())
            .with_spec_and_mainnet_gas_params(spec)
            .with_disable_eip7623(true);

        let fee_recipient = self
            .chain_spec()
            .fee_vault_address()
            .filter(|_| self.chain_spec().is_fee_vault_enabled())
            .unwrap_or(attributes.suggested_fee_recipient);

        // Morph doesn't support EIP-4844 blob transactions, but when SpecId >= CANCUN,
        // revm requires `blob_excess_gas_and_price` to be set. We provide a placeholder
        // value to satisfy the validation.
        let block_env = BlockEnv {
            number: U256::from(parent.number() + 1),
            beneficiary: fee_recipient,
            timestamp: U256::from(attributes.timestamp),
            difficulty: U256::ZERO,
            prevrandao: Some(attributes.prev_randao),
            gas_limit: attributes.gas_limit,
            basefee: attributes.base_fee_per_gas.unwrap_or_else(|| {
                self.chain_spec()
                    .next_block_base_fee(parent, attributes.timestamp)
                    .unwrap_or_default()
            }),
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
                excess_blob_gas: 0,
                blob_gasprice: 1, // minimum blob gas price
            }),
        };

        Ok(EvmEnv {
            cfg_env,
            block_env: MorphBlockEnv { inner: block_env },
        })
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<Block>,
    ) -> Result<EthBlockExecutionCtx<'a>, Self::Error> {
        Ok(EthBlockExecutionCtx {
            tx_count_hint: Some(block.body().transactions.len()),
            parent_hash: block.header().parent_hash(),
            parent_beacon_block_root: block.header().parent_beacon_block_root(),
            ommers: &[],
            withdrawals: block.body().withdrawals.as_ref().map(Cow::Borrowed),
            extra_data: block.extra_data().clone(),
        })
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<MorphHeader>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<EthBlockExecutionCtx<'_>, Self::Error> {
        Ok(EthBlockExecutionCtx {
            tx_count_hint: None,
            parent_hash: parent.hash(),
            parent_beacon_block_root: attributes.parent_beacon_block_root,
            ommers: &[],
            withdrawals: attributes.inner.withdrawals.map(Cow::Owned),
            extra_data: attributes.inner.extra_data,
        })
    }
}
