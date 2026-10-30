use super::*;
use rand_core::OsRng;
use std::cell::RefCell;
use std::collections::HashMap;

/// In-memory stand-in for the login keychain. `add` replaces silently, like
/// the generic create-or-update setters, so only `create_with`'s own
/// existing-item check keeps stored bytes intact.
#[derive(Default)]
struct FakeBackend {
    items: RefCell<HashMap<(String, String), Vec<u8>>>,
}

impl FakeBackend {
    fn put(&self, account: &str, payload: Vec<u8>) {
        self.items
            .borrow_mut()
            .insert((SERVICE.to_string(), account.to_string()), payload);
    }
    fn stored(&self, account: &str) -> Option<Vec<u8>> {
        self.items
            .borrow()
            .get(&(SERVICE.to_string(), account.to_string()))
            .cloned()
    }
}

impl KeychainBackend for FakeBackend {
    fn read(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, KeychainError> {
        Ok(self
            .items
            .borrow()
            .get(&(service.to_string(), account.to_string()))
            .map(|bytes| Zeroizing::new(bytes.clone())))
    }
    fn add(&self, service: &str, account: &str, payload: &[u8]) -> Result<(), KeychainError> {
        self.items
            .borrow_mut()
            .insert((service.to_string(), account.to_string()), payload.to_vec());
        Ok(())
    }
}

/// A random source that always fails, standing in for an unreadable OS source.
struct FailingRng;

impl RngCore for FailingRng {
    fn next_u32(&mut self) -> u32 {
        0
    }
    fn next_u64(&mut self) -> u64 {
        0
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        dest.fill(0);
    }
    fn try_fill_bytes(&mut self, _dest: &mut [u8]) -> Result<(), rand_core::Error> {
        Err(rand_core::Error::from(
            std::num::NonZeroU32::new(1).unwrap(),
        ))
    }
}

#[test]
fn unreadable_randomness_is_reported_and_stores_nothing() {
    let backend = FakeBackend::default();
    let err = create_with(&backend, &mut FailingRng, "node-a", realm()).err();
    assert!(matches!(err, Some(KeychainError::RandomnessUnavailable)));
    assert!(backend.items.borrow().is_empty());
}

fn realm() -> Realm {
    Realm::parse("relay.example").unwrap()
}

fn fingerprint_of(bundle: &EnrolmentBundle) -> [u8; 32] {
    *bundle.verify().unwrap().fingerprint().as_bytes()
}

fn created(backend: &FakeBackend, account: &str) -> EnrolmentBundle {
    create_with(backend, &mut OsRng, account, realm()).unwrap()
}

#[test]
fn create_then_open_round_trips_with_matching_fingerprint() {
    let backend = FakeBackend::default();
    let bundle = created(&backend, "node-a");
    let payload = backend.stored("node-a").unwrap();
    assert_eq!(payload.len(), 65);
    assert_eq!(payload[0], 1);
    let Ok(facility) = open_with(&backend, "node-a", &fingerprint_of(&bundle)) else {
        panic!("matching fingerprint must open");
    };
    assert_eq!(facility.enrolment_bundle(realm()).unwrap(), bundle);
}

#[test]
fn wrong_version_byte_is_invalid_material() {
    let backend = FakeBackend::default();
    let bundle = created(&backend, "node-a");
    let mut payload = backend.stored("node-a").unwrap();
    payload[0] = 2;
    backend.put("node-a", payload);
    let err = open_with(&backend, "node-a", &fingerprint_of(&bundle)).err();
    assert!(matches!(err, Some(KeychainError::InvalidMaterial)));
}

#[test]
fn sixty_four_byte_item_is_invalid_material() {
    let backend = FakeBackend::default();
    let bundle = created(&backend, "node-a");
    let mut payload = backend.stored("node-a").unwrap();
    payload.pop();
    assert_eq!(payload.len(), 64);
    backend.put("node-a", payload);
    let err = open_with(&backend, "node-a", &fingerprint_of(&bundle)).err();
    assert!(matches!(err, Some(KeychainError::InvalidMaterial)));
}

#[test]
fn sixty_six_byte_item_is_invalid_material() {
    let backend = FakeBackend::default();
    let bundle = created(&backend, "node-a");
    let mut payload = backend.stored("node-a").unwrap();
    payload.push(0);
    assert_eq!(payload.len(), 66);
    backend.put("node-a", payload);
    let err = open_with(&backend, "node-a", &fingerprint_of(&bundle)).err();
    assert!(matches!(err, Some(KeychainError::InvalidMaterial)));
}

#[test]
fn fingerprint_mismatch_is_public_key_mismatch() {
    let backend = FakeBackend::default();
    let bundle = created(&backend, "node-a");
    let mut wrong = fingerprint_of(&bundle);
    wrong[0] ^= 0xff;
    let err = open_with(&backend, "node-a", &wrong).err();
    assert!(matches!(err, Some(KeychainError::PublicKeyMismatch)));
}

#[test]
fn create_on_existing_account_is_already_exists_and_leaves_bytes_unchanged() {
    let backend = FakeBackend::default();
    created(&backend, "node-a");
    let before = backend.stored("node-a").unwrap();
    let err = create_with(&backend, &mut OsRng, "node-a", realm()).err();
    assert!(matches!(err, Some(KeychainError::AlreadyExists)));
    assert_eq!(backend.stored("node-a").unwrap(), before);
}

#[test]
fn missing_item_is_item_unavailable_and_creates_nothing() {
    let backend = FakeBackend::default();
    let err = open_with(&backend, "node-a", &[0u8; 32]).err();
    assert!(matches!(err, Some(KeychainError::ItemUnavailable)));
    assert!(backend.items.borrow().is_empty());
}

#[test]
fn accounts_are_independent() {
    let backend = FakeBackend::default();
    let first = created(&backend, "node-a");
    let second = created(&backend, "node-b");
    assert_ne!(fingerprint_of(&first), fingerprint_of(&second));
    let err = open_with(&backend, "node-b", &fingerprint_of(&first)).err();
    assert!(matches!(err, Some(KeychainError::PublicKeyMismatch)));
}

/// Compile-time proof that a type implements none of the named traits: the
/// probe call is ambiguous (a compile error) if the trait is implemented.
macro_rules! assert_not_impl {
    ($module:ident, $bound:path) => {
        mod $module {
            use crate::keychain::MacOsKeyFacility;
            trait AmbiguousIfImpl<A> {
                fn probe() {}
            }
            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
            impl<T: ?Sized + $bound> AmbiguousIfImpl<u8> for T {}
            #[test]
            fn facility_does_not_implement_trait() {
                let _ = <MacOsKeyFacility as AmbiguousIfImpl<_>>::probe;
            }
        }
    };
}

assert_not_impl!(no_debug, ::core::fmt::Debug);
assert_not_impl!(no_clone, ::core::clone::Clone);
assert_not_impl!(no_serialize, ::serde::Serialize);
