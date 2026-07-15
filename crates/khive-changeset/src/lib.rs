//! KG change-set op-list model and NDJSON-delta codec.
//!
//! See `crates/khive-changeset/docs/api/ndjson-codec.md` for the wire format.

mod changeset;
mod envelope;
mod op;
mod strict;

pub use changeset::{from_ndjson, to_ndjson, ChangeSet, ChangeSetError};
pub use envelope::{Envelope, CURRENT_SCHEMA_VERSION};
pub use op::{
    CreateOp, CreateTarget, DeleteOp, DeletePreimage, EdgePatch, EdgePreimage, EntityCreateFields,
    EntityPatch, EntityPreimage, LinkOp, MergeOp, MergePreimage, NoteCreateFields, NotePatch,
    NotePreimage, Op, UpdateOp, UpdatePatch, UpdatePreimage,
};
