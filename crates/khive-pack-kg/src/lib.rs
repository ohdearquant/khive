//! pack-kg — Knowledge Graph verb pack for khive.
//!
//! Provides 16 verbs for managing entities, notes, edges, graph queries, and
//! event-sourced change proposals. First-party pack shipped with the khive binary.

pub mod apply_worker;
mod dispatch;
pub mod entity_type_registry;
mod handler_defs;
pub mod handlers;
mod pack;
pub mod projection_worker;
pub mod vocab;

pub use entity_type_registry::{EntityTypeDef, EntityTypeRegistry, ResolvedType};
pub use khive_types::EntityKind;
pub use pack::KgPack;
pub use vocab::NoteKind;
