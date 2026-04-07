use crate::{MorphBlockAssembler, MorphEvmConfig, MorphEvmError, MorphNextBlockEnvAttributes};
use alloy_consensus::BlockHeader;
use alloy_primitives::B256;
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

        let mut cfg_env = CfgEnv::<MorphHardfork>::default()
            .with_chain_id(self.chain_spec().chain().id())
            .with_spec(spec);
        cfg_env.disable_eip7623 = true;

        let fee_recipient = self
            .chain_spec()
            .fee_vault_address()
            .unwrap_or_else(|| header.beneficiary());

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
            .with_spec(spec);
        cfg_env.disable_eip7623 = true;

        let fee_recipient = self
            .chain_spec()
            .fee_vault_address()
            .unwrap_or(attributes.suggested_fee_recipient);

        // Morph doesn't support EIP-4844 blob transactions, but when SpecId >= CANCUN,
        // revm requires `blob_excess_gas_and_price` to be set. We provide a placeholder
        // value to satisfy the validation.
        let block_env = BlockEnv {
            number: U256::from(parent.number() + 1),
            beneficiary: fee_recipient,
            timestamp: U256::from(attributes.timestamp),
            difficulty: U256::ZERO,
            // Morph L2 follows geth's L2 path here: PREVRANDAO/mixHash is fixed to zero.
            prevrandao: Some(B256::ZERO),
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
    use alloy_consensus::Header;
    use alloy_primitives::{B256, Bytes, U256};
    use morph_chainspec::MorphChainSpec;
    use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
    use std::sync::Arc;

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
        let genesis: alloy_genesis::Genesis = serde_json::from_value(genesis_json).unwrap();
        Arc::new(MorphChainSpec::from(genesis))
    }

    fn create_morph_header(number: u64, timestamp: u64) -> MorphHeader {
        MorphHeader {
            inner: Header {
                number,
                timestamp,
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(1_000_000),
                ..Default::default()
            },
            next_l1_msg_index: 0,
        }
    }

    #[test]
    fn test_evm_env_sets_chain_id() {
        let chain_spec = create_test_chainspec();
        let config = MorphEvmConfig::new_with_default_factory(chain_spec);

        let header = create_morph_header(100, 1000);
        let env = config.evm_env(&header).unwrap();

        assert_eq!(env.cfg_env.chain_id, 1337);
    }

    #[test]
    fn test_evm_env_sets_block_env_fields() {
        let chain_spec = create_test_chainspec();
        let config = MorphEvmConfig::new_with_default_factory(chain_spec);

        let header = create_morph_header(100, 1000);
        let env = config.evm_env(&header).unwrap();

        assert_eq!(env.block_env.inner.number, U256::from(100u64));
        assert_eq!(env.block_env.inner.timestamp, U256::from(1000u64));
        assert_eq!(env.block_env.inner.gas_limit, 30_000_000);
        assert_eq!(env.block_env.inner.basefee, 1_000_000);
    }

    #[test]
    fn test_evm_env_blob_gas_placeholder() {
        let chain_spec = create_test_chainspec();
        let config = MorphEvmConfig::new_with_default_factory(chain_spec);

        let header = create_morph_header(100, 1000);
        let env = config.evm_env(&header).unwrap();

        // Morph uses placeholder blob gas values
        let blob_info = env.block_env.inner.blob_excess_gas_and_price.unwrap();
        assert_eq!(blob_info.excess_blob_gas, 0);
        assert_eq!(blob_info.blob_gasprice, 1);
    }

    #[test]
    fn test_evm_env_eip7623_disabled() {
        let chain_spec = create_test_chainspec();
        let config = MorphEvmConfig::new_with_default_factory(chain_spec);

        let header = create_morph_header(100, 1000);
        let env = config.evm_env(&header).unwrap();

        assert!(env.cfg_env.disable_eip7623);
    }

    #[test]
    fn test_next_evm_env_increments_block_number() {
        let chain_spec = create_test_chainspec();
        let config = MorphEvmConfig::new_with_default_factory(chain_spec);

        let parent = create_morph_header(99, 1000);
        let attrs = MorphNextBlockEnvAttributes {
            inner: NextBlockEnvAttributes {
                timestamp: 1001,
                suggested_fee_recipient: alloy_primitives::Address::ZERO,
                prev_randao: B256::repeat_byte(0xcc),
                gas_limit: 30_000_000,
                parent_beacon_block_root: None,
                withdrawals: None,
                extra_data: Bytes::new(),
            },
            base_fee_per_gas: Some(500_000),
        };

        let env = config.next_evm_env(&parent, &attrs).unwrap();

        assert_eq!(env.block_env.inner.number, U256::from(100u64));
        assert_eq!(env.block_env.inner.timestamp, U256::from(1001u64));
        assert_eq!(env.block_env.inner.basefee, 500_000);
    }

    #[test]
    fn test_context_for_block_populates_fields() {
        let chain_spec = create_test_chainspec();
        let config = MorphEvmConfig::new_with_default_factory(chain_spec);

        let header = create_morph_header(100, 1000);
        let block = morph_primitives::Block {
            header,
            body: morph_primitives::BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: None,
            },
        };
        let sealed = SealedBlock::seal_slow(block);

        let ctx = config.context_for_block(&sealed).unwrap();
        assert_eq!(ctx.parent_hash, sealed.header().parent_hash());
        assert!(ctx.ommers.is_empty());
    }

    #[test]
    fn test_context_for_next_block_uses_parent_hash() {
        let chain_spec = create_test_chainspec();
        let config = MorphEvmConfig::new_with_default_factory(chain_spec);

        let parent = create_morph_header(99, 1000);
        let parent_sealed = SealedHeader::seal_slow(parent);

        let attrs = MorphNextBlockEnvAttributes {
            inner: NextBlockEnvAttributes {
                timestamp: 1001,
                suggested_fee_recipient: alloy_primitives::Address::ZERO,
                prev_randao: B256::ZERO,
                gas_limit: 30_000_000,
                parent_beacon_block_root: None,
                withdrawals: None,
                extra_data: Bytes::new(),
            },
            base_fee_per_gas: None,
        };

        let ctx = config
            .context_for_next_block(&parent_sealed, attrs)
            .unwrap();
        assert_eq!(ctx.parent_hash, parent_sealed.hash());
    }
}
