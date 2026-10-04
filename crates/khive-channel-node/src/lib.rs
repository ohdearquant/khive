//! Node wire protocol v1 encodings, cryptography, and HTTPS calls.
//! Contact confirmation, key persistence, and message commits belong to callers.

pub mod client;
pub mod pins;
pub mod response;
pub mod source;
pub mod submit;

pub mod encoding;
pub mod envelope;
pub mod keys;
pub mod plaintext;
pub mod receipt;
pub mod request;
pub mod timestamp;
pub mod wire;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("noncanonical or invalid encoding")]
    InvalidEncoding,
    #[error("integer outside protocol bounds")]
    InvalidInteger,
    #[error("unsupported protocol version")]
    UnsupportedVersion,
    #[error("invalid public key")]
    InvalidKey,
    #[error("invalid enrolment proof")]
    InvalidProof,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("envelope exceeds protocol size bounds")]
    EnvelopeTooLarge,
    #[error("HPKE encapsulation failed")]
    Encryption,
    #[error("HPKE envelope did not authenticate")]
    Decryption,
    #[error("OS randomness unavailable")]
    Randomness,
    #[error("clock is outside protocol bounds")]
    Clock,
}

#[cfg(test)]
mod vectors;

#[cfg(test)]
mod client_test_server;
#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod docs_r3_tests;
#[cfg(test)]
mod plaintext_r3_tests;
#[cfg(test)]
mod timestamp_r3_tests;
#[cfg(test)]
mod wire_r3_tests;
