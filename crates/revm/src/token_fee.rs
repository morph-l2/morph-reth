//! Token Fee Info for Morph L2 ERC20 gas payment.
//!
//! This module provides the infrastructure for fetching ERC20 token prices
//! and calculating gas fees in alternative tokens.
//!
//! Reference: <https://github.com/morph-l2/revm/blob/release/v42/crates/revm/src/morph/token_fee.rs>

use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, U256, address, keccak256};
use morph_chainspec::hardfork::MorphHardfork;
use revm::SystemCallEvm;
use revm::{Database as RevmDatabase, Inspector, context_interface::result::EVMError, inspector::NoOpInspector};

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

#[derive(Clone, Debug)]
struct TokenRegistryEntry {
    token_address: Address,
    is_active: bool,
    decimals: u8,
    price_ratio: U256,
    scale: U256,
    balance_slot: Option<U256>,
}

impl TokenFeeInfo {
    /// Fetch token fee information with EVM call fallback.
    ///
    /// Reads token parameters from L2 Token Registry storage. If the token's
    /// balance slot is unknown, falls back to an EVM `balanceOf` call.
    /// Returns `None` if the token is not registered.
    pub fn fetch<DB: Database>(
        db: &mut DB,
        token_id: u16,
        caller: Address,
        hardfork: MorphHardfork,
    ) -> Result<Option<Self>, DB::Error> {
        let entry = match load_registry_entry(db, token_id)? {
            Some(e) => e,
            None => return Ok(None),
        };

        let balance =
            read_erc20_balance(db, entry.token_address, caller, entry.balance_slot, hardfork)?;

        Ok(Some(Self {
            token_address: entry.token_address,
            is_active: entry.is_active,
            decimals: entry.decimals,
            price_ratio: entry.price_ratio,
            scale: entry.scale,
            caller,
            balance,
            balance_slot: entry.balance_slot,
        }))
    }

    /// Fetch token fee information using storage-only reads.
    ///
    /// Does not execute EVM calls. If the balance slot is unknown,
    /// the returned `balance` will be zero.
    pub fn fetch_storage_only<DB: RevmDatabase>(
        db: &mut DB,
        token_id: u16,
        caller: Address,
    ) -> Result<Option<Self>, DB::Error> {
        let entry = match load_registry_entry(db, token_id)? {
            Some(e) => e,
            None => return Ok(None),
        };

        let balance = read_balance_slot(db, entry.token_address, caller, entry.balance_slot)?;

        Ok(Some(Self {
            token_address: entry.token_address,
            is_active: entry.is_active,
            decimals: entry.decimals,
            price_ratio: entry.price_ratio,
            scale: entry.scale,
            caller,
            balance,
            balance_slot: entry.balance_slot,
        }))
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

fn load_registry_entry<DB: RevmDatabase>(
    db: &mut DB,
    token_id: u16,
) -> Result<Option<TokenRegistryEntry>, DB::Error> {
    // Get the base slot for this token_id in tokenRegistry mapping
    let mut token_id_bytes = [0u8; 32];
    token_id_bytes[30..32].copy_from_slice(&token_id.to_be_bytes());
    let base = mapping_slot(TOKEN_REGISTRY_SLOT, token_id_bytes.to_vec());

    // TokenInfo struct layout in storage (Solidity packing):
    // base + 0: tokenAddress (20 bytes) + padding
    // base + 1: balanceSlot (32 bytes)
    // base + 2: isActive (1 byte) + decimals (1 byte) + padding
    // base + 3: scale (32 bytes)

    let slot_0 = db.storage(L2_TOKEN_REGISTRY_ADDRESS, base)?;
    let token_address = Address::from_word(slot_0.into());
    if token_address == Address::default() {
        return Ok(None);
    }

    let balance_slot_raw = db.storage(L2_TOKEN_REGISTRY_ADDRESS, base + U256::from(1))?;
    let balance_slot = if !balance_slot_raw.is_zero() {
        Some(balance_slot_raw.saturating_sub(U256::from(1)))
    } else {
        None
    };

    // isActive at byte 31, decimals at byte 30 (big-endian)
    let slot_2 = db.storage(L2_TOKEN_REGISTRY_ADDRESS, base + U256::from(2))?;
    let bytes = slot_2.to_be_bytes::<32>();
    let is_active = bytes[31] != 0;
    let decimals = bytes[30];

    let scale = db.storage(L2_TOKEN_REGISTRY_ADDRESS, base + U256::from(3))?;

    // Get price ratio from priceRatio mapping
    let price_ratio = load_mapping_value(
        db,
        L2_TOKEN_REGISTRY_ADDRESS,
        PRICE_RATIO_SLOT,
        token_id_bytes.to_vec(),
    )?;

    Ok(Some(TokenRegistryEntry {
        token_address,
        is_active,
        decimals,
        price_ratio,
        scale,
        balance_slot,
    }))
}

/// Calculate the storage slot for a Solidity mapping value.
///
/// For `mapping(keyType => valueType)` at slot `base_slot`,
/// the value for `key` is at `keccak256(key ++ base_slot)`.
pub fn mapping_slot(base_slot: U256, mut key: Vec<u8>) -> U256 {
    let mut preimage = base_slot.to_be_bytes_vec();
    key.append(&mut preimage);
    U256::from_be_bytes(keccak256(key).0)
}

/// Calculate mapping slot for an address key (left-padded to 32 bytes).
#[inline]
pub fn mapping_slot_for(base_slot: U256, account: Address) -> U256 {
    let mut key = [0u8; 32];
    key[12..32].copy_from_slice(account.as_slice());
    mapping_slot(base_slot, key.to_vec())
}

/// Load a value from a mapping in contract storage.
fn load_mapping_value<DB: RevmDatabase>(
    db: &mut DB,
    contract: Address,
    base_slot: U256,
    key: Vec<u8>,
) -> Result<U256, DB::Error> {
    db.storage(contract, mapping_slot(base_slot, key))
}

/// Read ERC20 balance with EVM call fallback.
///
/// If `balance_slot` is known, reads directly from storage.
/// Otherwise, constructs a temporary EVM to call `balanceOf(address)`.
fn read_erc20_balance<DB: Database>(
    db: &mut DB,
    token: Address,
    account: Address,
    balance_slot: Option<U256>,
    hardfork: MorphHardfork,
) -> Result<U256, DB::Error> {
    if balance_slot.is_some() {
        return read_balance_slot(db, token, account, balance_slot);
    }

    // EVM fallback: construct temporary MorphEvm for balanceOf call
    let db: &mut dyn Database<Error = DB::Error> = db;
    let mut evm = MorphEvm::new(MorphContext::new(db, hardfork), NoOpInspector {});

    match call_balance_of(&mut evm, token, account) {
        Ok(balance) => Ok(balance),
        Err(EVMError::Database(e)) => Err(e),
        Err(_) => Ok(U256::ZERO), // Non-DB errors → zero (safe fallback)
    }
}

/// Read ERC20 balance directly from storage slot.
///
/// Returns zero if `balance_slot` is `None`.
fn read_balance_slot<DB: RevmDatabase>(
    db: &mut DB,
    token: Address,
    account: Address,
    balance_slot: Option<U256>,
) -> Result<U256, DB::Error> {
    match balance_slot {
        Some(slot) => {
            let mut key = [0u8; 32];
            key[12..32].copy_from_slice(account.as_slice());
            load_mapping_value(db, token, slot, key.to_vec())
        }
        None => Ok(U256::ZERO),
    }
}

/// Execute EVM `balanceOf(address)` call.
fn call_balance_of<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    token: Address,
    account: Address,
) -> Result<U256, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
    I: Inspector<MorphContext<DB>>,
{
    let calldata = encode_balance_of(account);
    match evm.system_call_one(token, calldata) {
        Ok(result) if result.is_success() => {
            if let Some(output) = result.output()
                && output.len() >= 32
            {
                return Ok(U256::from_be_slice(&output[..32]));
            }
            Ok(U256::ZERO)
        }
        Ok(_) => Ok(U256::ZERO),
        Err(_) => Ok(U256::ZERO),
    }
}

/// Query ERC20 balance via EVM call.
///
/// Use this when you have a `MorphEvm` instance and need to call `balanceOf`.
pub fn erc20_balance_of<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    token: Address,
    account: Address,
) -> Result<U256, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
    I: Inspector<MorphContext<DB>>,
{
    call_balance_of(evm, token, account)
}

/// Encode ERC20 `balanceOf(address)` calldata.
///
/// Function selector: `0x70a08231`
pub fn encode_balance_of(account: Address) -> Bytes {
    const SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&SELECTOR);
    data.extend_from_slice(&[0u8; 12]); // Left-pad address
    data.extend_from_slice(account.as_slice());
    Bytes::from(data)
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
    fn test_mapping_slot() {
        // Test that mapping slot calculation produces deterministic results
        let slot = U256::from(151);
        let key = vec![0u8; 32];
        let result1 = mapping_slot(slot, key.clone());
        let result2 = mapping_slot(slot, key);
        assert_eq!(result1, result2);
    }

    #[test]
    fn test_mapping_slot_for() {
        let slot = U256::from(1);
        let account = address!("1234567890123456789012345678901234567890");
        let result = mapping_slot_for(slot, account);
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
    fn test_encode_balance_of() {
        let account = address!("1234567890123456789012345678901234567890");
        let calldata = encode_balance_of(account);

        // Should be 4 bytes selector + 32 bytes address = 36 bytes
        assert_eq!(calldata.len(), 36);
        // First 4 bytes should be the selector
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
