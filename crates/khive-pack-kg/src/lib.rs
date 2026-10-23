//! pack-kg — Knowledge Graph verb pack for khive. 31 verbs: entities, notes, edges, queries, proposals, context, resolve, whoami, scan, db_diagnostics, restore, schema, apply_changeset.
//! Stream verbs: stream.append, stream.batch, stream.read, stream.stat, stream.truncate.

mod sql;

pub mod apply_worker;
mod canonical_apply;
mod dispatch;
pub mod entity_type_registry;
mod handler_defs;
pub mod handlers;
mod pack;
pub mod projection_worker;
mod schema;
pub mod vocab;

pub use entity_type_registry::{EntityTypeDef, EntityTypeRegistry, ResolvedType};
pub use khive_types::EntityKind;
pub use pack::KgPack;
pub use vocab::NoteKind;
