//! Generate object-safe pack metadata accessors from the pack's associated constants.
//!
//! Use [`crate::pack_runtime_metadata!`] inside a `PackRuntime` implementation and
//! [`crate::pack_factory_metadata!`] inside its factory implementation. Dispatch,
//! construction, lifecycle hooks, input schemas and validation rule implementations
//! remain explicit; their behavior cannot be derived from metadata constants.

// Macro expansions use this re-export so consumers need no particular import
// spelling or direct dependency on khive-types.
#[doc(hidden)]
pub use khive_types::Pack;

/// Delegate every const-backed `PackRuntime` metadata accessor to `Self: Pack`.
///
/// Invoke once inside `impl PackRuntime for MyPack` as
/// `khive_runtime::pack_runtime_metadata!();`. It emits `name`, `note_kinds`,
/// `entity_kinds`, `brain_consumer_kinds`, `handlers`, `edge_rules`, `entity_types`,
/// `requires`, `note_kind_specs`, `note_embedding_policies`, `schema_plan` and
/// `schema_column_additions`. Do not also implement those methods manually.
///
/// A missing `Pack::SCHEMA_PLAN` becomes `SchemaPlan::empty()`. Validation rule
/// values remain explicit because `Pack::VALIDATION_RULES` contains only ids.
#[macro_export]
macro_rules! pack_runtime_metadata {
    () => {
        fn name(&self) -> &str {
            <Self as $crate::pack_metadata::Pack>::NAME
        }

        fn note_kinds(&self) -> &'static [&'static str] {
            <Self as $crate::pack_metadata::Pack>::NOTE_KINDS
        }

        fn entity_kinds(&self) -> &'static [&'static str] {
            <Self as $crate::pack_metadata::Pack>::ENTITY_KINDS
        }

        fn brain_consumer_kinds(&self) -> &'static [&'static str] {
            <Self as $crate::pack_metadata::Pack>::BRAIN_CONSUMER_KINDS
        }

        fn handlers(&self) -> &'static [$crate::pack::HandlerDef] {
            <Self as $crate::pack_metadata::Pack>::HANDLERS
        }

        fn edge_rules(&self) -> &'static [$crate::pack::EdgeEndpointRule] {
            <Self as $crate::pack_metadata::Pack>::EDGE_RULES
        }

        fn entity_types(&self) -> &'static [$crate::pack::EntityTypeDef] {
            <Self as $crate::pack_metadata::Pack>::ENTITY_TYPES
        }

        fn requires(&self) -> &'static [&'static str] {
            <Self as $crate::pack_metadata::Pack>::REQUIRES
        }

        fn note_kind_specs(&self) -> &'static [$crate::pack::NoteKindSpec] {
            <Self as $crate::pack_metadata::Pack>::NOTE_KIND_SPECS
        }

        fn note_embedding_policies(&self) -> &'static [$crate::pack::NoteEmbeddingPolicySpec] {
            <Self as $crate::pack_metadata::Pack>::NOTE_EMBEDDING_POLICIES
        }

        fn schema_column_additions(&self) -> &'static [$crate::pack::PackColumnAddition] {
            <Self as $crate::pack_metadata::Pack>::SCHEMA_COLUMN_ADDITIONS
        }

        fn schema_plan(&self) -> $crate::SchemaPlan {
            match <Self as $crate::pack_metadata::Pack>::SCHEMA_PLAN {
                ::core::option::Option::Some(plan) => $crate::SchemaPlan {
                    pack: plan.pack,
                    statements: plan.statements,
                },
                ::core::option::Option::None => $crate::SchemaPlan::empty(),
            }
        }
    };
}

/// Delegate a factory's name and dependencies to the pack type it constructs.
///
/// Invoke inside `impl PackFactory for MyFactory` as
/// `khive_runtime::pack_factory_metadata!(MyPack);`. Construction, resolver,
/// installation and intentionally-verbless behavior remain explicit.
#[macro_export]
macro_rules! pack_factory_metadata {
    ($pack:ty) => {
        fn name(&self) -> &'static str {
            <$pack as $crate::pack_metadata::Pack>::NAME
        }

        fn requires(&self) -> &'static [&'static str] {
            <$pack as $crate::pack_metadata::Pack>::REQUIRES
        }
    };
}
