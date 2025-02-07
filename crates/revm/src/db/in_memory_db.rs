use super::{DatabaseCommit, DatabaseRef};
use crate::{interpreter::bytecode::Bytecode, Database, KECCAK_EMPTY};
use crate::{Account, AccountInfo, Log};
use alloc::{
    collections::btree_map::{self, BTreeMap},
    vec::Vec,
};
use hashbrown::{hash_map::Entry, HashMap as Map};
use primitive_types::{H160, H256, U256};
use sha3::{Digest, Keccak256};

pub type InMemoryDB = CacheDB<EmptyDB>;

impl InMemoryDB {
    pub fn default() -> Self {
        CacheDB::new(EmptyDB {})
    }
}

/// Memory backend, storing all state values in a `Map` in memory.
#[derive(Debug, Clone)]
pub struct CacheDB<ExtDB: DatabaseRef> {
    /// Dummy account info where `code` is always `None`.
    /// Code bytes can be found in `contracts`.
    pub accounts: BTreeMap<H160, DbAccount>,
    pub contracts: Map<H256, Bytecode>,
    pub logs: Vec<Log>,
    pub block_hashes: Map<U256, H256>,
    pub db: ExtDB,
}

#[derive(Debug, Clone, Default)]
pub struct DbAccount {
    pub info: AccountInfo,
    /// If account is selfdestructed or newly created, storage will be cleared.
    pub account_state: AccountState,
    /// storage slots
    pub storage: BTreeMap<U256, U256>,
}

#[derive(Debug, Clone, Default)]
pub enum AccountState {
    /// EVM touched this account
    EVMTouched,
    /// EVM cleared storage of this account, mostly by selfdestruct
    EVMStorageCleared,
    /// EVM didnt interacted with this account
    #[default]
    None,
}

impl<ExtDB: DatabaseRef> CacheDB<ExtDB> {
    pub fn new(db: ExtDB) -> Self {
        let mut contracts = Map::new();
        contracts.insert(KECCAK_EMPTY, Bytecode::new());
        contracts.insert(H256::zero(), Bytecode::new());
        Self {
            accounts: BTreeMap::new(),
            contracts,
            logs: Vec::default(),
            block_hashes: Map::new(),
            db,
        }
    }

    pub fn insert_contract(&mut self, account: &mut AccountInfo) {
        if let Some(code) = &account.code {
            if !code.is_empty() {
                account.code_hash = code.hash();
                self.contracts
                    .entry(account.code_hash)
                    .or_insert_with(|| code.clone());
            }
        }
        if account.code_hash.is_zero() {
            account.code_hash = KECCAK_EMPTY;
        }
    }

    /// Insert account info but not override storage
    pub fn insert_account_info(&mut self, address: H160, mut info: AccountInfo) {
        self.insert_contract(&mut info);
        self.accounts.entry(address).or_default().info = info;
    }

    /// insert account storage without overriding account info
    pub fn insert_account_storage(&mut self, address: H160, slot: U256, value: U256) {
        let db = &self.db;
        self.accounts
            .entry(address)
            .or_insert_with(|| DbAccount {
                info: db.basic(address),
                ..Default::default()
            })
            .storage
            .insert(slot, value);
    }

    /// replace account storage without overriding account info
    pub fn replace_account_storage(&mut self, address: H160, storage: Map<U256, U256>) {
        let db = &self.db;
        let mut account = self.accounts.entry(address).or_insert_with(|| DbAccount {
            info: db.basic(address),
            ..Default::default()
        });
        account.account_state = AccountState::EVMStorageCleared;
        account.storage = storage.into_iter().collect();
    }
}

impl<ExtDB: DatabaseRef> DatabaseCommit for CacheDB<ExtDB> {
    fn commit(&mut self, changes: Map<H160, Account>) {
        for (address, mut account) in changes {
            if account.is_destroyed {
                let db_account = self.accounts.entry(address).or_default();
                db_account.storage.clear();
                db_account.account_state = AccountState::EVMStorageCleared;
                db_account.info = AccountInfo::default();
                continue;
            }
            self.insert_contract(&mut account.info);

            let db_account = self.accounts.entry(address).or_default();
            db_account.info = account.info;

            db_account.account_state = if account.storage_cleared {
                db_account.storage.clear();
                AccountState::EVMStorageCleared
            } else {
                AccountState::EVMTouched
            };
            db_account.storage.extend(
                account
                    .storage
                    .into_iter()
                    .map(|(key, value)| (key, value.present_value())),
            );
        }
    }
}

impl<ExtDB: DatabaseRef> Database for CacheDB<ExtDB> {
    fn block_hash(&mut self, number: U256) -> H256 {
        match self.block_hashes.entry(number) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let hash = self.db.block_hash(number);
                entry.insert(hash);
                hash
            }
        }
    }

    fn basic(&mut self, address: H160) -> AccountInfo {
        match self.accounts.entry(address) {
            btree_map::Entry::Occupied(entry) => entry.get().info.clone(),
            btree_map::Entry::Vacant(entry) => {
                let info = self.db.basic(address);
                entry.insert(DbAccount {
                    info: info.clone(),
                    account_state: AccountState::EVMTouched,
                    storage: BTreeMap::new(),
                });
                info
            }
        }
    }

    /// Get the value in an account's storage slot.
    ///
    /// It is assumed that account is already loaded.
    fn storage(&mut self, address: H160, index: U256) -> U256 {
        match self.accounts.entry(address) {
            btree_map::Entry::Occupied(mut acc_entry) => {
                let acc_entry = acc_entry.get_mut();
                match acc_entry.storage.entry(index) {
                    btree_map::Entry::Occupied(entry) => *entry.get(),
                    btree_map::Entry::Vacant(entry) => {
                        if matches!(acc_entry.account_state, AccountState::EVMStorageCleared) {
                            U256::zero()
                        } else {
                            let slot = self.db.storage(address, index);
                            entry.insert(slot);
                            slot
                        }
                    }
                }
            }
            btree_map::Entry::Vacant(acc_entry) => {
                // acc needs to be loaded for us to access slots.
                let info = self.db.basic(address);
                let value = self.db.storage(address, index);
                acc_entry.insert(DbAccount {
                    info,
                    account_state: AccountState::None,
                    storage: BTreeMap::from([(index, value)]),
                });
                value
            }
        }
    }

    fn code_by_hash(&mut self, code_hash: H256) -> Bytecode {
        match self.contracts.entry(code_hash) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                // if you return code bytes when basic fn is called this function is not needed.
                entry.insert(self.db.code_by_hash(code_hash)).clone()
            }
        }
    }
}

impl<ExtDB: DatabaseRef> DatabaseRef for CacheDB<ExtDB> {
    fn block_hash(&self, number: U256) -> H256 {
        match self.block_hashes.get(&number) {
            Some(entry) => *entry,
            None => self.db.block_hash(number),
        }
    }

    fn basic(&self, address: H160) -> AccountInfo {
        match self.accounts.get(&address) {
            Some(acc) => acc.info.clone(),
            None => self.db.basic(address),
        }
    }

    fn storage(&self, address: H160, index: U256) -> U256 {
        match self.accounts.get(&address) {
            Some(acc_entry) => match acc_entry.storage.get(&index) {
                Some(entry) => *entry,
                None => {
                    if matches!(acc_entry.account_state, AccountState::EVMStorageCleared) {
                        U256::zero()
                    } else {
                        self.db.storage(address, index)
                    }
                }
            },
            None => self.db.storage(address, index),
        }
    }

    fn code_by_hash(&self, code_hash: H256) -> Bytecode {
        match self.contracts.get(&code_hash) {
            Some(entry) => entry.clone(),
            None => self.db.code_by_hash(code_hash),
        }
    }
}

/// An empty database that always returns default values when queried.
#[derive(Debug, Default, Clone)]
pub struct EmptyDB();

impl DatabaseRef for EmptyDB {
    /// Get basic account information.
    fn basic(&self, _address: H160) -> AccountInfo {
        AccountInfo::default()
    }
    /// Get account code by its hash
    fn code_by_hash(&self, _code_hash: H256) -> Bytecode {
        Bytecode::new()
    }
    /// Get storage value of address at index.
    fn storage(&self, _address: H160, _index: U256) -> U256 {
        U256::default()
    }

    // History related
    fn block_hash(&self, number: U256) -> H256 {
        let mut buffer: [u8; 4 * 8] = [0; 4 * 8];
        number.to_big_endian(&mut buffer);
        H256::from_slice(&Keccak256::digest(&buffer))
    }
}

/// Custom benchmarking DB that only has account info for the zero address.
///
/// Any other address will return an empty account.
#[derive(Debug, Default, Clone)]
pub struct BenchmarkDB(pub Bytecode, H256);

impl BenchmarkDB {
    pub fn new_bytecode(bytecode: Bytecode) -> Self {
        let hash = bytecode.hash();
        Self(bytecode, hash)
    }
}

impl Database for BenchmarkDB {
    /// Get basic account information.
    fn basic(&mut self, address: H160) -> AccountInfo {
        if address == H160::zero() {
            return AccountInfo {
                nonce: 1,
                balance: U256::from(10000000),
                code: Some(self.0.clone()),
                code_hash: self.1,
            };
        }
        AccountInfo::default()
    }

    /// Get account code by its hash
    fn code_by_hash(&mut self, _code_hash: H256) -> Bytecode {
        Bytecode::default()
    }

    /// Get storage value of address at index.
    fn storage(&mut self, _address: H160, _index: U256) -> U256 {
        U256::default()
    }

    // History related
    fn block_hash(&mut self, _number: U256) -> H256 {
        H256::default()
    }
}

#[cfg(test)]
mod tests {
    use primitive_types::H160;

    use crate::{AccountInfo, Database};

    use super::{CacheDB, EmptyDB};

    #[test]
    pub fn test_insert_account_storage() {
        let account = H160::from_low_u64_be(42);
        let nonce = 42;
        let mut init_state = CacheDB::new(EmptyDB::default());
        init_state.insert_account_info(
            account,
            AccountInfo {
                nonce,
                ..Default::default()
            },
        );

        let (key, value) = (123u64.into(), 456u64.into());
        let mut new_state = CacheDB::new(init_state);
        new_state.insert_account_storage(account, key, value);

        assert_eq!(new_state.basic(account).nonce, nonce);
        assert_eq!(new_state.storage(account, key), value);
    }

    #[test]
    pub fn test_replace_account_storage() {
        let account = H160::from_low_u64_be(42);
        let nonce = 42;
        let mut init_state = CacheDB::new(EmptyDB::default());
        init_state.insert_account_info(
            account,
            AccountInfo {
                nonce,
                ..Default::default()
            },
        );

        let (key0, value0) = (123u64.into(), 456u64.into());
        let (key1, value1) = (789u64.into(), 999u64.into());
        init_state.insert_account_storage(account, key0, value0);

        let mut new_state = CacheDB::new(init_state);
        new_state.replace_account_storage(account, [(key1, value1)].into());

        assert_eq!(new_state.basic(account).nonce, nonce);
        assert_eq!(new_state.storage(account, key0), 0.into());
        assert_eq!(new_state.storage(account, key1), value1);
    }
}
