use morph_chainspec::hardfork::MorphHardfork;
use morph_revm::MorphTxEnv;
use revm::{
    context::{BlockEnv, CfgEnv, TransactionType, TxEnv},
    context_interface::{
        block::BlobExcessGasAndPrice,
        transaction::{AccessList, SignedAuthorization},
    },
    database::CacheState,
    primitives::{Address, AddressMap, B256, Bytes, TxKind, U256, keccak256},
    state::Bytecode,
};
use revm_statetest_types::{
    AccountInfo, Env, Test, TestAuthorization, deserialize_maybe_empty, recover_address,
};
use serde::{Deserialize, Deserializer, de};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Deserialize)]
pub struct MorphTestSuite(pub BTreeMap<String, MorphTestUnit>);

#[derive(Debug, Deserialize)]
pub struct MorphTestUnit {
    #[serde(default, rename = "_info")]
    pub info: Option<serde_json::Value>,
    pub env: Env,
    pub pre: AddressMap<AccountInfo>,
    pub post: BTreeMap<String, Vec<Test>>,
    pub transaction: MorphTransactionParts,
    #[serde(default)]
    pub out: Option<Bytes>,
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
    pub fn state(&self) -> CacheState {
        let mut cache_state = CacheState::new();
        for (address, info) in &self.pre {
            let code_hash = keccak256(&info.code);
            let bytecode = Bytecode::new_raw_checked(info.code.clone())
                .unwrap_or_else(|_| Bytecode::new_legacy(info.code.clone()));
            let account_info = revm::state::AccountInfo {
                balance: info.balance,
                code_hash,
                code: Some(bytecode),
                nonce: info.nonce,
                account_id: None,
            };
            cache_state.insert_account_with_storage(*address, account_info, info.storage.clone());
        }
        cache_state
    }

    pub fn block_env(&self, cfg: &mut CfgEnv<MorphHardfork>) -> BlockEnv {
        let mut block = BlockEnv {
            number: self.env.current_number,
            beneficiary: self.env.current_coinbase,
            timestamp: self.env.current_timestamp,
            gas_limit: self.env.current_gas_limit.try_into().unwrap_or(u64::MAX),
            basefee: self
                .env
                .current_base_fee
                .unwrap_or_default()
                .try_into()
                .unwrap_or(u64::MAX),
            difficulty: self.env.current_difficulty,
            prevrandao: self.env.current_random,
            slot_num: self
                .env
                .slot_number
                .unwrap_or_default()
                .try_into()
                .unwrap_or(u64::MAX),
            ..BlockEnv::default()
        };

        if let Some(excess_blob_gas) = self.env.current_excess_blob_gas {
            block.set_blob_excess_gas_and_price(
                excess_blob_gas.to(),
                cfg.blob_base_fee_update_fraction(),
            );
        } else {
            block.blob_excess_gas_and_price = Some(BlobExcessGasAndPrice {
                excess_blob_gas: 0,
                blob_gasprice: 1,
            });
        }

        if block.prevrandao.is_none() {
            block.prevrandao = Some(B256::ZERO);
        }

        block
    }

    pub fn morph_tx_env(&self, test: &Test) -> Result<MorphTxEnv, SchemaError> {
        self.transaction.morph_tx_env(
            test.indexes.data,
            test.indexes.gas,
            test.indexes.value,
            self.env.current_chain_id,
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
    ) -> Result<MorphTxEnv, SchemaError> {
        let caller = match self.sender {
            Some(address) => address,
            None => recover_address(self.secret_key.as_slice())
                .ok_or(SchemaError::UnknownPrivateKey(self.secret_key))?,
        };
        // Preserve the raw tx-type byte when the JSON explicitly specifies
        // it. Routing it through `revm::context::TransactionType` first
        // would lossily map any non-standard byte (Morph's 0x7f, L1Msg's
        // 0x7e, future fork types) to `Custom = 0xFF`, since revm's enum
        // only knows the 5 vanilla Ethereum tx types. The cast-to-u8 of
        // `Custom` then yields 0xFF, so `MorphTxExt::is_morph_tx` and
        // `is_l1_msg` both return false and the morph fee handler /
        // L1Message short-circuit never activate. Only fall through to
        // the enum-based inference for txs that didn't carry a `type`
        // field at all.
        let tx_type = match self.tx_type {
            Some(t) => t,
            None => {
                self.tx_type(data_index)
                    .ok_or(SchemaError::InvalidTransactionType)? as u8
            }
        };
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
                    .map(|auth| {
                        revm::context::either::Either::<SignedAuthorization, _>::Left(auth.into())
                    })
                    .collect()
            })
            .unwrap_or_default();

        let inner = TxEnv {
            caller,
            gas_price: self
                .gas_price
                .or(self.max_fee_per_gas)
                .unwrap_or_default()
                .try_into()
                .unwrap_or(u128::MAX),
            gas_priority_fee: self
                .max_priority_fee_per_gas
                .map(|fee| fee.try_into().unwrap_or(u128::MAX)),
            blob_hashes: self.blob_versioned_hashes.clone(),
            max_fee_per_blob_gas: self
                .max_fee_per_blob_gas
                .map(|fee| fee.try_into().unwrap_or(u128::MAX))
                .unwrap_or(u128::MAX),
            tx_type,
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
        };

        let mut tx = MorphTxEnv::new(inner);
        if let Some(version) = self.version {
            tx = tx.with_version(version);
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

        Ok(tx)
    }

    fn tx_type(&self, access_list_index: usize) -> Option<TransactionType> {
        if let Some(tx_type) = self.tx_type {
            return Some(TransactionType::from(tx_type));
        }

        let mut tx_type = TransactionType::Legacy;
        if self
            .access_lists
            .get(access_list_index)
            .is_some_and(Option::is_some)
        {
            tx_type = TransactionType::Eip2930;
        }
        if self.max_fee_per_gas.is_some() {
            tx_type = TransactionType::Eip1559;
        }
        if self.max_fee_per_blob_gas.is_some() {
            self.to?;
            return Some(TransactionType::Eip4844);
        }
        if self.authorization_list.is_some() {
            self.to?;
            return Some(TransactionType::Eip7702);
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
        "jade" | "osaka" => Ok(MorphHardfork::Jade),
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
