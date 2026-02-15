//! Shared MorphTx validation logic.
//!
//! This module provides common validation logic for MorphTx (0x7F) transactions
//! that is used by both the validator (for new transactions) and the maintenance
//! task (for revalidating existing transactions).

use alloy_consensus::Transaction;
use alloy_evm::Database;
use alloy_primitives::{Address, U256};
use morph_chainspec::hardfork::MorphHardfork;
use morph_primitives::MorphTxEnvelope;
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
/// 1. Validates ETH balance >= tx.value() (value is still paid in ETH)
/// 2. For `fee_token_id > 0`, validates token balance with REVM-compatible fee_limit semantics
/// 3. For `fee_token_id == 0`, validates ETH can cover full tx cost + L1 data fee
///
pub fn validate_morph_tx<DB: Database>(
    db: &mut DB,
    input: &MorphTxValidationInput<'_>,
) -> Result<MorphTxValidationResult, MorphTxError> {
    let tx_value = input.consensus_tx.value();
    if tx_value > input.eth_balance {
        return Err(MorphTxError::InsufficientEthForValue {
            balance: input.eth_balance,
            value: tx_value,
        });
    }

    let fields = input
        .consensus_tx
        .morph_fields()
        .ok_or(MorphTxError::InvalidTokenId)?;
    let fee_token_id = fields.fee_token_id;
    let fee_limit = fields.fee_limit;

    // Shared fee components used by both ETH-fee and token-fee branches.
    let gas_limit = U256::from(input.consensus_tx.gas_limit());
    let max_fee_per_gas = U256::from(input.consensus_tx.max_fee_per_gas());
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

    let required_token_amount = token_info.eth_to_token_amount(total_eth_fee);

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
    use alloy_primitives::address;

    #[test]
    fn test_morph_tx_validation_input_construction() {
        use alloy_consensus::{Signed, TxEip1559};
        use alloy_primitives::{B256, Signature, TxKind};

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
        };

        assert_eq!(input.sender, sender);
        assert_eq!(input.hardfork, MorphHardfork::Viridian);
        assert_eq!(input.eth_balance, U256::from(1_000_000_000_000_000_000u128));
        assert_eq!(input.l1_data_fee, U256::from(100_000));
    }
}
