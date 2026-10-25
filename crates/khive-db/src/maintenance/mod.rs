//! Metadata primitives for cooperative maintenance (ADR-049 Amendment 16).
//!
//! These types do not open stores, publish declarations, or grant admission.

mod store_set;

pub use store_set::StoreSet;

mod record;
#[cfg(test)]
mod store_set_tests;

pub use record::{
    CoordinatorIdentity, Declaration, ImageIdentity, Phase, RecordRefusal, TargetIdentity,
    MAX_CONFIG_ID_BYTES, MAX_PHASE_INTERVAL_MS, MAX_RECORD_BYTES,
};

#[cfg(test)]
mod record_tests;
