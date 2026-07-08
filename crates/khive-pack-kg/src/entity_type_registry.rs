//! Compatibility re-export of the canonical entity-type registry (#571).
//!
//! The registry, its definitions, and its alias-resolution logic now live in
//! `khive-types::entity_type` so `khive-pack-kg` (KG create validation) and
//! `khive-pack-schedule` (replay validation) resolve against the exact same
//! taxonomy instead of maintaining separate copies. This module keeps the
//! `khive_pack_kg::entity_type_registry::*` import path working for existing
//! callers.

pub use khive_types::{EntityTypeDef, EntityTypeRegistry, ResolvedEntityType as ResolvedType};
