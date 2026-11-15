//! Store-set metadata and declaration publication for cooperative maintenance (ADR-049 Amendment 16).
//!
//! These primitives do not open stores or grant admission.

mod store_set;

pub use store_set::StoreSet;

#[cfg(test)]
mod store_set_tests;

mod process;
mod record;
mod record_io;

pub use record::{
    CoordinatorIdentity, Declaration, EpochDraft, ImageIdentity, Phase, PhaseAdvance,
    ReadExpectation, ReadObservation, RecordRefusal, TargetIdentity, MAX_CONFIG_ID_BYTES,
    MAX_PHASE_INTERVAL_MS, MAX_RECORD_BYTES,
};
pub use record_io::{
    read_record, ClaimError, DeclarationWriter, PublicationReceipt, PublishCause, PublishFailure,
    PublishPhase, RecordLocation, RecordSlot, Visibility,
};

#[cfg(test)]
mod record_tests;
