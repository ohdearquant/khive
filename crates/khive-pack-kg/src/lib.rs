//! pack-kg — Knowledge Graph verb pack for khive. 28 verbs: entities, notes, edges, queries, proposals, context, resolve, whoami, scan, db_diagnostics, restore, schema.

mod sql;

pub mod apply_worker;
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
