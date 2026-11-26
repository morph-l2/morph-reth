//! Shared MorphTx validation logic.
//!
//! This module provides common validation logic for MorphTx (0x7F) transactions
//! that is used by both the validator (for new transactions) and the maintenance
//! task (for revalidating existing transactions).

use alloy_primitives::{Address, U256};
use morph_chainspec::hardfork::MorphHardfork;
use morph_primitives::{
    MorphTxEnvelope, TxMorph,
    transaction::morph_transaction::{MORPH_TX_VERSION_1, MORPH_TX_VERSION_2},
};
use morph_revm::{MorphEvmEnv, TokenFeeInfo};
use reth_evm::Database;

use crate::{
    MorphTxError, MorphTxValidationError,
    state_cache::{StateCache, TokenFeeValidator},
};

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
    /// Execution environment of the block whose state is read, used if the fee token
    /// resolves balances through an EVM `balanceOf` call.
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
    /// The amount that will be paid (min of fee_limit and required)
    pub amount_to_pay: U256,
}

/// Validates a MorphTx transaction's token-related fields.
///
/// This is the main entry point for MorphTx validation. It:
/// 1. Validates structural MorphTx rules (`version`, `fee_limit`, memo length, fee ordering)
/// 2. Validates ETH balance >= tx.value() (value is still paid in ETH)
/// 3. For `fee_token_id > 0`, validates token usability and balance with REVM-compatible fee_limit semantics
/// 4. For `fee_token_id == 0`, validates ETH can cover full tx cost + L1 data fee
///
pub fn validate_morph_tx<DB: Database>(
    db: &mut DB,
    input: &MorphTxValidationInput<'_>,
) -> Result<MorphTxValidationResult, MorphTxValidationError<String>> {
    let cache = StateCache::default();
    let mut validator = TokenFeeValidator::new(&cache, db, input.evm_env.clone());
    validate_morph_tx_with(input, |token_id, sender| {
        validator.resolve_token_fee_info(token_id, sender)
    })
}

/// Validates stateless MorphTx fork gates and structural invariants without any DB access.
///
/// Called early in [`MorphTransactionValidator`](crate::MorphTransactionValidator) before
/// state lookups, and again inside [`validate_morph_tx_with`] so background revalidation
/// enforces the exact same rules.
pub(crate) fn validate_morph_tx_stateless(
    morph_tx: &TxMorph,
    hardfork: MorphHardfork,
) -> Result<(), MorphTxError> {
    if !hardfork.is_jade() && morph_tx.version == MORPH_TX_VERSION_1 {
        return Err(MorphTxError::ForkNotActive {
            reason: "MorphTx version 1 is not yet active (jade fork not reached)".to_string(),
        });
    }

    // V2 (EIP-7702 authorization list) is gated on Celadon. The list itself is
    // validated by `TxMorph::validate` below (V0/V1 must not carry one, a
    // non-empty V2 list forbids CREATE; an empty V2 list is allowed); authority
    // tracking and delegated-sender limits come from the upstream validator,
    // which reads the list through `Transaction::authorization_list`.
    if !hardfork.is_celadon() && morph_tx.version == MORPH_TX_VERSION_2 {
        return Err(MorphTxError::ForkNotActive {
            reason: "MorphTx version 2 is not yet active (celadon fork not reached)".to_string(),
        });
    }

    if let Err(reason) = morph_tx.validate() {
        return Err(MorphTxError::InvalidFormat {
            reason: reason.to_string(),
        });
    }

    Ok(())
}

/// Validates a MorphTx transaction using a caller-supplied fee-token resolver.
///
/// Structural rules, ETH `value` coverage, and `fee_token_id == 0` ETH-fee checks run before
/// `resolve_token`, so invalid or ETH-paid MorphTxs never touch token registry state.
pub(crate) fn validate_morph_tx_with<E>(
    input: &MorphTxValidationInput<'_>,
    resolve_token: impl FnOnce(u16, Address) -> Result<TokenFeeInfo, MorphTxValidationError<E>>,
) -> Result<MorphTxValidationResult, MorphTxValidationError<E>> {
    // Keep MorphTx structural validation in the shared path so both initial
    // admission and background revalidation enforce the same invariants.
    let morph_tx = match input.consensus_tx {
        MorphTxEnvelope::Morph(signed) => signed.tx(),
        _ => return Err(MorphTxError::InvalidTokenId.into()),
    };

    validate_morph_tx_stateless(morph_tx, input.hardfork)?;

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
            amount_to_pay: U256::ZERO,
        });
    }

    let token_info = resolve_token(fee_token_id, input.sender)?;

    // Defensive usability guards in case a custom resolver did not check them.
    if !token_info.is_active {
        return Err(MorphTxError::TokenNotActive {
            token_id: fee_token_id,
        }
        .into());
    }
    if token_info.price_ratio.is_zero() {
        return Err(MorphTxError::InvalidPriceRatio {
            token_id: fee_token_id,
        }
        .into());
    }
    if token_info.scale.is_zero() {
        return Err(MorphTxError::InvalidTokenScale {
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
        amount_to_pay: required_token_amount,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloy_consensus::Signed;
    use alloy_primitives::{B256, Bytes, Signature, TxKind, address};
    use evm2::bytecode::Bytecode;
    use evm2::evm::{AccountInfo, CacheDB, DynDatabase, EmptyDB, InMemoryDB};
    use morph_primitives::{TxMorph, transaction::morph_transaction::MORPH_TX_VERSION_1};
    use morph_revm::{
        L2_TOKEN_REGISTRY_ADDRESS, compute_mapping_slot, compute_mapping_slot_for_address,
    };

    /// The environment the fee-token balance query is evaluated in.
    pub(crate) fn test_evm_env(hardfork: MorphHardfork) -> MorphEvmEnv {
        MorphEvmEnv::new(hardfork, morph_revm::MorphBlockEnv::default(), 2818)
    }

    /// Minimal ERC20 `balanceOf(address)` runtime bytecode:
    ///
    /// Reads `calldataload(4)` (the queried account), hashes `(account, slot 0)` with `KECCAK256`,
    /// loads that storage slot, and returns the 32-byte word.
    pub(crate) const BALANCE_OF_RUNTIME: &[u8] = &[
        0x60, 0x04, // PUSH1 0x04
        0x35, // CALLDATALOAD -> account (32 bytes, left-padded)
        0x5f, // PUSH0
        0x52, // MSTORE [0..32] = account
        0x5f, // PUSH0
        0x60, 0x20, // PUSH1 0x20
        0x52, // MSTORE [32..64] = 0 (mapping base slot 0)
        0x60, 0x40, // PUSH1 0x40
        0x5f, // PUSH0
        0x20, // KECCAK256(0, 64)
        0x54, // SLOAD
        0x5f, // PUSH0
        0x52, // MSTORE
        0x60, 0x20, // PUSH1 0x20
        0x5f, // PUSH0
        0xf3, // RETURN
    ];

    /// Bytecodes that make a call-mode `balanceOf` fail deterministically.
    pub(crate) const FAILING_BALANCE_OF: &[(&str, &[u8])] = &[
        ("revert", &[0x5f, 0x5f, 0xfd]),
        ("short return", &[0x60, 0x10, 0x5f, 0xf3]),
        ("invalid opcode", &[0xfe]),
    ];

    /// Storage slot of `account`'s balance in the [`BALANCE_OF_RUNTIME`] contract.
    pub(crate) fn token_balance_key(account: Address) -> U256 {
        compute_mapping_slot_for_address(U256::ZERO, account)
    }

    /// Registry storage entries for an active 1:1 call-mode fee token (`balance_slot == 0`).
    pub(crate) fn call_mode_registry_storage(
        token_id: u16,
        token: Address,
    ) -> [(U256, U256); 5] {
        let mut key = [0u8; 32];
        key[30..32].copy_from_slice(&token_id.to_be_bytes());
        let base = compute_mapping_slot(U256::from(151), &key);
        let mut packed = [0u8; 32];
        packed[30] = 18; // decimals
        packed[31] = 1; // is_active = true
        [
            (base, U256::from_be_bytes(token.into_word().0)),
            (base + U256::from(1), U256::ZERO), // call mode
            (base + U256::from(2), U256::from_be_bytes(packed)),
            (base + U256::from(3), U256::ONE), // scale = 1
            (compute_mapping_slot(U256::from(153), &key), U256::ONE), // price_ratio = 1
        ]
    }

    /// Builds an in-memory DB containing an active 1:1 call-mode fee token with `runtime` code
    /// and `token_balance` for `payer`.
    pub(crate) fn call_mode_token_state(
        token_id: u16,
        token: Address,
        payer: Address,
        token_balance: U256,
        runtime: &[u8],
    ) -> InMemoryDB {
        let mut db = CacheDB::new(EmptyDB::default());
        for (slot, value) in call_mode_registry_storage(token_id, token) {
            db.insert_account_storage(&L2_TOKEN_REGISTRY_ADDRESS, &slot, &value);
        }
        let code = Bytecode::new_legacy(Bytes::copy_from_slice(runtime));
        db.insert_account_info(&token, AccountInfo::default().with_code(code));
        db.insert_account_storage(&token, &token_balance_key(payer), &token_balance);
        db
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum TokenRead {
        Registry,
        TokenAccount,
        TokenStorage,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct ReadError(pub(crate) TokenRead);

    impl std::fmt::Display for ReadError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "simulated DB read failure: {:?}", self.0)
        }
    }

    impl std::error::Error for ReadError {}

    /// Database wrapper that injects a read error on the chosen target.
    pub(crate) struct UnreadableTokenDb {
        pub(crate) inner: InMemoryDB,
        pub(crate) token: Address,
        pub(crate) fail_on: TokenRead,
    }

    impl Database for UnreadableTokenDb {
        type Error = ReadError;

        fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
            if self.fail_on == TokenRead::TokenAccount && *address == self.token {
                return Err(ReadError(TokenRead::TokenAccount));
            }
            Ok(DynDatabase::get_account(&mut self.inner, address).unwrap())
        }

        fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
            Ok(DynDatabase::get_code_by_hash(&mut self.inner, code_hash).unwrap())
        }

        fn get_storage(&mut self, address: &Address, index: &U256) -> Result<U256, Self::Error> {
            if self.fail_on == TokenRead::Registry && *address == L2_TOKEN_REGISTRY_ADDRESS {
                return Err(ReadError(TokenRead::Registry));
            }
            if self.fail_on == TokenRead::TokenStorage && *address == self.token {
                return Err(ReadError(TokenRead::TokenStorage));
            }
            Ok(DynDatabase::get_storage(&mut self.inner, address, index).unwrap())
        }

        fn get_block_hash(&mut self, number: &U256) -> Result<B256, Self::Error> {
            Ok(DynDatabase::get_block_hash(&mut self.inner, number).unwrap())
        }
    }

    #[test]
    fn test_morph_tx_validation_input_construction() {
        use alloy_consensus::TxEip1559;

        let sender = address!("1000000000000000000000000000000000000001");
        let env = test_evm_env(MorphHardfork::Viridian);

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
            l1_data_fee: U256::from(100_000),
            hardfork: MorphHardfork::Viridian,
            evm_env: &env,
        };

        assert_eq!(input.sender, sender);
        assert_eq!(input.hardfork, MorphHardfork::Viridian);
        assert_eq!(input.eth_balance, U256::from(1_000_000_000_000_000_000u128));
        assert_eq!(input.l1_data_fee, U256::from(100_000));
    }

    #[test]
    fn test_validate_morph_tx_rejects_invalid_format_before_state_checks() {
        let sender = address!("1000000000000000000000000000000000000001");
        let env = test_evm_env(MorphHardfork::Jade);
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
            evm_env: &env,
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
        let env = test_evm_env(MorphHardfork::Jade);
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
            hardfork: MorphHardfork::Jade,
            evm_env: &env,
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert_eq!(err, MorphTxValidationError::Invalid(MorphTxError::InvalidTokenId));
    }

    #[test]
    fn test_validate_morph_tx_v1_rejected_before_jade_as_fork_not_active() {
        let sender = address!("1000000000000000000000000000000000000001");
        let env = test_evm_env(MorphHardfork::Emerald);
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
            fee_limit: U256::ZERO,
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
            hardfork: MorphHardfork::Emerald,
            evm_env: &env,
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert_eq!(
            err,
            MorphTxValidationError::Invalid(MorphTxError::ForkNotActive {
                reason: "MorphTx version 1 is not yet active (jade fork not reached)".to_string(),
            })
        );
    }

    #[test]
    fn test_validate_morph_tx_v2_rejected_before_celadon_as_fork_not_active() {
        let sender = address!("1000000000000000000000000000000000000001");
        let env = test_evm_env(MorphHardfork::Jade);
        let tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            version: MORPH_TX_VERSION_2,
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
        let mut input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender,
            eth_balance: U256::from(1_000_000_000_000_000_000u128),
            l1_data_fee: U256::ZERO,
            hardfork: MorphHardfork::Jade,
            evm_env: &env,
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert_eq!(
            err,
            MorphTxValidationError::Invalid(MorphTxError::ForkNotActive {
                reason: "MorphTx version 2 is not yet active (celadon fork not reached)".to_string(),
            })
        );

        let celadon_env = test_evm_env(MorphHardfork::Celadon);
        input.hardfork = MorphHardfork::Celadon;
        input.evm_env = &celadon_env;
        let result = validate_morph_tx(&mut db, &input).unwrap();
        assert!(!result.uses_token_fee);
    }

    const FEE_TOKEN: Address = address!("5300000000000000000000000000000000000042");
    const TOKEN_PAYER: Address = address!("1000000000000000000000000000000000000001");
    const TX_FEE: u64 = 21_000 * 100;

    fn token_fee_envelope() -> MorphTxEnvelope {
        let tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(address!("0000000000000000000000000000000000000002")),
            value: U256::ZERO,
            access_list: Default::default(),
            version: 0,
            fee_token_id: 1,
            fee_limit: U256::ZERO,
            reference: None,
            memo: None,
            authorization_list: Vec::new(),
            input: Default::default(),
        };
        MorphTxEnvelope::Morph(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ))
    }

    fn validate_token_fee<DB: Database>(
        db: &mut DB,
    ) -> Result<MorphTxValidationResult, MorphTxValidationError<String>> {
        let envelope = token_fee_envelope();
        let env = test_evm_env(MorphHardfork::Celadon);
        let input = MorphTxValidationInput {
            consensus_tx: &envelope,
            sender: TOKEN_PAYER,
            eth_balance: U256::ZERO,
            l1_data_fee: U256::ZERO,
            hardfork: MorphHardfork::Celadon,
            evm_env: &env,
        };
        validate_morph_tx(db, &input)
    }

    #[test]
    fn call_mode_token_fee_accepts_sufficient_and_rejects_short_balance() {
        let fee = U256::from(TX_FEE);
        let mut db = call_mode_token_state(1, FEE_TOKEN, TOKEN_PAYER, fee, BALANCE_OF_RUNTIME);
        let result = validate_token_fee(&mut db).unwrap();
        assert!(result.uses_token_fee);
        assert_eq!(result.required_token_amount, fee);

        let short = fee - U256::ONE;
        let mut db = call_mode_token_state(1, FEE_TOKEN, TOKEN_PAYER, short, BALANCE_OF_RUNTIME);
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

    #[test]
    fn a_balance_query_without_a_balance_is_an_invalid_transaction() {
        use reth_transaction_pool::error::PoolTransactionError;

        for (label, runtime) in FAILING_BALANCE_OF {
            let mut db =
                call_mode_token_state(1, FEE_TOKEN, TOKEN_PAYER, U256::from(TX_FEE), runtime);
            let err = validate_token_fee(&mut db).unwrap_err();
            assert_eq!(
                err,
                MorphTxValidationError::Invalid(MorphTxError::TokenBalanceQueryFailed {
                    token_id: 1,
                }),
                "case {label}"
            );
            assert!(
                !MorphTxError::TokenBalanceQueryFailed { token_id: 1 }.is_bad_transaction(),
                "case {label} must not penalize peers"
            );
        }
    }

    #[test]
    fn unreadable_token_state_is_a_state_error() {
        for fail_on in [
            TokenRead::Registry,
            TokenRead::TokenAccount,
            TokenRead::TokenStorage,
        ] {
            let mut db = UnreadableTokenDb {
                inner: call_mode_token_state(
                    1,
                    FEE_TOKEN,
                    TOKEN_PAYER,
                    U256::from(TX_FEE),
                    BALANCE_OF_RUNTIME,
                ),
                token: FEE_TOKEN,
                fail_on,
            };
            let err = validate_token_fee(&mut db).unwrap_err();
            assert!(
                matches!(err, MorphTxValidationError::State(_)),
                "expected State error for {fail_on:?}, got {err:?}"
            );
        }
    }
}
