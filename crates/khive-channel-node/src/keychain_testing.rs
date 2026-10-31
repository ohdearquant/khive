//! In-memory stand-in for the login keychain, for tests only.
//!
//! It implements the private storage seam used by the real backend and runs
//! the same `create`, `open` and `recover` logic, so behavior such as the
//! add-only rule, version and length checks, and fingerprint matching is the
//! same as in production. No operating-system keychain is ever opened.

use super::{
    create_with, open_with, recover_with, EnrolmentBundle, KeychainBackend, KeychainError,
    MacOsKeyFacility, Realm,
};
use rand_core::OsRng;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use zeroize::Zeroizing;

type ItemMap = HashMap<(String, String), Vec<u8>>;

#[derive(Default)]
struct Store {
    items: Mutex<ItemMap>,
    fail_reads: AtomicBool,
    fail_adds: AtomicBool,
}

impl Store {
    fn items(&self) -> MutexGuard<'_, ItemMap> {
        self.items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl KeychainBackend for Store {
    fn read(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, KeychainError> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(KeychainError::ItemUnavailable);
        }
        Ok(self
            .items()
            .get(&(service.to_string(), account.to_string()))
            .map(|bytes| Zeroizing::new(bytes.clone())))
    }

    fn add(&self, service: &str, account: &str, payload: &[u8]) -> Result<(), KeychainError> {
        if self.fail_adds.load(Ordering::SeqCst) {
            return Err(KeychainError::ItemUnavailable);
        }
        let key = (service.to_string(), account.to_string());
        let mut items = self.items();
        if items.contains_key(&key) {
            return Err(KeychainError::AlreadyExists);
        }
        items.insert(key, payload.to_vec());
        Ok(())
    }
}

/// An in-memory keychain with the same add-only behavior as the login
/// keychain. Reads or adds can be made to fail to exercise error paths.
#[derive(Default)]
pub struct TestKeychain {
    store: Store,
}

impl TestKeychain {
    /// An empty keychain.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a fresh item under `reference` and returns the public bundle.
    /// An existing item is never replaced.
    pub fn create(&self, reference: &str, realm: Realm) -> Result<EnrolmentBundle, KeychainError> {
        create_with(&self.store, &mut OsRng, reference, realm)
    }

    /// Opens the item under `reference`; its public keys must match
    /// `fingerprint`. Nothing is created.
    pub fn open(
        &self,
        reference: &str,
        fingerprint: &[u8; 32],
    ) -> Result<MacOsKeyFacility, KeychainError> {
        open_with(&self.store, reference, fingerprint)
    }

    /// Returns the public bundle of the item under `reference`. Nothing is
    /// created, replaced or deleted.
    pub fn recover(&self, reference: &str, realm: Realm) -> Result<EnrolmentBundle, KeychainError> {
        recover_with(&self.store, reference, realm)
    }

    /// While `fail` is true every read fails with an unavailable-item error.
    pub fn fail_reads(&self, fail: bool) {
        self.store.fail_reads.store(fail, Ordering::SeqCst);
    }

    /// While `fail` is true every add fails with an unavailable-item error.
    pub fn fail_adds(&self, fail: bool) {
        self.store.fail_adds.store(fail, Ordering::SeqCst);
    }

    /// How many items are stored.
    pub fn item_count(&self) -> usize {
        self.store.items().len()
    }
}
