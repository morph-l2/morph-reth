//! Morph policy for the standard EIP-7910 `eth_config` response.

use alloy_eips::eip7910::EthConfig;
use jsonrpsee::core::RpcResult;
use reth_rpc_eth_api::helpers::config::EthConfigApiServer;

/// Removes Ethereum system contracts from an upstream EIP-7910 handler response.
///
/// Morph activates [`reth_chainspec::EthereumHardfork::Prague`] to select EIP-7702 behavior, but
/// does not deploy Prague's Ethereum L1 system contracts. The upstream handler derives those
/// contracts from Prague activation, so Morph must suppress them before returning the response.
#[derive(Debug, Clone)]
pub struct MorphEthConfigHandler<H> {
    inner: H,
}

impl<H> MorphEthConfigHandler<H> {
    /// Wraps an upstream EIP-7910 handler with Morph's system-contract policy.
    pub const fn new(inner: H) -> Self {
        Self { inner }
    }
}

impl<H> EthConfigApiServer for MorphEthConfigHandler<H>
where
    H: EthConfigApiServer,
{
    fn config(&self) -> RpcResult<EthConfig> {
        let mut config = self.inner.config()?;

        config.current.system_contracts.clear();
        if let Some(next) = &mut config.next {
            next.system_contracts.clear();
        }
        if let Some(last) = &mut config.last {
            last.system_contracts.clear();
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_eips::{
        eip7840::BlobParams,
        eip7910::{EthForkConfig, SystemContract},
    };

    use super::*;

    #[derive(Debug, Clone)]
    struct StaticEthConfigHandler(EthConfig);

    impl EthConfigApiServer for StaticEthConfigHandler {
        fn config(&self) -> RpcResult<EthConfig> {
            Ok(self.0.clone())
        }
    }

    fn fork_config() -> EthForkConfig {
        EthForkConfig {
            activation_time: 0,
            blob_schedule: BlobParams::cancun(),
            chain_id: 2818,
            fork_id: Default::default(),
            precompiles: BTreeMap::new(),
            system_contracts: SystemContract::prague(None).into_iter().collect(),
        }
    }

    #[test]
    fn removes_system_contracts_from_all_fork_configs() {
        let upstream = StaticEthConfigHandler(EthConfig {
            current: fork_config(),
            next: Some(fork_config()),
            last: Some(fork_config()),
        });

        let config = MorphEthConfigHandler::new(upstream).config().unwrap();

        assert!(config.current.system_contracts.is_empty());
        assert!(
            config
                .next
                .as_ref()
                .is_some_and(|fork| fork.system_contracts.is_empty())
        );
        assert!(
            config
                .last
                .as_ref()
                .is_some_and(|fork| fork.system_contracts.is_empty())
        );

        let json = serde_json::to_value(config).unwrap();
        assert!(json["current"].get("morph").is_none());
    }
}
