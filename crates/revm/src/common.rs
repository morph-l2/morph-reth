use crate::MorphTxEnv;
use alloy_primitives::{Address, U256};
use revm::{Database, context::JournalTr, state::AccountInfo};

/// Helper trait to abstract over different representations of Morph transactions.
#[auto_impl::auto_impl(&)]
pub trait MorphTx {
    /// Returns the transaction's caller address.
    fn caller(&self) -> Address;
}

impl MorphTx for MorphTxEnv {
    fn caller(&self) -> Address {
        revm::context::Transaction::caller(self)
    }
}

/// Helper trait to perform Morph-specific operations on top of different state providers.
pub trait MorphStateAccess<T> {
    type Error;

    /// Returns [`AccountInfo`] for the given address.
    fn basic(&mut self, address: Address) -> Result<AccountInfo, Self::Error>;

    /// Returns the storage value for the given address and key.
    fn sload(&mut self, address: Address, key: U256) -> Result<U256, Self::Error>;
}

impl<DB: Database> MorphStateAccess<()> for DB {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<AccountInfo, Self::Error> {
        self.basic(address).map(Option::unwrap_or_default)
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256, Self::Error> {
        self.storage(address, key)
    }
}

impl<T: JournalTr> MorphStateAccess<((), ())> for T {
    type Error = <T::Database as Database>::Error;

    fn basic(&mut self, address: Address) -> Result<AccountInfo, Self::Error> {
        self.load_account(address).map(|s| s.data.info.clone())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256, Self::Error> {
        JournalTr::sload(self, address, key).map(|s| s.data)
    }
}

#[cfg(feature = "reth")]
impl<T: reth_storage_api::StateProvider> MorphStateAccess<((), (), ())> for T {
    type Error = reth_evm::execute::ProviderError;

    fn basic(&mut self, address: Address) -> Result<AccountInfo, Self::Error> {
        self.basic_account(&address)
            .map(Option::unwrap_or_default)
            .map(Into::into)
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256, Self::Error> {
        self.storage(address, key.into())
            .map(Option::unwrap_or_default)
    }
}
