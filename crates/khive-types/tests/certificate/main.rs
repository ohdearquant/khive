//! ADR-076 non-redundancy certificate — integration test binary for khive-types.
//!
//! Entry point for `cargo test -p khive-types --test certificate`.
//!
//! Each relation proposed for admission to the closed edge-relation set must
//! add a module here and supply fixtures for all seven eliminator families
//! defined in `harness.rs`. The harness calls executable check functions and
//! asserts both positive coverage (every eliminator is defeated) and negative
//! controls (deliberately-redundant candidates are rejected).
//!
//! Endpoint-signature audit and EdgeRelation::ALL coverage gate live in the
//! `khive-pack-kg` certificate test (`cargo test -p khive-pack-kg --test certificate`),
//! which can depend on both khive-runtime and khive-pack-kg.

mod harness;

/// Certificate fixtures for the `cites` relation (proposed Tier-1, ADR-076).
mod cites;
