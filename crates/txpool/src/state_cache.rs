//! Tip-scoped concurrent state cache and reusable `evm2` token-fee validator for Morph txpool.

use crate::{MorphTxError, MorphTxValidationError};
use alloy_primitives::{Address, B256, U256, map::DefaultHashBuilder};
use dashmap::DashMap;
use evm2::{
    bytecode::Bytecode,
    evm::{AccountInfo, Database, Db},
    registry::HandlerError,
};
use morph_revm::{MorphEvm, MorphEvmEnv, TokenFeeInfo, TokenRegistryEntry, morph_evm};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Concurrent cache of raw state reads and resolved token-fee queries anchored to a single tip.
///
/// Transaction admission and pool maintenance repeatedly read the same L2 token registry slots,
/// token contract bytecode, and sender balances across transactions. Anchoring this cache to a
/// canonical tip hash lets all concurrent validations at that tip share reads in memory while
/// replacing the cache atomically whenever the head block advances.
#[derive(Debug, Default)]
pub(crate) struct StateCache {
    /// Cached basic account info, including non-existent accounts (`None`).
    accounts: DashMap<Address, Option<AccountInfo>, DefaultHashBuilder>,
    /// Cached storage values keyed by `(account, slot)`.
    storage: DashMap<(Address, U256), U256, DefaultHashBuilder>,
    /// Cached contract bytecode keyed by `code_hash`.
    contracts: DashMap<B256, Bytecode, DefaultHashBuilder>,
    /// Cached L2 token registry metadata keyed by `token_id`.
    registry_entries: DashMap<u16, Option<TokenRegistryEntry>, DefaultHashBuilder>,
    /// Cached call-mode `balanceOf` outcomes keyed by `(caller, token_id)`.
    ///
    /// Stores `Ok(balance)` on success and `Err(())` on deterministic EVM query failures
    /// (`TokenBalanceQueryFailed`). Transient database read errors (`HandlerError::Database` /
    /// `HandlerError::Fatal`) are never inserted here so a retry against healthy storage can
    /// succeed.
    call_balances: DashMap<(Address, u16), Result<U256, ()>, DefaultHashBuilder>,
    account_count: AtomicUsize,
    storage_count: AtomicUsize,
    contract_count: AtomicUsize,
    registry_count: AtomicUsize,
    call_balance_count: AtomicUsize,
}

impl StateCache {
    const MAX_ACCOUNTS: usize = 1 << 17;
    const MAX_STORAGE_SLOTS: usize = 1 << 18;
    const MAX_CONTRACTS: usize = 1 << 12;
    const MAX_REGISTRY_ENTRIES: usize = 1 << 12;
    const MAX_CALL_BALANCES: usize = 1 << 16;

    fn insert_registry_entry(&self, token_id: u16, entry: Option<TokenRegistryEntry>) {
        if self.registry_count.load(Ordering::Relaxed) < Self::MAX_REGISTRY_ENTRIES
            && self.registry_entries.insert(token_id, entry).is_none()
        {
            self.registry_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn insert_call_balance(&self, key: (Address, u16), outcome: Result<U256, ()>) {
        if self.call_balance_count.load(Ordering::Relaxed) < Self::MAX_CALL_BALANCES
            && self.call_balances.insert(key, outcome).is_none()
        {
            self.call_balance_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// [`Database`] adapter that serves reads from a shared [`StateCache`] and populates it on miss.
#[derive(Debug)]
pub(crate) struct StateCacheDb<'a, DB> {
    cache: &'a StateCache,
    db: DB,
}

impl<'a, DB> StateCacheDb<'a, DB> {
    pub(crate) const fn new(cache: &'a StateCache, db: DB) -> Self {
        Self { cache, db }
    }
}

impl<DB: Database> Database for StateCacheDb<'_, DB> {
    type Error = DB::Error;

    fn is_fatal(error: &Self::Error) -> bool {
        DB::is_fatal(error)
    }

    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(account) = self.cache.accounts.get(address) {
            return Ok(account.clone());
        }
        let account = self.db.get_account(address)?;
        if self.cache.account_count.load(Ordering::Relaxed) < StateCache::MAX_ACCOUNTS
            && self
                .cache
                .accounts
                .insert(*address, account.clone())
                .is_none()
        {
            self.cache.account_count.fetch_add(1, Ordering::Relaxed);
        }
        Ok(account)
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
        if let Some(code) = self.cache.contracts.get(code_hash) {
            return Ok(code.clone());
        }
        let code = self.db.get_code_by_hash(code_hash)?;
        if self.cache.contract_count.load(Ordering::Relaxed) < StateCache::MAX_CONTRACTS
            && self
                .cache
                .contracts
                .insert(*code_hash, code.clone())
                .is_none()
        {
            self.cache.contract_count.fetch_add(1, Ordering::Relaxed);
        }
        Ok(code)
    }

    fn get_storage(&mut self, address: &Address, index: &U256) -> Result<U256, Self::Error> {
        if let Some(value) = self.cache.storage.get(&(*address, *index)) {
            return Ok(*value);
        }
        let value = self.db.get_storage(address, index)?;
        if self.cache.storage_count.load(Ordering::Relaxed) < StateCache::MAX_STORAGE_SLOTS
            && self
                .cache
                .storage
                .insert((*address, *index), value)
                .is_none()
        {
            self.cache.storage_count.fetch_add(1, Ordering::Relaxed);
        }
        Ok(value)
    }

    fn get_block_hash(&mut self, number: &U256) -> Result<B256, Self::Error> {
        self.db.get_block_hash(number)
    }
}

/// Reusable `evm2`-backed token-fee resolver for a batch of transactions or maintenance pass.
///
/// Constructing [`MorphEvm`] once per batch is essentially free (`InterpreterPool` and `State`
/// allocate nothing until a call-mode `balanceOf` actually runs), and when call-mode tokens are
/// queried, the pooled interpreter frame and backing [`StateCache`] are reused across every
/// transaction without any manual frame-stack teardown.
pub(crate) struct TokenFeeValidator<'a> {
    cache: &'a StateCache,
    evm: MorphEvm<'a>,
}

impl<'a> TokenFeeValidator<'a> {
    /// Creates a reusable validator backed by `cache` and `db` under `env`.
    pub(crate) fn new<DB: Database + 'a>(cache: &'a StateCache, db: DB, env: MorphEvmEnv) -> Self {
        let cached_db = StateCacheDb::new(cache, db);
        Self {
            cache,
            evm: morph_evm(Db::new(cached_db), env),
        }
    }

    /// Resolves and validates `token_id` metadata and `caller`'s token balance.
    ///
    /// Enforces registry usability (`is_active`, non-zero `price_ratio`, non-zero `scale`)
    /// strictly before any call-mode `balanceOf` execution.
    ///
    /// Separates deterministic transaction/token verdicts ([`MorphTxValidationError::Invalid`])
    /// from transient database/provider read failures ([`MorphTxValidationError::State`]).
    pub(crate) fn resolve_token_fee_info(
        &mut self,
        token_id: u16,
        caller: Address,
    ) -> Result<TokenFeeInfo, MorphTxValidationError<String>> {
        let entry = match self.cache.registry_entries.get(&token_id) {
            Some(cached) => *cached,
            None => {
                let loaded = TokenRegistryEntry::load_from_evm(&mut self.evm, token_id)
                    .map_err(|err| MorphTxValidationError::State(err.to_string()))?;
                self.cache.insert_registry_entry(token_id, loaded);
                loaded
            }
        };

        let entry = entry.ok_or(MorphTxError::TokenNotFound { token_id })?;

        if !entry.is_active {
            return Err(MorphTxError::TokenNotActive { token_id }.into());
        }
        if entry.price_ratio.is_zero() {
            return Err(MorphTxError::InvalidPriceRatio { token_id }.into());
        }
        if entry.scale.is_zero() {
            return Err(MorphTxError::InvalidTokenScale { token_id }.into());
        }

        if entry.balance_slot.is_some() {
            let balance = entry
                .read_caller_balance_with_evm(&mut self.evm, caller)
                .map_err(|err| MorphTxValidationError::State(err.to_string()))?;
            return Ok(entry.into_fee_info(caller, balance));
        }

        // Call mode (`balance_slot == None`): deduplicate `balanceOf` per `(caller, token_id)`.
        if let Some(cached) = self.cache.call_balances.get(&(caller, token_id)) {
            return match *cached.value() {
                Ok(balance) => Ok(entry.into_fee_info(caller, balance)),
                Err(()) => Err(MorphTxError::TokenBalanceQueryFailed { token_id }.into()),
            };
        }

        match entry.read_caller_balance_with_evm(&mut self.evm, caller) {
            Ok(balance) => {
                self.cache
                    .insert_call_balance((caller, token_id), Ok(balance));
                Ok(entry.into_fee_info(caller, balance))
            }
            // Never cache transient state/database read errors as zero or failed balances.
            Err(HandlerError::Database(err)) => Err(MorphTxValidationError::State(err.to_string())),
            Err(HandlerError::Fatal(err)) => Err(MorphTxValidationError::State(err.to_string())),
            Err(_) => {
                self.cache.insert_call_balance((caller, token_id), Err(()));
                Err(MorphTxError::TokenBalanceQueryFailed { token_id }.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, address, bytes};
    use evm2::evm::{CacheDB, DynDatabase, EmptyDB, InMemoryDB};
    use morph_chainspec::hardfork::MorphHardfork;
    use morph_revm::{L2_TOKEN_REGISTRY_ADDRESS, MorphBlockEnv, compute_mapping_slot};
    use std::sync::Arc;

    #[derive(Debug, Clone, Copy)]
    struct TransientDbError;

    impl core::fmt::Display for TransientDbError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("transient storage failure")
        }
    }

    impl core::error::Error for TransientDbError {}

    struct CountingTokenDb {
        inner: InMemoryDB,
        token: Address,
        fail_token_storage: bool,
        token_storage_reads: Arc<AtomicUsize>,
    }

    impl Database for CountingTokenDb {
        type Error = TransientDbError;

        fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(DynDatabase::get_account(&mut self.inner, address).unwrap())
        }

        fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
            Ok(DynDatabase::get_code_by_hash(&mut self.inner, code_hash).unwrap())
        }

        fn get_storage(&mut self, address: &Address, index: &U256) -> Result<U256, Self::Error> {
            if *address == self.token {
                self.token_storage_reads.fetch_add(1, Ordering::Relaxed);
                if self.fail_token_storage {
                    return Err(TransientDbError);
                }
            }
            Ok(DynDatabase::get_storage(&mut self.inner, address, index).unwrap())
        }

        fn get_block_hash(&mut self, number: &U256) -> Result<B256, Self::Error> {
            Ok(DynDatabase::get_block_hash(&mut self.inner, number).unwrap())
        }
    }

    fn setup_call_mode_registry(
        token_id: u16,
        token: Address,
        is_active: bool,
        price_ratio: U256,
        scale: U256,
        code: Bytes,
        slot0_balance: U256,
    ) -> InMemoryDB {
        let mut db = CacheDB::new(EmptyDB::default());
        let mut key = [0u8; 32];
        key[30..32].copy_from_slice(&token_id.to_be_bytes());
        let base = compute_mapping_slot(U256::from(151), &key);

        let mut packed = [0u8; 32];
        packed[30] = 18;
        packed[31] = u8::from(is_active);

        db.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &base,
            &U256::from_be_bytes(token.into_word().0),
        );
        db.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &(base + U256::from(1)),
            &U256::ZERO,
        );
        db.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &(base + U256::from(2)),
            &U256::from_be_bytes(packed),
        );
        db.insert_account_storage(&L2_TOKEN_REGISTRY_ADDRESS, &(base + U256::from(3)), &scale);
        db.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &compute_mapping_slot(U256::from(153), &key),
            &price_ratio,
        );

        db.insert_account_info(
            &token,
            AccountInfo::default().with_code(Bytecode::new_legacy(code)),
        );
        db.insert_account_storage(&token, &U256::ZERO, &slot0_balance);
        db
    }

    #[test]
    fn deduplicates_call_mode_balance_and_skips_bytecode_for_unusable_tokens() {
        let token = address!("5300000000000000000000000000000000000042");
        let caller = address!("1000000000000000000000000000000000000001");
        let env = MorphEvmEnv::new(MorphHardfork::Celadon, MorphBlockEnv::default(), 2818);

        // 1. Inactive / zero-scale / zero-price tokens never touch `token` storage.
        for (is_active, price, scale, expected) in [
            (
                false,
                U256::ONE,
                U256::ONE,
                MorphTxError::TokenNotActive { token_id: 1 },
            ),
            (
                true,
                U256::ZERO,
                U256::ONE,
                MorphTxError::InvalidPriceRatio { token_id: 1 },
            ),
            (
                true,
                U256::ONE,
                U256::ZERO,
                MorphTxError::InvalidTokenScale { token_id: 1 },
            ),
        ] {
            let reads = Arc::new(AtomicUsize::new(0));
            let db = CountingTokenDb {
                inner: setup_call_mode_registry(
                    1,
                    token,
                    is_active,
                    price,
                    scale,
                    bytes!("6000545f5260205ff3"),
                    U256::from(500u64),
                ),
                token,
                fail_token_storage: true,
                token_storage_reads: Arc::clone(&reads),
            };
            let cache = StateCache::default();
            let mut validator = TokenFeeValidator::new(&cache, db, env.clone());
            assert_eq!(
                validator.resolve_token_fee_info(1, caller).unwrap_err(),
                MorphTxValidationError::Invalid(expected)
            );
            assert_eq!(reads.load(Ordering::Relaxed), 0);
        }

        // 2. Active call-mode token runs `balanceOf` once and serves subsequent lookups from cache.
        let reads = Arc::new(AtomicUsize::new(0));
        let db = CountingTokenDb {
            inner: setup_call_mode_registry(
                1,
                token,
                true,
                U256::ONE,
                U256::ONE,
                bytes!("6000545f5260205ff3"),
                U256::from(999_000u64),
            ),
            token,
            fail_token_storage: false,
            token_storage_reads: Arc::clone(&reads),
        };
        let cache = StateCache::default();
        let mut validator = TokenFeeValidator::new(&cache, db, env);
        for _ in 0..5 {
            let info = validator.resolve_token_fee_info(1, caller).unwrap();
            assert_eq!(info.balance, U256::from(999_000u64));
        }
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn transient_db_errors_are_not_cached_while_deterministic_reverts_are() {
        let token = address!("5300000000000000000000000000000000000042");
        let caller = address!("1000000000000000000000000000000000000001");
        let env = MorphEvmEnv::new(MorphHardfork::Celadon, MorphBlockEnv::default(), 2818);
        let cache = StateCache::default();

        // First validator hits a transient DB error during `balanceOf` SLOAD -> must NOT poison `cache`
        // and must surface as `MorphTxValidationError::State`.
        let reads = Arc::new(AtomicUsize::new(0));
        let failing_db = CountingTokenDb {
            inner: setup_call_mode_registry(
                1,
                token,
                true,
                U256::ONE,
                U256::ONE,
                bytes!("6000545f5260205ff3"),
                U256::from(123_456u64),
            ),
            token,
            fail_token_storage: true,
            token_storage_reads: Arc::clone(&reads),
        };
        let mut v1 = TokenFeeValidator::new(&cache, failing_db, env.clone());
        assert!(matches!(
            v1.resolve_token_fee_info(1, caller).unwrap_err(),
            MorphTxValidationError::State(_)
        ));

        // Second validator sharing the same tip `cache` over healthy DB succeeds!
        let healthy_db = CountingTokenDb {
            inner: setup_call_mode_registry(
                1,
                token,
                true,
                U256::ONE,
                U256::ONE,
                bytes!("6000545f5260205ff3"),
                U256::from(123_456u64),
            ),
            token,
            fail_token_storage: false,
            token_storage_reads: Arc::clone(&reads),
        };
        let mut v2 = TokenFeeValidator::new(&cache, healthy_db, env.clone());
        let info = v2.resolve_token_fee_info(1, caller).unwrap();
        assert_eq!(info.balance, U256::from(123_456u64));

        // Deterministic contract revert (`SLOAD(0); PUSH0; PUSH0; REVERT`) IS cached as
        // `MorphTxValidationError::Invalid(TokenBalanceQueryFailed)`.
        let revert_cache = StateCache::default();
        let revert_reads = Arc::new(AtomicUsize::new(0));
        let revert_db = CountingTokenDb {
            inner: setup_call_mode_registry(
                1,
                token,
                true,
                U256::ONE,
                U256::ONE,
                bytes!("600054505f5ffd"),
                U256::ZERO,
            ),
            token,
            fail_token_storage: false,
            token_storage_reads: Arc::clone(&revert_reads),
        };
        let mut v3 = TokenFeeValidator::new(&revert_cache, revert_db, env);
        for _ in 0..4 {
            assert_eq!(
                v3.resolve_token_fee_info(1, caller).unwrap_err(),
                MorphTxValidationError::Invalid(MorphTxError::TokenBalanceQueryFailed {
                    token_id: 1,
                })
            );
        }
        assert_eq!(revert_reads.load(Ordering::Relaxed), 1);
    }
}
