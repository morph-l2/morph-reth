use crate::{MorphBlockAssembler, MorphEvmConfig, MorphEvmError, MorphNextBlockEnvAttributes};
use alloy_consensus::BlockHeader;
use alloy_primitives::Address;
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

impl MorphEvmConfig {
    /// Resolves who receives execution-layer fee rewards.
    ///
    /// When fee vault is configured, Morph routes fees to the vault regardless of
    /// the header/suggested beneficiary. This keeps execution accounting aligned
    /// with Morph's fee vault model while consensus still enforces header coinbase rules.
    fn resolve_fee_recipient(&self, fallback: Address) -> Address {
        let chain_spec = self.chain_spec();
        chain_spec
            .fee_vault_address()
            .filter(|_| chain_spec.is_fee_vault_enabled())
            .unwrap_or(fallback)
    }
}

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

        let mut cfg_env = CfgEnv::<MorphHardfork>::default()
            .with_chain_id(self.chain_spec().chain().id())
            .with_spec_and_mainnet_gas_params(spec)
            .with_disable_eip7623(true);

        // Disable EIP-7825 transaction gas limit cap
        // Morph allows transactions with gas limit > 16777216 (EIP-7825 cap)
        cfg_env.tx_gas_limit_cap = Some(header.gas_limit());

        let fee_recipient = self.resolve_fee_recipient(header.beneficiary());

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

        let mut cfg_env = CfgEnv::<MorphHardfork>::default()
            .with_chain_id(self.chain_spec().chain().id())
            .with_spec_and_mainnet_gas_params(spec)
            .with_disable_eip7623(true);

        // Disable EIP-7825 transaction gas limit cap
        // Morph allows transactions with gas limit > 16777216 (EIP-7825 cap)
        cfg_env.tx_gas_limit_cap = Some(attributes.gas_limit);

        let fee_recipient = self.resolve_fee_recipient(attributes.suggested_fee_recipient);

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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use serde_json::json;
    use std::sync::Arc;

    fn create_test_chainspec(with_fee_vault: bool) -> Arc<morph_chainspec::MorphChainSpec> {
        let morph_cfg = if with_fee_vault {
            json!({
                "feeVaultAddress": "0x530000000000000000000000000000000000000a"
            })
        } else {
            json!({})
        };

        let genesis_json = json!({
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
                "morph": morph_cfg
            },
            "alloc": {}
        });

        let genesis: alloy_genesis::Genesis =
            serde_json::from_value(genesis_json).expect("genesis should be valid");
        Arc::new(morph_chainspec::MorphChainSpec::from(genesis))
    }

    #[test]
    fn test_resolve_fee_recipient_uses_fee_vault_when_enabled() {
        let config = MorphEvmConfig::new_with_default_factory(create_test_chainspec(true));
        let fallback = address!("1111111111111111111111111111111111111111");
        let expected_vault = address!("530000000000000000000000000000000000000a");

        assert_eq!(config.resolve_fee_recipient(fallback), expected_vault);
    }

    #[test]
    fn test_resolve_fee_recipient_falls_back_when_fee_vault_disabled() {
        let config = MorphEvmConfig::new_with_default_factory(create_test_chainspec(false));
        let fallback = address!("2222222222222222222222222222222222222222");

        assert_eq!(config.resolve_fee_recipient(fallback), fallback);
    }
}
