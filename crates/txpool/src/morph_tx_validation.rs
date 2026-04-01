//! Shared MorphTx validation logic.
//!
//! This module provides common validation logic for MorphTx (0x7F) transactions
//! that is used by both the validator (for new transactions) and the maintenance
//! task (for revalidating existing transactions).

use alloy_evm::Database;
use alloy_primitives::{Address, U256};
use morph_chainspec::hardfork::MorphHardfork;
use morph_primitives::{MorphTxEnvelope, transaction::morph_transaction::MORPH_TX_VERSION_1};
use morph_revm::TokenFeeInfo;

use crate::MorphTxError;

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
    /// Current block base fee used to derive the effective gas price.
    pub base_fee_per_gas: Option<u64>,
    /// Current hardfork
    pub hardfork: MorphHardfork,
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
/// 3. For `fee_token_id > 0`, validates token balance with REVM-compatible fee_limit semantics
/// 4. For `fee_token_id == 0`, validates ETH can cover full tx cost + L1 data fee
///
pub fn validate_morph_tx<DB: Database>(
    db: &mut DB,
    input: &MorphTxValidationInput<'_>,
) -> Result<MorphTxValidationResult, MorphTxError> {
    // Keep MorphTx structural validation in the shared path so both initial
    // admission and background revalidation enforce the same invariants.
    let morph_tx = match input.consensus_tx {
        MorphTxEnvelope::Morph(signed) => signed.tx(),
        _ => return Err(MorphTxError::InvalidTokenId),
    };

    if !input.hardfork.is_jade() && morph_tx.version == MORPH_TX_VERSION_1 {
        return Err(MorphTxError::InvalidFormat {
            reason: "MorphTx version 1 is not yet active (jade fork not reached)".to_string(),
        });
    }

    if let Err(reason) = morph_tx.validate() {
        return Err(MorphTxError::InvalidFormat {
            reason: reason.to_string(),
        });
    }

    let tx_value = morph_tx.value;
    if tx_value > input.eth_balance {
        return Err(MorphTxError::InsufficientEthForValue {
            balance: input.eth_balance,
            value: tx_value,
        });
    }

    let fee_token_id = morph_tx.fee_token_id;
    let fee_limit = morph_tx.fee_limit;

    // Shared fee components used by both ETH-fee and token-fee branches.
    let gas_limit = U256::from(morph_tx.gas_limit);
    let max_fee_per_gas = U256::from(morph_tx.max_fee_per_gas);
    let effective_gas_price = U256::from(morph_tx.effective_gas_price(input.base_fee_per_gas));
    let gas_fee = gas_limit.saturating_mul(max_fee_per_gas);
    let total_eth_fee = gas_fee.saturating_add(input.l1_data_fee);
    let total_eth_cost = total_eth_fee.saturating_add(tx_value);

    // fee_token_id == 0 means MorphTx uses ETH-fee path (reference/memo-only MorphTx).
    if fee_token_id == 0 {
        if total_eth_cost > input.eth_balance {
            return Err(MorphTxError::InsufficientEthForValue {
                balance: input.eth_balance,
                value: total_eth_cost,
            });
        }
        return Ok(MorphTxValidationResult {
            uses_token_fee: false,
            token_info: None,
            required_token_amount: U256::ZERO,
            amount_to_pay: U256::ZERO,
        });
    }

    let token_info = TokenFeeInfo::load_for_caller(db, fee_token_id, input.sender, input.hardfork)
        .map_err(|err| MorphTxError::TokenInfoFetchFailed {
            token_id: fee_token_id,
            message: format!("{err:?}"),
        })?
        .ok_or(MorphTxError::TokenNotFound {
            token_id: fee_token_id,
        })?;

    // Check token is active
    if !token_info.is_active {
        return Err(MorphTxError::TokenNotActive {
            token_id: fee_token_id,
        });
    }

    // Check price ratio is valid
    if token_info.price_ratio.is_zero() {
        return Err(MorphTxError::InvalidPriceRatio {
            token_id: fee_token_id,
        });
    }

    let token_gas_fee = gas_limit.saturating_mul(effective_gas_price);
    let total_token_fee = token_gas_fee.saturating_add(input.l1_data_fee);
    let required_token_amount = token_info.eth_to_token_amount(total_token_fee);

    // Match REVM semantics:
    // - fee_limit == 0 => use token balance as effective limit
    // - fee_limit > balance => cap by token balance
    let effective_limit = if fee_limit.is_zero() || fee_limit > token_info.balance {
        token_info.balance
    } else {
        fee_limit
    };

    // Check token balance against effective limit.
    if effective_limit < required_token_amount {
        return Err(MorphTxError::InsufficientTokenBalance {
            token_id: fee_token_id,
            token_address: token_info.token_address,
            balance: effective_limit,
            required: required_token_amount,
        });
    }

    Ok(MorphTxValidationResult {
        uses_token_fee: true,
        token_info: Some(token_info),
        required_token_amount,
        amount_to_pay: required_token_amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Signed;
    use alloy_primitives::{B256, Signature, TxKind, address};
    use morph_primitives::{TxMorph, transaction::morph_transaction::MORPH_TX_VERSION_1};
    use reth_revm::revm::database::EmptyDB;

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
            base_fee_per_gas: Some(1_000_000_000),
            hardfork: MorphHardfork::Viridian,
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
            base_fee_per_gas: Some(1_000_000_000),
            hardfork: MorphHardfork::Jade,
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();

        assert_eq!(
            err,
            MorphTxError::InvalidFormat {
                reason: "version 1 MorphTx cannot have FeeLimit when FeeTokenID is 0".to_string(),
            }
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
            base_fee_per_gas: Some(1_000_000_000),
            hardfork: MorphHardfork::Viridian,
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert_eq!(err, MorphTxError::InvalidTokenId);
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
            base_fee_per_gas: Some(1_000_000_000),
            hardfork: MorphHardfork::Viridian,
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert!(matches!(err, MorphTxError::InsufficientEthForValue { .. }));
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
            base_fee_per_gas: Some(1_000_000_000),
            hardfork: MorphHardfork::Jade,
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
            base_fee_per_gas: Some(1_000_000_000),
            hardfork: MorphHardfork::Jade,
        };
        let mut db = EmptyDB::default();

        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert!(matches!(err, MorphTxError::InsufficientEthForValue { .. }));
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
            base_fee_per_gas: Some(1_000_000_000),
            hardfork: MorphHardfork::Viridian,
        };
        let mut db = EmptyDB::default();

        // EmptyDB has no token registry state, so token lookup will fail
        let err = validate_morph_tx(&mut db, &input).unwrap_err();
        assert!(
            matches!(err, MorphTxError::TokenNotFound { token_id: 42 }),
            "expected TokenNotFound {{ token_id: 42 }}, got {err:?}"
        );
    }
}
