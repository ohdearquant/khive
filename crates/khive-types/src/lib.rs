//! khive-types — core primitives and substrate data types.
//!
//! `#![no_std]` compatible. Minimal dependencies. No ID generation, no clock
//! access, no panics. Substrate structs (Note, Entity, Event) are merged into
//! this crate — they are the data shape that the rest of the runtime operates on.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

pub mod edge;
pub mod entity;
pub mod entity_type;
pub mod error;
pub mod event;
pub mod hash;
pub mod header;
pub mod id;
pub mod khive_error;
pub mod namespace;
pub mod note;
pub mod pack;
pub mod substrate;
pub mod timestamp;
pub mod vector;

pub use edge::{EdgeCategory, EdgeRelation};
pub use entity::{Entity, EntityKind, Link, PropertyValue};
pub use entity_type::{EntityTypeDef, EntityTypeError, EntityTypeRegistry, ResolvedEntityType};
pub use error::{TypeError, UnknownVariant};
pub use event::{
    AggregateRef, ApplyResult, Event, EventBuilder, EventKind, EventOutcome, EventPayload,
    ProposalAppliedPayload, ProposalDecision, ProposalReviewedPayload, ProposalWithdrawnPayload,
    RerankExecutedPayload,
};
#[cfg(feature = "serde")]
pub use event::{
    EntityDraft, NoteDraft, ProposalChangeset, ProposalCreatedPayload, ProposalEntityPatch,
};
pub use hash::Hash32;
pub use header::Header;
pub use id::{Id128, ParseIdError};
pub use khive_error::{Details, ErrorCode, ErrorDomain, ErrorKind, KhiveError, RetryHint};
pub use namespace::Namespace;
pub use note::{Note, NoteStatus};
// REASON: `VerbDef` is marked `#[deprecated]` in pack.rs but still re-exported
// here for callers that have not yet migrated to `HandlerDef`.
// Remove this allow once all downstream crates are migrated.
#[allow(deprecated)]
pub use pack::VerbDef;
pub use pack::{
    EdgeEndpointRule, EndpointKind, HandlerDef, NoteKindSpec, NoteLifecycleSpec, Pack,
    PackSchemaPlan, ParamDef, VerbCategory, VerbPresentationPolicy, Visibility,
};
pub use substrate::{SubstrateKind, SUBSTRATE_COUNT};
pub use timestamp::Timestamp;
pub use vector::DistanceMetric;
