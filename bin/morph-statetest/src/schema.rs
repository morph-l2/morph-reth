use alloy_eips::{eip2930::AccessList, eip7702::SignedAuthorization};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use evm2::{
    bytecode::Bytecode,
    ethereum::LazyAuthorization,
    evm::{AccountInfo, InMemoryDB},
};
use morph_chainspec::hardfork::MorphHardfork;
use morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_2;
use morph_revm::{MorphBlockEnv, MorphTxEnv, MorphTxExt};
use serde::{Deserialize, Deserializer, de};
use std::collections::BTreeMap;
use thiserror::Error;

const LEGACY_TX_TYPE: u8 = 0;
const EIP2930_TX_TYPE: u8 = 1;
const EIP1559_TX_TYPE: u8 = 2;
const EIP4844_TX_TYPE: u8 = 3;
const EIP7702_TX_TYPE: u8 = 4;

#[derive(Debug, Deserialize)]
pub struct MorphTestSuite(pub BTreeMap<String, MorphTestUnit>);

#[derive(Debug, Deserialize)]
pub struct MorphTestUnit {
    #[serde(default, rename = "_info")]
    pub info: Option<serde_json::Value>,
    pub env: Env,
    pub pre: BTreeMap<Address, TestAccount>,
    pub post: BTreeMap<String, Vec<Test>>,
    pub transaction: MorphTransactionParts,
    #[serde(default)]
    pub out: Option<Bytes>,
}

/// The block a fixture runs in.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Env {
    #[serde(rename = "currentChainID")]
    pub current_chain_id: Option<U256>,
    pub current_coinbase: Address,
    #[serde(default)]
    pub current_difficulty: U256,
    pub current_gas_limit: U256,
    pub current_number: U256,
    pub current_timestamp: U256,
    pub current_base_fee: Option<U256>,
    pub previous_hash: Option<B256>,
    pub current_random: Option<B256>,
    pub current_beacon_root: Option<B256>,
    pub current_withdrawals_root: Option<B256>,
    pub current_excess_blob_gas: Option<U256>,
    pub slot_number: Option<U256>,
}

/// A prestate account.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestAccount {
    pub balance: U256,
    pub code: Bytes,
    #[serde(deserialize_with = "deserialize_str_as_u64")]
    pub nonce: u64,
    pub storage: BTreeMap<U256, U256>,
}

/// Selects one data/gas/value combination of a fixture transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct TxPartIndices {
    pub data: usize,
    pub gas: usize,
    pub value: usize,
}

/// The expected result of one fixture case.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Test {
    pub expect_exception: Option<String>,
    pub indexes: TxPartIndices,
    pub hash: B256,
    pub logs: B256,
    pub txbytes: Option<Bytes>,
}

/// An EIP-7702 authorization as written in fixtures, which may repeat `yParity` as `v`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestAuthorization(SignedAuthorization);

impl<'de> Deserialize<'de> for TestAuthorization {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        if let Some(object) = value.as_object_mut()
            && object.contains_key("v")
            && object.contains_key("yParity")
        {
            object.remove("v");
        }
        serde_json::from_value(value)
            .map(Self)
            .map_err(de::Error::custom)
    }
}

/// A fixture transaction environment and the account sending it.
#[derive(Debug)]
pub struct FixtureTx {
    pub env: MorphTxEnv,
    pub caller: Address,
}

fn deserialize_str_as_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let string = String::deserialize(deserializer)?;
    if let Some(stripped) = string.strip_prefix("0x") {
        u64::from_str_radix(stripped, 16)
    } else {
        string.parse()
    }
    .map_err(de::Error::custom)
}

fn deserialize_maybe_empty<'de, D>(deserializer: D) -> Result<Option<Address>, D::Error>
where
    D: Deserializer<'de>,
{
    let string = String::deserialize(deserializer)?;
    if string.is_empty() {
        Ok(None)
    } else {
        string.parse().map_err(de::Error::custom).map(Some)
    }
}

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("unknown Morph hardfork: {0}")]
    UnknownFork(String),
    #[error("unknown private key: {0}")]
    UnknownPrivateKey(B256),
    #[error("transaction index {index} is out of bounds for {field}")]
    IndexOutOfBounds { field: &'static str, index: usize },
    #[error("transaction type is invalid for the selected fields")]
    InvalidTransactionType,
    #[error("quantity does not fit in {target}: {value}")]
    QuantityOverflow { target: &'static str, value: String },
    #[error(
        "blob transaction without post.txbytes is not representable: morph-statetest cannot fabricate an Eip4844 envelope for L1 fee calculation, so the L1 fee path would silently fall through to 0 and mask cross-client divergence"
    )]
    BlobTxRequiresTxBytes,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphTransactionParts {
    #[serde(
        default,
        rename = "type",
        deserialize_with = "deserialize_opt_u8_quantity"
    )]
    pub tx_type: Option<u8>,
    pub data: Vec<Bytes>,
    pub gas_limit: Vec<U256>,
    pub gas_price: Option<U256>,
    pub nonce: U256,
    pub secret_key: B256,
    #[serde(default)]
    pub sender: Option<Address>,
    #[serde(default, deserialize_with = "deserialize_maybe_empty")]
    pub to: Option<Address>,
    pub value: Vec<U256>,
    pub max_fee_per_gas: Option<U256>,
    pub max_priority_fee_per_gas: Option<U256>,
    pub initcodes: Option<Vec<Bytes>>,
    #[serde(default)]
    pub access_lists: Vec<Option<AccessList>>,
    pub authorization_list: Option<Vec<TestAuthorization>>,
    #[serde(default)]
    pub blob_versioned_hashes: Vec<B256>,
    pub max_fee_per_blob_gas: Option<U256>,
    #[serde(
        default,
        rename = "feeTokenID",
        alias = "feeTokenId",
        deserialize_with = "deserialize_opt_u16_quantity"
    )]
    pub fee_token_id: Option<u16>,
    #[serde(default)]
    pub fee_limit: Option<U256>,
    #[serde(default, deserialize_with = "deserialize_opt_u8_quantity")]
    pub version: Option<u8>,
    #[serde(default)]
    pub reference: Option<B256>,
    #[serde(default)]
    pub memo: Option<Bytes>,
}

impl MorphTestUnit {
    /// Builds the database holding the prestate.
    pub fn state(&self) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        for (address, account) in &self.pre {
            let bytecode = Bytecode::new_raw_checked(account.code.clone())
                .unwrap_or_else(|_| Bytecode::new_legacy(account.code.clone()));
            db.insert_account_info(
                address,
                AccountInfo::default()
                    .with_balance(account.balance)
                    .with_nonce(account.nonce)
                    .with_code(bytecode),
            );
            for (key, value) in &account.storage {
                db.insert_account_storage(address, key, value);
            }
        }
        db
    }

    /// Builds the block the fixture runs in.
    ///
    /// The blob base fee keeps its placeholder: BLOBBASEFEE is disabled on Morph and blob
    /// transactions do not exist there, so nothing can observe it.
    pub fn block_env(&self) -> MorphBlockEnv {
        MorphBlockEnv {
            number: self.env.current_number,
            beneficiary: self.env.current_coinbase,
            timestamp: self.env.current_timestamp,
            gas_limit: U256::from(self.env.current_gas_limit.saturating_to::<u64>()),
            basefee: U256::from(
                self.env
                    .current_base_fee
                    .unwrap_or_default()
                    .saturating_to::<u64>(),
            ),
            difficulty: self.env.current_difficulty,
            prevrandao: self
                .env
                .current_random
                .map(|random| U256::from_be_bytes(random.0))
                .unwrap_or_default(),
            slot_num: self.env.slot_number.unwrap_or_default(),
            ..Default::default()
        }
    }

    pub fn morph_tx_env(
        &self,
        test: &Test,
        _fork: MorphHardfork,
    ) -> Result<FixtureTx, SchemaError> {
        self.transaction.morph_tx_env(
            test.indexes.data,
            test.indexes.gas,
            test.indexes.value,
            self.env.current_chain_id,
            test.txbytes.clone(),
            self.env.current_base_fee.is_some(),
        )
    }
}

impl MorphTransactionParts {
    fn morph_tx_env(
        &self,
        data_index: usize,
        gas_index: usize,
        value_index: usize,
        chain_id: Option<U256>,
        txbytes: Option<Bytes>,
        has_base_fee: bool,
    ) -> Result<FixtureTx, SchemaError> {
        let caller = match self.sender {
            Some(address) => address,
            None => PrivateKeySigner::from_slice(self.secret_key.as_slice())
                .map_err(|_| SchemaError::UnknownPrivateKey(self.secret_key))?
                .address(),
        };
        let tx_type = self
            .tx_type(data_index)
            .ok_or(SchemaError::InvalidTransactionType)?;
        let gas_limit = self
            .gas_limit
            .get(gas_index)
            .ok_or(SchemaError::IndexOutOfBounds {
                field: "gasLimit",
                index: gas_index,
            })?
            .saturating_to();
        let data = self
            .data
            .get(data_index)
            .ok_or(SchemaError::IndexOutOfBounds {
                field: "data",
                index: data_index,
            })?
            .clone();
        let value = *self
            .value
            .get(value_index)
            .ok_or(SchemaError::IndexOutOfBounds {
                field: "value",
                index: value_index,
            })?;
        let access_list = self
            .access_lists
            .get(data_index)
            .cloned()
            .flatten()
            .unwrap_or_default();
        let authorization_list = self
            .authorization_list
            .clone()
            .map(|auth_list| {
                auth_list
                    .into_iter()
                    .map(|auth| LazyAuthorization::Signed(auth.0))
                    .collect()
            })
            .unwrap_or_default();

        let gas_price = self
            .gas_price
            .or(self.max_fee_per_gas)
            .unwrap_or_default()
            .try_into()
            .unwrap_or(u128::MAX);
        let gas_priority_fee = self
            .max_priority_fee_per_gas
            .map(|fee| fee.try_into().unwrap_or(u128::MAX))
            .or_else(|| has_base_fee.then_some(gas_price));

        let mut tx = MorphTxEnv {
            tx_type,
            gas_price,
            gas_priority_fee,
            gas_limit,
            data,
            nonce: u64::try_from(self.nonce).unwrap_or(u64::MAX),
            value,
            access_list,
            authorization_list,
            chain_id: chain_id.and_then(|id| id.try_into().ok()),
            kind: match self.to {
                Some(address) => TxKind::Call(address),
                None => TxKind::Create,
            },
            ..Default::default()
        };

        if let Some(version) = self.version {
            tx = tx.with_version(version);
        } else if tx.is_morph_tx() && self.authorization_list.is_some() {
            // Only V2 may carry an `authorizationList` field, so its presence
            // (even `[]`) selects V2 instead of leaving the version unset, which
            // the handler would treat as V0. Presence rather than length is the
            // same convention `tx_type` uses to select 0x04.
            tx = tx.with_version(MORPH_TX_VERSION_2);
        }
        if let Some(fee_token_id) = self.fee_token_id {
            tx = tx.with_fee_token_id(fee_token_id);
        }
        if let Some(fee_limit) = self.fee_limit {
            tx = tx.with_fee_limit(fee_limit);
        }
        if let Some(reference) = self.reference {
            tx = tx.with_reference(reference);
        }
        if let Some(memo) = &self.memo
            && !memo.is_empty()
        {
            tx = tx.with_memo(memo.clone());
        }
        if let Some(txbytes) = txbytes {
            tx = tx.with_rlp_bytes(txbytes);
        } else if !self.blob_versioned_hashes.is_empty() {
            // morph-reth has no Eip4844 envelope variant — see
            // crates/primitives/src/transaction/envelope.rs. A blob tx
            // arriving here without post.txbytes cannot be encoded for
            // L1 fee calculation, and silently falling through would
            // leave rlp_bytes empty in handler.rs (unwrap_or_default),
            // making L1 fee = 0 on reth while geth charges normally.
            // Fail loud at parse time instead of producing a phantom
            // state divergence downstream.
            return Err(SchemaError::BlobTxRequiresTxBytes);
        } else if !tx.is_l1_msg() {
            let fallback_chain_id = chain_id
                .and_then(|id| id.try_into().ok())
                .unwrap_or_default();
            // go-ethereum sizes simulation L1 fees via asUnsignedTx, which
            // post-London is always a dynamic-fee envelope (unless MorphTx /
            // EIP-7702). `encode_for_l1_fee` now does that itself.
            let rlp_bytes = tx.encode_for_l1_fee(fallback_chain_id);
            tx = tx.with_rlp_bytes(rlp_bytes);
        }

        Ok(FixtureTx { env: tx, caller })
    }

    /// Returns the raw type byte the JSON specifies, or infers it from the fields present.
    fn tx_type(&self, access_list_index: usize) -> Option<u8> {
        if let Some(tx_type) = self.tx_type {
            return Some(tx_type);
        }

        let mut tx_type = LEGACY_TX_TYPE;
        if self
            .access_lists
            .get(access_list_index)
            .is_some_and(Option::is_some)
        {
            tx_type = EIP2930_TX_TYPE;
        }
        if self.max_fee_per_gas.is_some() {
            tx_type = EIP1559_TX_TYPE;
        }
        if self.max_fee_per_blob_gas.is_some() {
            self.to?;
            return Some(EIP4844_TX_TYPE);
        }
        if self.authorization_list.is_some() {
            self.to?;
            return Some(EIP7702_TX_TYPE);
        }
        Some(tx_type)
    }
}

pub fn parse_fork(name: &str) -> Result<MorphHardfork, SchemaError> {
    match name.to_ascii_lowercase().as_str() {
        "bernoulli" => Ok(MorphHardfork::Bernoulli),
        "curie" => Ok(MorphHardfork::Curie),
        "morph203" | "morph-203" => Ok(MorphHardfork::Morph203),
        "viridian" | "prague" => Ok(MorphHardfork::Viridian),
        "emerald" => Ok(MorphHardfork::Emerald),
        "jade" => Ok(MorphHardfork::Jade),
        // OSAKA is the spec level of the latest Morph fork, so the generic
        // Ethereum name maps to it (matches `MorphHardfork::from(SpecId::OSAKA)`).
        "celadon" | "osaka" => Ok(MorphHardfork::Celadon),
        "cancun" => Ok(MorphHardfork::Morph203),
        _ => Err(SchemaError::UnknownFork(name.to_string())),
    }
}

fn deserialize_opt_u16_quantity<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_opt_quantity(deserializer, "u16", |value| {
        u16::try_from(value).map_err(|_| ())
    })
}

fn deserialize_opt_u8_quantity<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_opt_quantity(deserializer, "u8", |value| {
        u8::try_from(value).map_err(|_| ())
    })
}

fn deserialize_opt_quantity<'de, D, T, F>(
    deserializer: D,
    target: &'static str,
    convert: F,
) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    F: FnOnce(u64) -> Result<T, ()>,
{
    let Some(value) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }

    let (raw, parsed) = match value {
        serde_json::Value::Number(number) => {
            let raw = number.to_string();
            let parsed = number
                .as_u64()
                .ok_or_else(|| de::Error::custom(format!("invalid quantity: {raw}")))?;
            (raw, parsed)
        }
        serde_json::Value::String(raw) => {
            let parsed = if let Some(hex) = raw.strip_prefix("0x") {
                u64::from_str_radix(hex, 16)
            } else {
                raw.parse()
            }
            .map_err(de::Error::custom)?;
            (raw, parsed)
        }
        other => {
            return Err(de::Error::custom(format!(
                "expected quantity string or number, got {other}"
            )));
        }
    };

    convert(parsed)
        .map(Some)
        .map_err(|_| SchemaError::QuantityOverflow { target, value: raw })
        .map_err(de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statetest_without_txbytes_sets_l1_fee_encoding() {
        let suite: MorphTestSuite = serde_json::from_str(
            r#"{
              "case": {
                "env": {
                  "currentChainID": "0x1",
                  "currentCoinbase": "0x0000000000000000000000000000000000000000",
                  "currentDifficulty": "0x0",
                  "currentGasLimit": "0x989680",
                  "currentNumber": "0x1",
                  "currentTimestamp": "0x1",
                  "currentBaseFee": "0x0"
                },
                "pre": {},
                "transaction": {
                  "nonce": "0x0",
                  "gasPrice": "0x1",
                  "gasLimit": ["0x5208"],
                  "to": "0x00000000000000000000000000000000000000f1",
                  "value": ["0x0"],
                  "data": ["0x"],
                  "accessLists": [null],
                  "secretKey": "0x45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8"
                },
                "post": {
                  "Jade": [{
                    "indexes": { "data": 0, "gas": 0, "value": 0 },
                    "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "logs": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "expectException": null
                  }]
                }
              }
            }"#,
        )
        .expect("suite should parse");

        let unit = suite.0.values().next().unwrap();
        let post = &unit.post["Jade"][0];
        let tx = unit
            .morph_tx_env(post, MorphHardfork::Jade)
            .expect("tx env should build")
            .env;

        assert!(
            tx.rlp_bytes.as_ref().is_some_and(|bytes| !bytes.is_empty()),
            "statetest txs without post.txbytes still need fallback L1 fee bytes"
        );
        assert_eq!(
            tx.rlp_bytes.as_ref().unwrap().first(),
            Some(&0x02),
            "when currentBaseFee is present, fallback L1 fee encoding must match go-ethereum's dynamic-fee envelope"
        );
    }

    #[test]
    fn morph_tx_with_authorization_list_is_modelled_as_v2() {
        let suite: MorphTestSuite = serde_json::from_str(
            r#"{
              "case": {
                "env": {
                  "currentChainID": "0x1",
                  "currentCoinbase": "0x0000000000000000000000000000000000000000",
                  "currentDifficulty": "0x0",
                  "currentGasLimit": "0x989680",
                  "currentNumber": "0x1",
                  "currentTimestamp": "0x1",
                  "currentBaseFee": "0x1"
                },
                "pre": {},
                "transaction": {
                  "type": "0x7f",
                  "nonce": "0x0",
                  "gasLimit": ["0x186a0"],
                  "to": "0x00000000000000000000000000000000000000f1",
                  "value": ["0x0"],
                  "data": ["0x"],
                  "accessLists": [null],
                  "maxFeePerGas": "0x10",
                  "maxPriorityFeePerGas": "0x1",
                  "feeTokenID": "0x1",
                  "feeLimit": "0x3e8",
                  "authorizationList": [{
                    "chainId": "0x1",
                    "address": "0x4242424242424242424242424242424242424242",
                    "nonce": "0x0",
                    "yParity": "0x0",
                    "r": "0x1",
                    "s": "0x2"
                  }],
                  "secretKey": "0x45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8"
                },
                "post": {
                  "Celadon": [{
                    "indexes": { "data": 0, "gas": 0, "value": 0 },
                    "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "logs": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "expectException": null
                  }]
                }
              }
            }"#,
        )
        .expect("suite should parse");

        let unit = suite.0.values().next().unwrap();
        let post = &unit.post["Celadon"][0];
        let tx = unit
            .morph_tx_env(post, MorphHardfork::Celadon)
            .expect("tx env should build")
            .env;

        assert!(tx.is_morph_tx());
        assert_eq!(tx.version, Some(MORPH_TX_VERSION_2));
        assert_eq!(tx.fee_token_id, Some(1));
        assert_eq!(tx.authorization_list.len(), 1);

        // The fallback L1 fee bytes must be the V2 envelope (0x7f || 0x02 || rlp)
        // and carry the authorization list: the delegate address appears verbatim.
        let encoded = tx.rlp_bytes.expect("fallback L1 fee bytes");
        assert_eq!(encoded[0], 0x7f);
        assert_eq!(encoded[1], MORPH_TX_VERSION_2);
        let delegate = [0x42u8; 20];
        assert!(
            encoded.windows(20).any(|window| window == delegate),
            "L1 fee sizing bytes must include the authorization list"
        );
    }

    #[test]
    fn parse_fork_maps_celadon_and_osaka() {
        assert_eq!(parse_fork("Celadon").unwrap(), MorphHardfork::Celadon);
        assert_eq!(parse_fork("osaka").unwrap(), MorphHardfork::Celadon);
        assert_eq!(parse_fork("jade").unwrap(), MorphHardfork::Jade);
    }

    #[test]
    fn blob_tx_without_txbytes_errors_instead_of_silently_zeroing_l1_fee() {
        let suite: MorphTestSuite = serde_json::from_str(
            r#"{
              "case": {
                "env": {
                  "currentChainID": "0x1",
                  "currentCoinbase": "0x0000000000000000000000000000000000000000",
                  "currentDifficulty": "0x0",
                  "currentGasLimit": "0x989680",
                  "currentNumber": "0x1",
                  "currentTimestamp": "0x1",
                  "currentBaseFee": "0x0"
                },
                "pre": {},
                "transaction": {
                  "type": "0x3",
                  "nonce": "0x0",
                  "gasLimit": ["0x5208"],
                  "to": "0x00000000000000000000000000000000000000f1",
                  "value": ["0x0"],
                  "data": ["0x"],
                  "accessLists": [null],
                  "maxFeePerGas": "0x10",
                  "maxPriorityFeePerGas": "0x1",
                  "maxFeePerBlobGas": "0x20",
                  "blobVersionedHashes": ["0x011e0690904ead419cbeb3e3f051f17f3ec476e751b7fcb7b36f4e470616b713"],
                  "secretKey": "0x45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8"
                },
                "post": {
                  "Jade": [{
                    "indexes": { "data": 0, "gas": 0, "value": 0 },
                    "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "logs": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "expectException": null
                  }]
                }
              }
            }"#,
        )
        .expect("suite should parse");

        let unit = suite.0.values().next().unwrap();
        let post = &unit.post["Jade"][0];
        let result = unit.morph_tx_env(post, MorphHardfork::Jade);

        match result {
            Err(SchemaError::BlobTxRequiresTxBytes) => {}
            other => panic!(
                "expected blob tx without txbytes to fail loudly with BlobTxRequiresTxBytes; \
                 got {other:?}. If this assertion ever flips, double-check handler.rs is not \
                 silently computing L1 fee against empty rlp_bytes for blob txs."
            ),
        }
    }
}
