//! Token Fee Info for Morph L2 ERC20 gas payment.
//!
//! This module provides the infrastructure for fetching ERC20 token prices
//! and calculating gas fees in alternative tokens.
//!
//! Reference: <https://github.com/morph-l2/revm/blob/release/v42/crates/revm/src/morph/token_fee.rs>

use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, TxKind, U256, address, keccak256};
use morph_chainspec::hardfork::MorphHardfork;
use morph_primitives::L1_TX_TYPE_ID;
use revm::{
    ExecuteEvm, Inspector, context::TxEnv, context_interface::result::EVMError,
    inspector::NoOpInspector,
};

use crate::evm::MorphContext;
use crate::{MorphEvm, MorphInvalidTransaction};

/// L2 Token Registry contract address on Morph L2.
/// Reference: <https://github.com/morph-l2/morph/blob/main/contracts/contracts/l2/system/L2TokenRegistry.sol>
pub const L2_TOKEN_REGISTRY_ADDRESS: Address = address!("5300000000000000000000000000000000000021");

/// TokenRegistry storage slot for mapping(uint16 => TokenInfo) - slot 151.
const TOKEN_REGISTRY_SLOT: U256 = U256::from_limbs([151u64, 0, 0, 0]);
/// PriceRatio storage slot for mapping(uint16 => uint256) - slot 153.
const PRICE_RATIO_SLOT: U256 = U256::from_limbs([153u64, 0, 0, 0]);

/// Token fee information for ERC20 gas payment.
///
/// Contains the token parameters fetched from the L2 Token Registry contract.
/// These parameters are used to calculate gas fees in alternative ERC20 tokens.
#[derive(Clone, Debug, Default)]
pub struct TokenFeeInfo {
    /// The fee token address.
    pub token_address: Address,
    /// Whether the token is active for gas payment.
    pub is_active: bool,
    /// Token decimals.
    pub decimals: u8,
    /// The price ratio of the token (relative to ETH).
    pub price_ratio: U256,
    /// The scale of the token for price calculation.
    pub scale: U256,
    /// The caller address.
    pub caller: Address,
    /// The token balance of the caller.
    pub balance: U256,
    /// The user's ERC20 balance storage slot (if known).
    pub balance_slot: Option<U256>,
}

impl TokenFeeInfo {
    /// Try to fetch the token fee information from the database.
    ///
    /// This reads the token parameters from the L2 Token Registry contract storage.
    /// Returns `None` if the token is not registered.
    pub fn try_fetch<DB: Database>(
        db: &mut DB,
        token_id: u16,
        caller: Address,
    ) -> Result<Option<Self>, DB::Error> {
        // Get the base slot for this token_id in tokenRegistry mapping
        let mut token_id_bytes = [0u8; 32];
        token_id_bytes[30..32].copy_from_slice(&token_id.to_be_bytes());
        let token_registry_base = get_mapping_slot(TOKEN_REGISTRY_SLOT, token_id_bytes.to_vec());

        // TokenInfo struct layout in storage (following Solidity storage packing rules):
        // slot + 0: tokenAddress (address, 20 bytes) + 12 bytes padding
        // slot + 1: balanceSlot (bytes32, 32 bytes)
        // slot + 2: isActive (bool, 1 byte) + decimals (uint8, 1 byte) + 30 bytes padding
        // slot + 3: scale (uint256, 32 bytes)

        // Read tokenAddress from slot + 0
        let slot_0 = db.storage(L2_TOKEN_REGISTRY_ADDRESS, token_registry_base)?;
        let token_address = Address::from_word(slot_0.into());
        if token_address == Address::default() {
            return Ok(None);
        }

        // Read balanceSlot from slot + 1
        let balance_slot_value = db.storage(
            L2_TOKEN_REGISTRY_ADDRESS,
            token_registry_base + U256::from(1),
        )?;
        let token_balance_slot = if !balance_slot_value.is_zero() {
            Some(balance_slot_value.saturating_sub(U256::from(1u64)))
        } else {
            None
        };

        // Read isActive and decimals from slot + 2
        // In big-endian representation, rightmost byte is the lowest position
        // isActive is at the rightmost (byte 31), decimals is to its left (byte 30)
        let slot_2 = db.storage(
            L2_TOKEN_REGISTRY_ADDRESS,
            token_registry_base + U256::from(2),
        )?;
        let slot_2_bytes = slot_2.to_be_bytes::<32>();
        let is_active = slot_2_bytes[31] != 0;
        let decimals = slot_2_bytes[30];

        // Read scale from slot + 3
        let scale = db.storage(
            L2_TOKEN_REGISTRY_ADDRESS,
            token_registry_base + U256::from(3),
        )?;

        // Get price ratio from priceRatio mapping
        let price_ratio = load_mapping_value(
            db,
            L2_TOKEN_REGISTRY_ADDRESS,
            PRICE_RATIO_SLOT,
            token_id_bytes.to_vec(),
        )?;

        // Get caller's token balance
        let caller_token_balance =
            get_erc20_balance(db, token_address, caller, token_balance_slot)?;

        let token_fee = Self {
            token_address,
            is_active,
            decimals,
            price_ratio,
            scale,
            caller,
            balance: caller_token_balance,
            balance_slot: token_balance_slot,
        };

        Ok(Some(token_fee))
    }

    /// Calculate the token amount required for a given ETH amount.
    ///
    /// Uses the price ratio and scale to convert ETH value to token amount.
    pub fn calculate_token_amount(&self, eth_amount: U256) -> U256 {
        if self.price_ratio.is_zero() {
            return U256::ZERO;
        }

        // token_amount = eth_amount * scale / price_ratio
        let (token_amount, remainder) = eth_amount
            .saturating_mul(self.scale)
            .div_rem(self.price_ratio);
        // If there's a remainder, round up by adding 1
        if !remainder.is_zero() {
            token_amount.saturating_add(U256::from(1))
        } else {
            token_amount
        }
    }

    /// Check if the caller has sufficient token balance for the given ETH amount.
    pub fn has_sufficient_balance(&self, eth_amount: U256) -> bool {
        let required = self.calculate_token_amount(eth_amount);
        self.balance >= required
    }
}

/// Calculate the storage slot for a mapping value.
///
/// For a mapping `mapping(keyType => valueType)` at storage slot `slot_index`,
/// the value for `key` is stored at `keccak256(key || slot_index)`.
pub fn get_mapping_slot(slot_index: U256, mut key: Vec<u8>) -> U256 {
    let mut pre_image = slot_index.to_be_bytes_vec();
    key.append(&mut pre_image);
    let storage_key = keccak256(key);
    U256::from_be_bytes(storage_key.0)
}

/// Calculate the account's storage slot for a mapping value.
///
/// For address-keyed mappings, the address is left-padded to 32 bytes.
#[inline]
pub fn get_mapping_account_slot(slot_index: U256, account: Address) -> U256 {
    let mut key = [0u8; 32];
    key[12..32].copy_from_slice(account.as_slice());
    get_mapping_slot(slot_index, key.to_vec())
}

/// Load a value from a mapping in contract storage.
fn load_mapping_value<DB: Database>(
    db: &mut DB,
    account: Address,
    slot_index: U256,
    key: Vec<u8>,
) -> Result<U256, DB::Error> {
    let storage_slot = get_mapping_slot(slot_index, key);
    let storage_value = db.storage(account, storage_slot)?;
    Ok(storage_value)
}

/// Gas limit for ERC20 balance query calls.
const BALANCE_OF_GAS_LIMIT: u64 = 200000;

/// Get ERC20 token balance for an account (storage-only version).
///
/// First tries to read directly from storage if the balance slot is known.
/// If the balance slot is not available, returns zero.
///
/// Use [`get_erc20_balance_with_evm`] if you have access to a `MorphEvm` instance
/// and need the EVM call fallback.
pub fn get_erc20_balance<DB: Database>(
    db: &mut DB,
    token: Address,
    account: Address,
    token_balance_slot: Option<U256>,
) -> Result<U256, DB::Error> {
    // If balance slot is provided, read directly from storage
    if let Some(slot) = token_balance_slot {
        let mut data = [0u8; 32];
        data[12..32].copy_from_slice(account.as_slice());
        load_mapping_value(db, token, slot, data.to_vec())
    } else {
        // For the EVM fallback we construct a temporary MorphEvm instance.
        //
        // Notes:
        // - `MorphContext::new` requires a hardfork/spec parameter.
        // - We pass `&mut DB` as the context database type (so we don't move `db`).
        // - `NoOpInspector` satisfies the `Inspector` bound without adding side effects.
        let db: &mut dyn Database<Error = DB::Error> = db;

        let mut evm = MorphEvm::new(
            MorphContext::new(db, MorphHardfork::Curie),
            NoOpInspector {},
        );

        match get_erc20_balance_with_evm(&mut evm, token, account) {
            Ok(balance) => Ok(balance),
            Err(EVMError::Database(db_err)) => Err(db_err),
            // For non-database EVM errors, fall back to zero (matches original behavior).
            Err(_) => Ok(U256::ZERO),
        }
    }
}

/// Get ERC20 token balance for an account with EVM call fallback.
///
/// First tries to read directly from storage if the balance slot is known.
/// If the balance slot is not available, falls back to executing an EVM call
/// to `balanceOf(address)` using the provided `MorphEvm` instance.
pub fn get_erc20_balance_with_evm<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    token: Address,
    account: Address,
) -> Result<U256, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
    I: Inspector<MorphContext<DB>>,
{
    // Fallback: Execute EVM call to balanceOf(address)
    let calldata = build_balance_of_calldata(account);

    // Create a minimal transaction environment for the call
    let tx = TxEnv {
        caller: Address::ZERO,
        gas_limit: BALANCE_OF_GAS_LIMIT,
        kind: TxKind::Call(token),
        value: U256::ZERO,
        data: calldata,
        nonce: 0,
        tx_type: L1_TX_TYPE_ID, // Mark as L1 message to skip gas validation
        ..Default::default()
    };

    // Convert to MorphTxEnv
    let morph_tx = crate::MorphTxEnv::new(tx);

    // Execute using transact_one
    match evm.transact_one(morph_tx) {
        Ok(result) => {
            if result.is_success() {
                // Parse the returned balance (32 bytes)
                if let Some(output) = result.output()
                    && output.len() >= 32
                {
                    return Ok(U256::from_be_slice(&output[..32]));
                }
            }
            Ok(U256::ZERO)
        }
        Err(_) => {
            // On error, return zero (matches original behavior)
            Ok(U256::ZERO)
        }
    }
}

/// Build the calldata for ERC20 balanceOf(address) call.
///
/// Method signature: `balanceOf(address) -> 0x70a08231`
pub fn build_balance_of_calldata(account: Address) -> Bytes {
    let method_id = [0x70u8, 0xa0, 0x82, 0x31];
    let mut calldata = Vec::with_capacity(36);
    calldata.extend_from_slice(&method_id);
    calldata.extend_from_slice(&[0u8; 12]); // Pad address to 32 bytes
    calldata.extend_from_slice(account.as_slice());
    Bytes::from(calldata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_fee_info_default() {
        let info = TokenFeeInfo::default();
        assert_eq!(info.token_address, Address::default());
        assert!(!info.is_active);
        assert_eq!(info.decimals, 0);
        assert_eq!(info.price_ratio, U256::ZERO);
        assert_eq!(info.scale, U256::ZERO);
        assert_eq!(info.balance, U256::ZERO);
        assert!(info.balance_slot.is_none());
    }

    #[test]
    fn test_get_mapping_slot() {
        // Test that mapping slot calculation produces deterministic results
        let slot = U256::from(151);
        let key = vec![0u8; 32];
        let result1 = get_mapping_slot(slot, key.clone());
        let result2 = get_mapping_slot(slot, key);
        assert_eq!(result1, result2);
    }

    #[test]
    fn test_get_mapping_account_slot() {
        let slot = U256::from(1);
        let account = address!("1234567890123456789012345678901234567890");
        let result = get_mapping_account_slot(slot, account);
        // Result should be non-zero
        assert!(!result.is_zero());
    }

    #[test]
    fn test_calculate_token_amount() {
        let info = TokenFeeInfo {
            price_ratio: U256::from(2_000_000_000_000_000_000u128), // 2 ETH per token
            scale: U256::from(1_000_000_000_000_000_000u128),       // 1e18
            ..Default::default()
        };

        // 1 ETH should give 0.5 tokens
        let eth_amount = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
        let token_amount = info.calculate_token_amount(eth_amount);
        assert_eq!(token_amount, U256::from(500_000_000_000_000_000u128)); // 0.5 tokens
    }

    #[test]
    fn test_calculate_token_amount_zero_ratio() {
        let info = TokenFeeInfo {
            price_ratio: U256::ZERO,
            scale: U256::from(1_000_000_000_000_000_000u128),
            ..Default::default()
        };

        let eth_amount = U256::from(1_000_000_000_000_000_000u128);
        let token_amount = info.calculate_token_amount(eth_amount);
        assert_eq!(token_amount, U256::ZERO);
    }

    #[test]
    fn test_has_sufficient_balance() {
        let info = TokenFeeInfo {
            price_ratio: U256::from(1_000_000_000_000_000_000u128), // 1:1
            scale: U256::from(1_000_000_000_000_000_000u128),
            balance: U256::from(1_000_000_000_000_000_000u128), // 1 token
            ..Default::default()
        };

        // Has exactly enough
        assert!(info.has_sufficient_balance(U256::from(1_000_000_000_000_000_000u128)));
        // Has more than enough
        assert!(info.has_sufficient_balance(U256::from(500_000_000_000_000_000u128)));
        // Not enough
        assert!(!info.has_sufficient_balance(U256::from(2_000_000_000_000_000_000u128)));
    }

    #[test]
    fn test_build_balance_of_calldata() {
        let account = address!("1234567890123456789012345678901234567890");
        let calldata = build_balance_of_calldata(account);

        // Should be 4 bytes method id + 32 bytes address = 36 bytes
        assert_eq!(calldata.len(), 36);
        // First 4 bytes should be the method id
        assert_eq!(&calldata[0..4], &[0x70, 0xa0, 0x82, 0x31]);
        // Last 20 bytes should be the address
        assert_eq!(&calldata[16..36], account.as_slice());
    }

    #[test]
    fn test_l2_token_registry_address() {
        // Verify the constant is set correctly
        assert_eq!(
            L2_TOKEN_REGISTRY_ADDRESS,
            address!("5300000000000000000000000000000000000021")
        );
    }
}
