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
pub mod event;
pub mod header;
pub mod id;
pub mod namespace;
pub mod note;
pub mod substrate;
pub mod timestamp;
pub mod vector;

pub use edge::{EdgeCategory, EdgeRelation, UnknownRelation};
pub use entity::{Entity, EntityKind, Link, PropertyValue};
pub use event::{Event, EventBuilder, EventOutcome};
pub use header::Header;
pub use id::{Id128, ParseIdError};
pub use namespace::Namespace;
pub use note::{Note, NoteKind, NoteStatus};
pub use substrate::{SUBSTRATE_COUNT, SubstrateError, SubstrateKind};
pub use timestamp::Timestamp;
pub use vector::DistanceMetric;
