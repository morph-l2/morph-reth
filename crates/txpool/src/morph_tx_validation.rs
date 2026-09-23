//! Shared MorphTx validation logic.
//!
//! This module provides common validation logic for MorphTx (0x7F) transactions
//! that is used by both the validator (for new transactions) and the maintenance
//! task (for revalidating existing transactions).

use alloy_evm::Database;
use alloy_primitives::{Address, U256};
use morph_chainspec::hardfork::MorphHardfork;
use morph_primitives::{
    MorphTxEnvelope,
    transaction::morph_transaction::{MORPH_TX_VERSION_1, MORPH_TX_VERSION_2},
};
use morph_revm::{MorphEvmEnv, MorphInvalidTransaction, TokenFeeInfo};
use reth_revm::revm::context::result::EVMError;

use crate::{MorphTxError, MorphTxValidationError};

/// High-level input for MorphTx validation.
///
/// This encapsulates all the context needed to validate a MorphTx transaction.
#[derive(Debug, Clone)]
pub struct MorphTxValidationInput<'a> {
    /// The consensus transaction
    pub consensus_tx: &'a MorphTxEnvelope,
    /// The sender's address
    pub sender: Address,
    /// The sender's ETH balance (for tx.value() check)
    pub eth_balance: U256,
    /// L1 data fee (pre-calculated)
    pub l1_data_fee: U256,
    /// Current hardfork
    pub hardfork: MorphHardfork,
    /// The environment a call-mode fee token's `balanceOf` is evaluated in.
    ///
    /// Must be the environment of the block whose state `db` exposes, so admission and
    /// maintenance resolve the same balance the execution layer would.
    pub evm_env: &'a MorphEvmEnv,
}

/// Result of MorphTx validation.
#[derive(Debug)]
pub struct MorphTxValidationResult {
    /// Whether this tx uses token fee payment (`fee_token_id > 0`)
    pub uses_token_fee: bool,
    /// The token info fetched during validation (token-fee tx only)
    pub token_info: Option<TokenFeeInfo>,
    /// The required token amount
    pub required_token_amount: U256,
}

/// Validates a MorphTx transaction's token-related fields.
///
/// This is the main entry point for MorphTx validation. It:
/// 1. Validates structural MorphTx rules (`version`, `fee_limit`, memo length, fee ordering)
/// 2. Validates ETH balance >= tx.value() (value is still paid in ETH)
/// 3. For `fee_token_id > 0`, validates token balance with REVM-compatible fee_limit semantics
/// 4. For `fee_token_id == 0`, validates ETH can cover full tx cost + L1 data fee
///
pub fn validate_morph_tx<DB: Database>(
    db: &mut DB,
    input: &MorphTxValidationInput<'_>,
) -> Result<MorphTxValidationResult, MorphTxValidationError<DB::Error>> {
    validate_morph_tx_with_token_info(input, |token_id| {
        TokenFeeInfo::load_for_caller(db, token_id, input.sender, input.evm_env)
    })
}

/// Shared checks with a caller-provided, fixed-state token lookup.
/// Maintenance supplies a per-round cache; admission performs a fresh lookup.
pub(crate) fn validate_morph_tx_with_token_info<E>(
    input: &MorphTxValidationInput<'_>,
    load_token: impl FnOnce(u16) -> Result<Option<TokenFeeInfo>, EVMError<E, MorphInvalidTransaction>>,
) -> Result<MorphTxValidationResult, MorphTxValidationError<E>> {
    // Keep MorphTx structural validation in the shared path so both initial
    // admission and background revalidation enforce the same invariants.
    let morph_tx = match input.consensus_tx {
        MorphTxEnvelope::Morph(signed) => signed.tx(),
        _ => return Err(MorphTxError::InvalidTokenId.into()),
    };

    if !input.hardfork.is_jade() && morph_tx.version == MORPH_TX_VERSION_1 {
        return Err(MorphTxError::InvalidFormat {
            reason: "MorphTx version 1 is not yet active (jade fork not reached)".to_string(),
        }
        .into());
    }

    // V2 (EIP-7702 authorization list) is gated on Celadon. The list itself is
    // validated by `TxMorph::validate` below (V0/V1 must not carry one, a
    // non-empty V2 list forbids CREATE; an empty V2 list is allowed); authority
    // tracking and delegated-sender limits come from the upstream validator,
    // which reads the list through `Transaction::authorization_list`.
    if !input.hardfork.is_celadon() && morph_tx.version == MORPH_TX_VERSION_2 {
        return Err(MorphTxError::InvalidFormat {
            reason: "MorphTx version 2 is not yet active (celadon fork not reached)".to_string(),
        }
        .into());
    }

    if let Err(reason) = morph_tx.validate() {
        return Err(MorphTxError::InvalidFormat {
            reason: reason.to_string(),
        }
        .into());
    }

    let tx_value = morph_tx.value;
    if tx_value > input.eth_balance {
        return Err(MorphTxError::InsufficientEthForValue {
            balance: input.eth_balance,
            value: tx_value,
        }
        .into());
    }

    let fee_token_id = morph_tx.fee_token_id;
    let fee_limit = morph_tx.fee_limit;

    // Shared fee components used by both ETH-fee and token-fee branches.
    let gas_limit = U256::from(morph_tx.gas_limit);
    let max_fee_per_gas = U256::from(morph_tx.max_fee_per_gas);
    let gas_fee = gas_limit.saturating_mul(max_fee_per_gas);
    let total_eth_fee = gas_fee.saturating_add(input.l1_data_fee);
    let total_eth_cost = total_eth_fee.saturating_add(tx_value);

    // fee_token_id == 0 means MorphTx uses ETH-fee path (reference/memo-only MorphTx).
    if fee_token_id == 0 {
        if total_eth_cost > input.eth_balance {
            return Err(MorphTxError::InsufficientEthForValue {
                balance: input.eth_balance,
                value: total_eth_cost,
            }
            .into());
        }
        return Ok(MorphTxValidationResult {
            uses_token_fee: false,
            token_info: None,
            required_token_amount: U256::ZERO,
        });
    }

    let token_info = load_token(fee_token_id)
        .map_err(|err| match err {
            EVMError::Database(err) => MorphTxValidationError::State(err),
            _ => MorphTxValidationError::Invalid(MorphTxError::TokenBalanceQueryFailed {
                token_id: fee_token_id,
            }),
        })?
        .ok_or(MorphTxError::TokenNotFound {
            token_id: fee_token_id,
        })?;

    // Check token is active
    if !token_info.is_active {
        return Err(MorphTxError::TokenNotActive {
            token_id: fee_token_id,
        }
        .into());
    }

    // Check price ratio is valid
    if token_info.price_ratio.is_zero() {
        return Err(MorphTxError::InvalidPriceRatio {
            token_id: fee_token_id,
        }
        .into());
    }

    // Txpool admission follows geth's conservative budget check and requires
    // enough tokens for the max fee cap. Execution still charges the effective price.
    let token_gas_fee = gas_fee;
    let total_token_fee = token_gas_fee.saturating_add(input.l1_data_fee);
    let required_token_amount = token_info.eth_to_token_amount(total_token_fee);

    // Share the execution layer's clamp rather than restating it: a zero `fee_limit`
    // means the whole token balance, and a larger one is capped by it.
    let effective_limit = token_info.effective_fee_limit(fee_limit);

    // Check token balance against effective limit.
    if effective_limit < required_token_amount {
        return Err(MorphTxError::InsufficientTokenBalance {
            token_id: fee_token_id,
            token_address: token_info.token_address,
            balance: effective_limit,
            required: required_token_amount,
        }
        .into());
    }

    Ok(MorphTxValidationResult {
        uses_token_fee: true,
        token_info: Some(token_info),
        required_token_amount,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The environment the fee-token balance query is evaluated in.
    fn test_evm_env(hardfork: MorphHardfork) -> MorphEvmEnv {
        MorphEvmEnv::new(
            reth_revm::revm::context::CfgEnv::new_with_spec(hardfork),
            morph_revm::MorphBlockEnv::default(),
        )
    }
    use alloy_consensus::Signed;
    use alloy_primitives::{B256, Bytes, Signature, TxKind, address};
    use morph_primitives::{TxMorph, transaction::morph_transaction::MORPH_TX_VERSION_1};
    use morph_revm::{
        L2_TOKEN_REGISTRY_ADDRESS, compute_mapping_slot, compute_mapping_slot_for_address,
    };
    use reth_revm::revm::database::{CacheDB, EmptyDB};
    use reth_revm::revm::state::{AccountInfo, Bytecode};

    // ---------------------------------------------------------------------------------
    // Fee-token fixtures, shared with the validator and maintenance tests.
    //
    // Fee tokens are paid in EVM-call mode only: the registry's `balanceSlot` word is zero,
    // so every balance read runs the token's `balanceOf` in the EVM.
    // ---------------------------------------------------------------------------------

    /// Storage slot of the `balances` mapping that [`BALANCE_OF_RUNTIME`] reads.
    pub(crate) const TOKEN_BALANCES_SLOT: u8 = 7;

    /// A fee token answering every call as ERC20 `balanceOf(address)`:
    /// `mstore(0, calldataload(4)) mstore(32, 7) mstore(0, sload(keccak256(0, 64)))
    /// return(0, 32)`, i.e. `balances[account]` from the mapping at [`TOKEN_BALANCES_SLOT`].
    pub(crate) const BALANCE_OF_RUNTIME: &[u8] = &[
        0x60,
        0x04,
        0x35,
        0x5f,
        0x52, // PUSH1 4, CALLDATALOAD, PUSH0, MSTORE
        0x60,
        TOKEN_BALANCES_SLOT,
        0x60,
        0x20,
        0x52, // PUSH1 slot, PUSH1 32, MSTORE
        0x60,
        0x40,
        0x5f,
        0x20,
        0x54, // PUSH1 64, PUSH0, KECCAK256, SLOAD
        0x5f,
        0x52,
        0x60,
        0x20,
        0x5f,
        0xf3, // PUSH0, MSTORE, PUSH1 32, PUSH0, RETURN
    ];

    /// `balanceOf` implementations that do not produce a balance: one reverts, one returns
    /// a single byte instead of a word, one halts on `INVALID`.
    pub(crate) const FAILING_BALANCE_OF: [&[u8]; 3] = [
        &[0x5f, 0x5f, 0xfd],       // PUSH0, PUSH0, REVERT
        &[0x60, 0x01, 0x5f, 0xf3], // PUSH1 1, PUSH0, RETURN
        &[0xfe],                   // INVALID
    ];

    /// Registry storage registering `token` as active fee token `token_id` in EVM-call mode,
    /// with 18 decimals, a scale of 1 and a price ratio of 1.
    pub(crate) fn call_mode_registry_storage(token_id: u16, token: Address) -> [(U256, U256); 5] {
        let mut token_key = [0u8; 32];
        token_key[30..].copy_from_slice(&token_id.to_be_bytes());
        let base = compute_mapping_slot(U256::from(151), &token_key);
        let mut active_with_decimals = [0u8; 32];
        active_with_decimals[30] = 18;
        active_with_decimals[31] = 1;
        [
            (base, U256::from_be_bytes(token.into_word().0)),
            // A zero `balanceSlot` word selects EVM-call mode.
            (base + U256::from(1), U256::ZERO),
            (
                base + U256::from(2),
                U256::from_be_bytes(active_with_decimals),
            ),
            (base + U256::from(3), U256::from(1)),
            (
                compute_mapping_slot(U256::from(153), &token_key),
                U256::from(1),
            ),
        ]
    }

    /// Key of `account` in the `balances` mapping that [`BALANCE_OF_RUNTIME`] reads.
    pub(crate) fn token_balance_key(account: Address) -> U256 {
        compute_mapping_slot_for_address(U256::from(TOKEN_BALANCES_SLOT), account)
    }

    /// State registering `token` as call-mode fee token `token_id`, whose `balanceOf` runs
    /// `code`, with `balance` recorded for `holder` in its `balances` mapping.
    pub(crate) fn call_mode_token_state(
        token_id: u16,
        token: Address,
        code: &'static [u8],
        holder: Address,
        balance: U256,
    ) -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        for (slot, value) in call_mode_registry_storage(token_id, token) {
            db.insert_account_storage(L2_TOKEN_REGISTRY_ADDRESS, slot, value)
                .unwrap();
        }
        let code = Bytecode::new_raw(Bytes::from_static(code));
        db.insert_account_info(
            token,
            AccountInfo {
                code_hash: code.hash_slow(),
                code: Some(code),
                ..Default::default()
            },
        );
        db.insert_account_storage(token, token_balance_key(holder), balance)
            .unwrap();
        db
    }

    /// Which read of the fee token [`UnreadableTokenDb`] fails.
    #[derive(Debug, Clone, Copy)]
    pub(crate) enum TokenRead {
        /// Any storage slot of the token.
        Storage,
        /// The token's code, which the EVM then has to fetch by hash.
        Code,
    }

    /// Fails one kind of read of `token` with a provider error; everything else reads through.
    #[derive(Debug)]
    pub(crate) struct UnreadableTokenDb {
        pub(crate) inner: CacheDB<EmptyDB>,
        pub(crate) token: Address,
        pub(crate) failing: TokenRead,
    }

    impl reth_revm::Database for UnreadableTokenDb {
        type Error = reth_provider::ProviderError;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            let mut info = self.inner.basic(address).unwrap();
            if address == self.token && matches!(self.failing, TokenRead::Code) {
                // A real provider returns only the code hash; the code is loaded separately.
                if let Some(info) = info.as_mut() {
                    info.code = None;
                }
            }
            Ok(info)
        }

        fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
            let token_code_hash = self.inner.basic(self.token).unwrap().map(|i| i.code_hash);
            if matches!(self.failing, TokenRead::Code) && token_code_hash == Some(code_hash) {
                return Err(reth_provider::ProviderError::BestBlockNotFound);
            }
            Ok(self.inner.code_by_hash(code_hash).unwrap())
        }

        fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
            if address == self.token && matches!(self.failing, TokenRead::Storage) {
                return Err(reth_provider::ProviderError::BestBlockNotFound);
            }
            Ok(self.inner.storage(address, index).unwrap())
        }

        fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
            Ok(self.inner.block_hash(number).unwrap())
        }
    }

    #[test]
    fn test_morph_tx_validation_input_construction() {
        use alloy_consensus::TxEip1559;

        let sender = address!("1000000000000000000000000000000000000001");

        // Create a dummy EIP-1559 transaction for testing
        let tx = TxEip1559 {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            input: Default::default(),
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            access_list: Default::default(),
        };
        let envelope = MorphTxEnvelope::Eip1559(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));

        let input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender,
            eth_balance: U256::from(1_000_000_000_000_000_000u128), // 1 ETH
            l1_data_fee: U256::from(100_000),
            hardfork: MorphHardfork::Viridian,
            evm_env: &test_evm_env(MorphHardfork::Viridian),
        };

        assert_eq!(input.sender, sender);
        assert_eq!(input.hardfork, MorphHardfork::Viridian);
        assert_eq!(input.eth_balance, U256::from(1_000_000_000_000_000_000u128));
        assert_eq!(input.l1_data_fee, U256::from(100_000));
    }

    #[test]
    fn test_validate_morph_tx_rejects_invalid_format_before_state_checks() {
        let sender = address!("1000000000000000000000000000000000000001");
        let tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            version: MORPH_TX_VERSION_1,
            fee_token_id: 0,
            fee_limit: U256::from(1u64),
            reference: Some(B256::ZERO),
            memo: None,
            authorization_list: Vec::new(),
            input: Default::default(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));
        let input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender,
            eth_balance: U256::from(1_000_000_000_000_000_000u128),
            l1_data_fee: U256::ZERO,
            hardfork: MorphHardfork::Jade,
            evm_env: &test_evm_env(MorphHardfork::Jade),
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();

        assert_eq!(
            err,
            MorphTxValidationError::Invalid(MorphTxError::InvalidFormat {
                reason: "version 1 MorphTx cannot have FeeLimit when FeeTokenID is 0".to_string(),
            })
        );
    }

    #[test]
    fn test_validate_morph_tx_rejects_non_morph_envelope() {
        use alloy_consensus::TxEip1559;

        let sender = address!("1000000000000000000000000000000000000001");
        let tx = TxEip1559 {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            input: Default::default(),
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            access_list: Default::default(),
        };
        let envelope = MorphTxEnvelope::Eip1559(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));

        let input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender,
            eth_balance: U256::from(1_000_000_000_000_000_000u128),
            l1_data_fee: U256::ZERO,
            hardfork: MorphHardfork::Viridian,
            evm_env: &test_evm_env(MorphHardfork::Viridian),
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert_eq!(
            err,
            MorphTxValidationError::Invalid(MorphTxError::InvalidTokenId)
        );
    }

    #[test]
    fn test_validate_morph_tx_insufficient_eth_for_value() {
        let sender = address!("1000000000000000000000000000000000000001");
        let tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::from(10u128.pow(18)), // 1 ETH value
            access_list: Default::default(),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            input: Default::default(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));
        let input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender,
            eth_balance: U256::from(100u64), // Insufficient ETH
            l1_data_fee: U256::ZERO,
            hardfork: MorphHardfork::Viridian,
            evm_env: &test_evm_env(MorphHardfork::Viridian),
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert!(matches!(
            err,
            MorphTxValidationError::Invalid(MorphTxError::InsufficientEthForValue { .. })
        ));
    }

    #[test]
    fn test_validate_morph_tx_eth_fee_path_sufficient_balance() {
        let sender = address!("1000000000000000000000000000000000000001");
        // fee_token_id = 0 with version 1 (Jade) means ETH-fee path
        let tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000, // 1 Gwei
            max_priority_fee_per_gas: 500_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            version: MORPH_TX_VERSION_1,
            fee_token_id: 0,
            fee_limit: U256::ZERO,
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            input: Default::default(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));

        // gas_fee = 21000 * 1_000_000_000 = 21_000_000_000_000
        // total = gas_fee + l1_data_fee + value = 21_000_000_000_000 + 1000 + 0
        let input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender,
            eth_balance: U256::from(10u128.pow(18)), // 1 ETH (sufficient)
            l1_data_fee: U256::from(1000u64),
            hardfork: MorphHardfork::Jade,
            evm_env: &test_evm_env(MorphHardfork::Jade),
        };
        let mut db = EmptyDB::default();

        let result = validate_morph_tx(&mut db, &input).unwrap();
        assert!(
            !result.uses_token_fee,
            "fee_token_id=0 should use ETH-fee path"
        );
        assert_eq!(result.required_token_amount, U256::ZERO);
    }

    #[test]
    fn test_validate_morph_tx_eth_fee_path_insufficient_balance() {
        let sender = address!("1000000000000000000000000000000000000001");
        let tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 500_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            version: MORPH_TX_VERSION_1,
            fee_token_id: 0,
            fee_limit: U256::ZERO,
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            input: Default::default(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));

        let input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender,
            eth_balance: U256::from(100u64), // Way too low
            l1_data_fee: U256::from(1000u64),
            hardfork: MorphHardfork::Jade,
            evm_env: &test_evm_env(MorphHardfork::Jade),
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert!(matches!(
            err,
            MorphTxValidationError::Invalid(MorphTxError::InsufficientEthForValue { .. })
        ));
    }

    #[test]
    fn test_validate_morph_tx_token_fee_path_token_not_found() {
        let sender = address!("1000000000000000000000000000000000000001");
        let tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 500_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            version: 0,
            fee_token_id: 42, // Non-existent token
            fee_limit: U256::from(1000u64),
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            input: Default::default(),
        };
        let envelope = MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));

        let input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender,
            eth_balance: U256::from(10u128.pow(18)),
            l1_data_fee: U256::ZERO,
            hardfork: MorphHardfork::Viridian,
            evm_env: &test_evm_env(MorphHardfork::Viridian),
        };
        let mut db = EmptyDB::default();

        // EmptyDB has no token registry state, so token lookup will fail
        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert!(
            matches!(
                err,
                MorphTxValidationError::Invalid(MorphTxError::TokenNotFound { token_id: 42 })
            ),
            "expected TokenNotFound {{ token_id: 42 }}, got {err:?}"
        );
    }

    fn v2_eth_fee_envelope(
        authorization_list: Vec<alloy_eips::eip7702::SignedAuthorization>,
    ) -> MorphTxEnvelope {
        let tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 500_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            version: MORPH_TX_VERSION_2,
            fee_token_id: 0,
            fee_limit: U256::ZERO,
            reference: None,
            memo: None,
            authorization_list,
            input: Default::default(),
        };
        MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ))
    }

    fn sample_authorization() -> alloy_eips::eip7702::SignedAuthorization {
        alloy_eips::eip7702::Authorization {
            chain_id: U256::from(2818),
            address: address!("0000000000000000000000000000000000000042"),
            nonce: 0,
        }
        .into_signed(Signature::test_signature())
    }

    #[test]
    fn test_validate_morph_tx_v2_rejected_before_celadon() {
        let envelope = v2_eth_fee_envelope(vec![sample_authorization()]);
        let input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender: address!("1000000000000000000000000000000000000001"),
            eth_balance: U256::from(10u128.pow(18)),
            l1_data_fee: U256::from(1000u64),
            hardfork: MorphHardfork::Jade,
            evm_env: &test_evm_env(MorphHardfork::Jade),
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert!(matches!(
            err,
            MorphTxValidationError::Invalid(MorphTxError::InvalidFormat { ref reason })
                if reason == "MorphTx version 2 is not yet active (celadon fork not reached)"
        ));
    }

    #[test]
    fn test_validate_morph_tx_v2_eth_fee_path_accepted_after_celadon() {
        let envelope = v2_eth_fee_envelope(vec![sample_authorization()]);
        let input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender: address!("1000000000000000000000000000000000000001"),
            eth_balance: U256::from(10u128.pow(18)),
            l1_data_fee: U256::from(1000u64),
            hardfork: MorphHardfork::Celadon,
            evm_env: &test_evm_env(MorphHardfork::Celadon),
        };
        let mut db = EmptyDB::default();

        let result = validate_morph_tx(&mut db, &input).unwrap();
        assert!(!result.uses_token_fee);
    }

    /// A V2 without authorizations is admitted like a V1 (still Celadon-gated).
    #[test]
    fn test_validate_morph_tx_v2_empty_authorization_list_accepted() {
        let envelope = v2_eth_fee_envelope(vec![]);
        let mut input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender: address!("1000000000000000000000000000000000000001"),
            eth_balance: U256::from(10u128.pow(18)),
            l1_data_fee: U256::ZERO,
            hardfork: MorphHardfork::Celadon,
            evm_env: &test_evm_env(MorphHardfork::Celadon),
        };
        let mut db = EmptyDB::default();

        let result = validate_morph_tx(&mut db, &input).unwrap();
        assert!(!result.uses_token_fee);

        input.hardfork = MorphHardfork::Jade;
        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert!(matches!(
            err,
            MorphTxValidationError::Invalid(MorphTxError::InvalidFormat { ref reason })
                if reason == "MorphTx version 2 is not yet active (celadon fork not reached)"
        ));
    }

    const FEE_TOKEN: Address = address!("5300000000000000000000000000000000000042");
    const TOKEN_PAYER: Address = address!("1000000000000000000000000000000000000001");
    /// `gas_limit * max_fee_per_gas` of [`token_fee_envelope`]; with the fixtures' 1:1 price
    /// ratio and no L1 fee this is also its token requirement.
    const TOKEN_FEE: u64 = 21_000 * 100;

    fn token_fee_envelope() -> MorphTxEnvelope {
        let tx = TxMorph {
            chain_id: 2818,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            fee_token_id: 1,
            ..Default::default()
        };
        MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ))
    }

    /// Runs the MorphTx checks for [`token_fee_envelope`] from a sender without ETH.
    fn validate_token_fee<DB: Database>(
        db: &mut DB,
    ) -> Result<MorphTxValidationResult, MorphTxValidationError<DB::Error>> {
        let envelope = token_fee_envelope();
        let evm_env = test_evm_env(MorphHardfork::Jade);
        let input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender: TOKEN_PAYER,
            eth_balance: U256::ZERO,
            l1_data_fee: U256::ZERO,
            hardfork: MorphHardfork::Jade,
            evm_env: &evm_env,
        };
        validate_morph_tx(db, &input)
    }

    #[test]
    fn a_call_mode_token_balance_is_read_through_balance_of() {
        let fee = U256::from(TOKEN_FEE);
        let mut db = call_mode_token_state(1, FEE_TOKEN, BALANCE_OF_RUNTIME, TOKEN_PAYER, fee);
        let result = validate_token_fee(&mut db).unwrap();
        assert!(result.uses_token_fee);
        assert_eq!(result.required_token_amount, fee);
        assert_eq!(result.token_info.map(|info| info.balance), Some(fee));

        let short = fee - U256::from(1);
        let mut db = call_mode_token_state(1, FEE_TOKEN, BALANCE_OF_RUNTIME, TOKEN_PAYER, short);
        assert_eq!(
            validate_token_fee(&mut db).unwrap_err(),
            MorphTxValidationError::Invalid(MorphTxError::InsufficientTokenBalance {
                token_id: 1,
                token_address: FEE_TOKEN,
                balance: short,
                required: fee,
            })
        );
    }

    /// A `balanceOf` that reverts, answers with less than a word or halts yields no balance.
    /// That is a verdict on the transaction, but not one that blames the relaying peer.
    #[test]
    fn a_balance_query_without_a_balance_is_an_invalid_transaction() {
        use reth_transaction_pool::error::PoolTransactionError;

        for code in FAILING_BALANCE_OF {
            let mut db =
                call_mode_token_state(1, FEE_TOKEN, code, TOKEN_PAYER, U256::from(TOKEN_FEE));
            assert_eq!(
                validate_token_fee(&mut db).unwrap_err(),
                MorphTxValidationError::Invalid(MorphTxError::TokenBalanceQueryFailed {
                    token_id: 1
                }),
                "balanceOf code {code:02x?}"
            );
        }
        assert!(!MorphTxError::TokenBalanceQueryFailed { token_id: 1 }.is_bad_transaction());
    }

    /// A read failure inside the `balanceOf` call says nothing about the transaction.
    #[test]
    fn an_unreadable_fee_token_is_a_state_error() {
        for failing in [TokenRead::Storage, TokenRead::Code] {
            let mut db = UnreadableTokenDb {
                inner: call_mode_token_state(
                    1,
                    FEE_TOKEN,
                    BALANCE_OF_RUNTIME,
                    TOKEN_PAYER,
                    U256::from(TOKEN_FEE),
                ),
                token: FEE_TOKEN,
                failing,
            };
            assert!(
                matches!(
                    validate_token_fee(&mut db),
                    Err(MorphTxValidationError::State(_))
                ),
                "{failing:?}"
            );
        }
    }
}
