//! Morph types for genesis data.

use alloy_primitives::Address;
use alloy_serde::OtherFields;
use serde::{Deserialize, Serialize, de::Error as _};

/// Container type for all Morph-specific fields in a genesis file.
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphGenesisInfo {
    /// Information about hard forks specific to the Morph chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hard_fork_info: Option<MorphHardforkInfo>,
    /// Morph chain-specific configuration details.
    pub morph_chain_info: MorphChainConfig,
}

impl MorphGenesisInfo {
    /// Extracts the Morph specific fields from a genesis file.
    pub fn extract_from(others: &OtherFields) -> Option<Self> {
        Self::try_from(others).ok()
    }
}

impl TryFrom<&OtherFields> for MorphGenesisInfo {
    type Error = serde_json::Error;

    fn try_from(others: &OtherFields) -> Result<Self, Self::Error> {
        let hard_fork_info = MorphHardforkInfo::try_from(others).ok();
        let morph_chain_info = MorphChainConfig::try_from(others)?;

        Ok(Self {
            hard_fork_info,
            morph_chain_info,
        })
    }
}

/// Morph hardfork info specifies the block numbers and timestamps at which
/// the Morph hardforks were activated.
///
/// Note: Bernoulli and Curie use block-based activation, while Morph203, Viridian,
/// and Emerald use timestamp-based activation (matching go-ethereum behavior).
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphHardforkInfo {
    /// Bernoulli hardfork block number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bernoulli_block: Option<u64>,
    /// Curie hardfork block number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curie_block: Option<u64>,
    /// Morph203 hardfork timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub morph203_time: Option<u64>,
    /// Viridian hardfork timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viridian_time: Option<u64>,
    /// Emerald hardfork timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emerald_time: Option<u64>,
}

impl MorphHardforkInfo {
    /// Extract the Morph-specific genesis info from a genesis file.
    pub fn extract_from(others: &OtherFields) -> Option<Self> {
        Self::try_from(others).ok()
    }
}

impl TryFrom<&OtherFields> for MorphHardforkInfo {
    type Error = serde_json::Error;

    fn try_from(others: &OtherFields) -> Result<Self, Self::Error> {
        others.deserialize_as()
    }
}

/// The configuration for the Morph chain.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphChainConfig {
    /// The address of the L2 transaction fee vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_vault_address: Option<Address>,
    /// The maximum tx payload size per block in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tx_payload_bytes_per_block: Option<usize>,
}

impl MorphChainConfig {
    /// Extracts the morph config by looking for the `morph` key in genesis.
    pub fn extract_from(others: &OtherFields) -> Option<Self> {
        Self::try_from(others).ok()
    }

    /// Returns whether the fee vault is enabled.
    pub const fn is_fee_vault_enabled(&self) -> bool {
        self.fee_vault_address.is_some()
    }

    /// Checks if the given block size (in bytes) is valid for this chain.
    pub fn is_valid_block_size(&self, size: usize) -> bool {
        self.max_tx_payload_bytes_per_block
            .map(|max| size <= max)
            .unwrap_or(true)
    }
}

impl TryFrom<&OtherFields> for MorphChainConfig {
    type Error = serde_json::Error;

    fn try_from(others: &OtherFields) -> Result<Self, Self::Error> {
        if let Some(Ok(morph_config)) = others.get_deserialized::<Self>("morph") {
            Ok(morph_config)
        } else {
            Err(serde_json::Error::missing_field("morph"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_extract_morph_hardfork_info() {
        let genesis_info = r#"
        {
          "bernoulliBlock": 0,
          "curieBlock": 100,
          "morph203Time": 3000,
          "viridianTime": 4000,
          "emeraldTime": 5000
        }
        "#;

        let others: OtherFields = serde_json::from_str(genesis_info).unwrap();
        let hardfork_info = MorphHardforkInfo::extract_from(&others).unwrap();

        assert_eq!(
            hardfork_info,
            MorphHardforkInfo {
                bernoulli_block: Some(0),
                curie_block: Some(100),
                morph203_time: Some(3000),
                viridian_time: Some(4000),
                emerald_time: Some(5000),
            }
        );
    }

    #[test]
    fn test_extract_morph_chain_config() {
        let config_str = r#"
        {
          "morph": {
            "feeVaultAddress": "0x530000000000000000000000000000000000000a",
            "maxTxPayloadBytesPerBlock": 122880
          }
        }
        "#;

        let others: OtherFields = serde_json::from_str(config_str).unwrap();
        let config = MorphChainConfig::extract_from(&others).unwrap();

        assert_eq!(
            config.fee_vault_address,
            Some(address!("530000000000000000000000000000000000000a"))
        );
        assert_eq!(config.max_tx_payload_bytes_per_block, Some(122880));
        assert!(config.is_fee_vault_enabled());
        assert!(config.is_valid_block_size(100000));
        assert!(!config.is_valid_block_size(200000));
    }

    #[test]
    fn test_default_config() {
        let config = MorphChainConfig::default();
        assert!(!config.is_fee_vault_enabled());
        assert_eq!(config.fee_vault_address, None);
        assert_eq!(config.max_tx_payload_bytes_per_block, None);
        // Without max size limit, any size is valid
        assert!(config.is_valid_block_size(usize::MAX));
    }
}
