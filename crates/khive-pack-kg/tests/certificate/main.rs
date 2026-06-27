//! ADR-076 certificate gate — integration test binary for khive-pack-kg.
//!
//! Entry point for `cargo test -p khive-pack-kg --test certificate`.
//!
//! This binary audits the live endpoint-rule contract and enforces that every
//! relation in `EdgeRelation::ALL` has either passed the ADR-076 certificate or
//! holds an explicit system-role exemption. It depends on both khive-runtime and
//! khive-pack-kg, which allows it to read the real rules rather than hand-copied
//! snapshots.

/// Live endpoint-signature distinctness audit (ADR-076 §D2 Er eliminator).
mod endpoint_signatures;
