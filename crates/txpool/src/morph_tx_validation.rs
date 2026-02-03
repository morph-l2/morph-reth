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
    /// The token info fetched during validation
    pub token_info: TokenFeeInfo,
    /// The required token amount
    pub required_token_amount: U256,
    /// The amount that will be paid (min of fee_limit and required)
    pub amount_to_pay: U256,
}

/// Validates a MorphTx transaction's token-related fields.
///
/// This is the main entry point for MorphTx validation. It:
/// 1. Validates ETH balance >= tx.value() (value is still paid in ETH)
/// 2. Validates token balance and fee_limit
///
pub fn validate_morph_tx<DB: Database>(
    db: &mut DB,
    input: &MorphTxValidationInput<'_>,
) -> Result<MorphTxValidationResult, MorphTxError> {
    // Check ETH balance >= tx.value() (value is still paid in ETH)
    // Reference: geth tx_pool.go:1649 - costLimit.Cmp(tx.Value()) < 0
    let tx_value = input.consensus_tx.value();
    if tx_value > input.eth_balance {
        return Err(MorphTxError::InsufficientEthForValue {
            balance: input.eth_balance,
            value: tx_value,
        });
    }

    // Extract MorphTx fields
    let (fee_token_id, fee_limit) =
        extract_morph_tx_fields(input.consensus_tx).ok_or(MorphTxError::InvalidTokenId)?;

    // Token ID 0 is reserved for ETH
    if fee_token_id == 0 {
        return Err(MorphTxError::InvalidTokenId);
    }

    // Fetch token info from L2TokenRegistry
    let token_info = TokenFeeInfo::fetch(db, fee_token_id, input.sender, input.hardfork)
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

    // Calculate gas fee in ETH
    let gas_limit = U256::from(input.consensus_tx.gas_limit());
    let max_fee_per_gas = U256::from(input.consensus_tx.max_fee_per_gas());
    let gas_fee = gas_limit.saturating_mul(max_fee_per_gas);

    // Total ETH fee = gas_fee + l1_data_fee
    let total_eth_fee = gas_fee.saturating_add(input.l1_data_fee);

    // Convert ETH fee to token amount
    let required_token_amount = token_info.calculate_token_amount(total_eth_fee);

    // Check fee_limit >= required_token_amount
    if fee_limit < required_token_amount {
        return Err(MorphTxError::FeeLimitTooLow {
            fee_limit,
            required: required_token_amount,
        });
    }

    // Check token balance >= required amount (use min of fee_limit and required)
    let amount_to_pay = fee_limit.min(required_token_amount);
    if token_info.balance < amount_to_pay {
        return Err(MorphTxError::InsufficientTokenBalance {
            token_id: fee_token_id,
            token_address: token_info.token_address,
            balance: token_info.balance,
            required: amount_to_pay,
        });
    }

    Ok(MorphTxValidationResult {
        token_info,
        required_token_amount,
        amount_to_pay,
    })
}

/// Helper function to extract MorphTx fields from a consensus transaction.
///
/// Returns `None` if this is not a MorphTx or if required fields are missing.
pub fn extract_morph_tx_fields(tx: &MorphTxEnvelope) -> Option<(u16, U256)> {
    let fee_token_id = tx.fee_token_id()?;
    let fee_limit = tx.fee_limit()?;
    Some((fee_token_id, fee_limit))
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
