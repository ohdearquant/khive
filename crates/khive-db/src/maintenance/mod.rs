//! Metadata primitives for cooperative maintenance (ADR-049 Amendment 16).
//!
//! These types do not open stores, publish declarations, or grant admission.

mod store_set;

pub use store_set::StoreSet;

#[cfg(test)]
mod store_set_tests;
