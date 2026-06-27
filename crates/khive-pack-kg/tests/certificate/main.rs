//! ADR-076 certificate gate — integration test binary for khive-pack-kg.
//!
//! Entry point for `cargo test -p khive-pack-kg --test certificate`.
//!
//! This binary audits the live endpoint-rule contract: it reads the real base
//! and KG `EDGE_RULES` via khive-runtime and khive-pack-kg and flags any two
//! relations that share an identical endpoint signature, rather than checking
//! hand-copied snapshots. The `EdgeRelation::ALL` coverage gate lives in the
//! khive-types certificate test.

/// Live endpoint-signature distinctness audit (ADR-076 §D2 Er eliminator).
mod endpoint_signatures;
