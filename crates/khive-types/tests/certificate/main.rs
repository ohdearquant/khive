//! ADR-076 non-redundancy certificate — integration test binary.
//!
//! Entry point for `cargo test -p khive-types --test certificate`.
//!
//! Each relation that proposes admission to the closed edge-relation set must
//! add a module here and supply fixtures for all seven eliminator families
//! defined in `harness.rs`. The harness asserts coverage and divergence;
//! a missing eliminator or a non-diverging fixture is a compile-time or
//! runtime failure respectively.

mod harness;

/// Certificate fixtures for the `cites` relation (proposed Tier-1, ADR-076).
mod cites;

/// Endpoint-signature distinguishability audit for the closed relation set.
mod endpoint_signatures;
