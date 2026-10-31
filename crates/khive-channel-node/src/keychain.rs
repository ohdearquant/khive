//! Login-keychain-backed [`KeyFacility`] for macOS nodes.
//!
//! One generic-password item holds the device's private material: service
//! `org.khive.node.v1`, account = the caller's key reference, payload = 65
//! bytes (version byte `1`, 32 bytes of HPKE input keying material, 32 bytes
//! of Ed25519 seed). The facility keeps only zeroizing copies, never exports
//! key bytes, and implements no `Debug`, `Clone` or serialization.
//!
//! The real backend needs `target_os = "macos"` and the `macos-keychain`
//! feature. The open/create logic is generic over a private backend trait so
//! it is exercised with an in-module fake on every platform.
#![cfg_attr(
    not(all(target_os = "macos", feature = "macos-keychain")),
    allow(dead_code)
)]

use crate::encoding::{CanonicalUuid, Realm};
use crate::envelope::{EnvelopeHeader, SealedEnvelope};
use crate::keys::{
    DevicePublicKeys, EnrolmentBundle, InMemoryKeyFacility, KemPublicKey, KeyFacility,
};
use crate::ProtocolError;
use rand_core::RngCore;
use zeroize::Zeroizing;

/// Keychain item service shared by every node key item.
const SERVICE: &str = "org.khive.node.v1";
const FORMAT_VERSION: u8 = 1;
const MATERIAL_LEN: usize = 64;
const PAYLOAD_LEN: usize = 1 + MATERIAL_LEN;

/// Why a keychain-backed facility could not be opened or created.
#[derive(Debug, thiserror::Error)]
pub enum KeychainError {
    /// The effective user's home directory could not be determined.
    #[error("effective-user home directory is unavailable")]
    HomeUnavailable,
    /// The login keychain file is missing or could not be opened.
    #[error("login keychain is unavailable")]
    LoginKeychainUnavailable,
    /// The key item is missing or the keychain refused the operation.
    #[error("key item is unavailable")]
    ItemUnavailable,
    /// The stored item has the wrong version, length or key content.
    #[error("stored key material is invalid")]
    InvalidMaterial,
    /// The stored keys do not match the expected fingerprint.
    #[error("stored keys do not match the expected fingerprint")]
    PublicKeyMismatch,
    /// Operating-system randomness could not be read.
    #[error("operating-system randomness is unavailable")]
    RandomnessUnavailable,
    /// An item already exists for this key reference; it is never replaced.
    #[error("a key item already exists for this reference")]
    AlreadyExists,
}

/// Storage seam for the single key item. `read` returns `None` for a missing
/// item; `add` must fail with `AlreadyExists` rather than replace.
trait KeychainBackend {
    fn read(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, KeychainError>;
    fn add(&self, service: &str, account: &str, payload: &[u8]) -> Result<(), KeychainError>;
}

/// A key facility whose private material was loaded from the login keychain.
/// It offers private operations only through [`KeyFacility`].
pub struct MacOsKeyFacility {
    inner: InMemoryKeyFacility,
}

impl KeyFacility for MacOsKeyFacility {
    fn public_keys(&self) -> DevicePublicKeys {
        self.inner.public_keys()
    }
    fn sign(&self, input: &[u8]) -> [u8; 64] {
        self.inner.sign(input)
    }
    fn seal(
        &self,
        header: &EnvelopeHeader,
        logical_message_id: CanonicalUuid,
        recipient: &KemPublicKey,
        plaintext: &[u8],
    ) -> Result<SealedEnvelope, ProtocolError> {
        self.inner
            .seal(header, logical_message_id, recipient, plaintext)
    }
    fn open(
        &self,
        header: &EnvelopeHeader,
        logical_message_id: CanonicalUuid,
        pinned_sender: &KemPublicKey,
        envelope: &SealedEnvelope,
    ) -> Result<Vec<u8>, ProtocolError> {
        self.inner
            .open(header, logical_message_id, pinned_sender, envelope)
    }
}

/// Reads the item for `reference` and returns its 64 bytes of private
/// material after checking the version byte and the exact payload length. A
/// missing item is `ItemUnavailable`; nothing is ever created here.
fn read_material<B: KeychainBackend>(
    backend: &B,
    reference: &str,
) -> Result<Zeroizing<[u8; MATERIAL_LEN]>, KeychainError> {
    let payload = backend
        .read(SERVICE, reference)?
        .ok_or(KeychainError::ItemUnavailable)?;
    if payload.len() != PAYLOAD_LEN || payload[0] != FORMAT_VERSION {
        return Err(KeychainError::InvalidMaterial);
    }
    let mut material = Zeroizing::new([0u8; MATERIAL_LEN]);
    material.copy_from_slice(&payload[1..]);
    Ok(material)
}

/// Loads the item once, validates version, length and fingerprint, and keeps
/// only zeroizing copies. A missing item never creates anything.
fn open_with<B: KeychainBackend>(
    backend: &B,
    reference: &str,
    expected_fingerprint: &[u8; 32],
) -> Result<MacOsKeyFacility, KeychainError> {
    let material = read_material(backend, reference)?;
    let inner = InMemoryKeyFacility::from_keychain_material(&material)
        .map_err(|_| KeychainError::InvalidMaterial)?;
    if inner.public_keys().fingerprint().as_bytes() != expected_fingerprint {
        return Err(KeychainError::PublicKeyMismatch);
    }
    Ok(MacOsKeyFacility { inner })
}

/// Adds a fresh item for `reference` and returns only the public enrolment
/// bundle. An existing item is never rotated or overwritten.
fn create_with<B: KeychainBackend, R: RngCore>(
    backend: &B,
    rng: &mut R,
    reference: &str,
    realm: Realm,
) -> Result<EnrolmentBundle, KeychainError> {
    if backend.read(SERVICE, reference)?.is_some() {
        return Err(KeychainError::AlreadyExists);
    }
    let mut payload = Zeroizing::new([0u8; PAYLOAD_LEN]);
    payload[0] = FORMAT_VERSION;
    rng.try_fill_bytes(&mut payload[1..])
        .map_err(|_| KeychainError::RandomnessUnavailable)?;
    let mut material = Zeroizing::new([0u8; MATERIAL_LEN]);
    material.copy_from_slice(&payload[1..]);
    let facility = InMemoryKeyFacility::from_keychain_material(&material)
        .map_err(|_| KeychainError::InvalidMaterial)?;
    let bundle = facility
        .enrolment_bundle(realm)
        .map_err(|_| KeychainError::InvalidMaterial)?;
    backend.add(SERVICE, reference, &payload[..])?;
    Ok(bundle)
}

/// Reads the existing item for `reference` and returns only the public
/// enrolment bundle for `realm`. The version and length are checked exactly as
/// on open. A missing item is `ItemUnavailable`; this never adds, replaces or
/// deletes an item.
fn recover_with<B: KeychainBackend>(
    backend: &B,
    reference: &str,
    realm: Realm,
) -> Result<EnrolmentBundle, KeychainError> {
    let material = read_material(backend, reference)?;
    let facility = InMemoryKeyFacility::from_keychain_material(&material)
        .map_err(|_| KeychainError::InvalidMaterial)?;
    facility
        .enrolment_bundle(realm)
        .map_err(|_| KeychainError::InvalidMaterial)
}

/// Real login-keychain backend: the user's login keychain is opened by explicit
/// path under the effective user's home and no other keychain is searched.
/// Key bytes never appear in a command line.
#[cfg(all(target_os = "macos", feature = "macos-keychain"))]
mod login_keychain {
    use super::{
        create_with, open_with, recover_with, EnrolmentBundle, KeychainBackend, KeychainError,
        MacOsKeyFacility, Realm,
    };
    use rand_core::OsRng;
    use security_framework::os::macos::keychain::SecKeychain;
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use zeroize::Zeroizing;

    /// OSStatus returned when no matching keychain item exists.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
    /// OSStatus returned when adding an item that already exists.
    const ERR_SEC_DUPLICATE_ITEM: i32 = -25299;
    const LOGIN_KEYCHAIN_RELATIVE_PATH: &str = "Library/Keychains/login.keychain-db";

    /// The effective user's home directory from the account database, so a
    /// changed `HOME` environment variable cannot redirect key storage.
    fn effective_home() -> Result<PathBuf, KeychainError> {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        let mut buffer = vec![0u8; 1024];
        loop {
            // SAFETY: passwd is plain data; an all-zero value is a valid blank record.
            let mut record: libc::passwd = unsafe { std::mem::zeroed() };
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: every pointer refers to live local storage and the buffer
            // length passed is the buffer's real length.
            let status = unsafe {
                libc::getpwuid_r(
                    uid,
                    &mut record,
                    buffer.as_mut_ptr().cast::<libc::c_char>(),
                    buffer.len(),
                    &mut found,
                )
            };
            if status == libc::ERANGE && buffer.len() < 65536 {
                let doubled = buffer.len() * 2;
                buffer.resize(doubled, 0);
                continue;
            }
            if status != 0 || found.is_null() || record.pw_dir.is_null() {
                return Err(KeychainError::HomeUnavailable);
            }
            // SAFETY: pw_dir was just filled by getpwuid_r with a NUL-terminated
            // string that lives inside `buffer`, which is still alive.
            let directory = unsafe { CStr::from_ptr(record.pw_dir) };
            let home = PathBuf::from(OsStr::from_bytes(directory.to_bytes()));
            if !home.is_absolute() {
                return Err(KeychainError::HomeUnavailable);
            }
            return Ok(home);
        }
    }

    /// The login keychain, opened once by explicit path.
    struct LoginKeychain {
        keychain: SecKeychain,
    }

    impl LoginKeychain {
        fn open() -> Result<Self, KeychainError> {
            let path = effective_home()?.join(LOGIN_KEYCHAIN_RELATIVE_PATH);
            if !path.is_file() {
                return Err(KeychainError::LoginKeychainUnavailable);
            }
            let keychain =
                SecKeychain::open(&path).map_err(|_| KeychainError::LoginKeychainUnavailable)?;
            Ok(Self { keychain })
        }
    }

    impl KeychainBackend for LoginKeychain {
        fn read(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<Zeroizing<Vec<u8>>>, KeychainError> {
            match self.keychain.find_generic_password(service, account) {
                Ok((password, _item)) => Ok(Some(Zeroizing::new(password.to_vec()))),
                Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
                Err(_) => Err(KeychainError::ItemUnavailable),
            }
        }

        fn add(&self, service: &str, account: &str, payload: &[u8]) -> Result<(), KeychainError> {
            // The add-only call: the generic setters would replace an existing
            // item, which must never happen to a device key.
            match self
                .keychain
                .add_generic_password(service, account, payload)
            {
                Ok(()) => Ok(()),
                Err(error) if error.code() == ERR_SEC_DUPLICATE_ITEM => {
                    Err(KeychainError::AlreadyExists)
                }
                Err(_) => Err(KeychainError::ItemUnavailable),
            }
        }
    }

    impl MacOsKeyFacility {
        /// Opens the facility stored under `reference` in the login keychain.
        /// The stored public keys must match `expected_fingerprint`. A missing
        /// item is an error and nothing is created.
        pub fn open(
            reference: &str,
            expected_fingerprint: &[u8; 32],
        ) -> Result<Self, KeychainError> {
            open_with(&LoginKeychain::open()?, reference, expected_fingerprint)
        }

        /// Creates a fresh key item under `reference` and returns only the
        /// public enrolment bundle. An existing item is never replaced.
        pub fn create(reference: &str, realm: Realm) -> Result<EnrolmentBundle, KeychainError> {
            create_with(&LoginKeychain::open()?, &mut OsRng, reference, realm)
        }

        /// Returns the public enrolment bundle for the item already stored
        /// under `reference`. Nothing is created, replaced or deleted.
        pub fn recover_enrolment_bundle(
            reference: &str,
            realm: Realm,
        ) -> Result<EnrolmentBundle, KeychainError> {
            recover_with(&LoginKeychain::open()?, reference, realm)
        }
    }
}

/// In-memory keychain for tests of crates that consume this facility. It runs
/// the same create, open and recover logic as the login keychain and never
/// touches the operating system.
#[cfg(any(test, feature = "test-keychain"))]
#[path = "keychain_testing.rs"]
pub mod testing;

#[cfg(test)]
#[path = "keychain_tests.rs"]
mod tests;
