// FILE SIZE JUSTIFICATION: pack.rs is the load-bearing dispatch core — VerbRegistry,
// VerbRegistryBuilder, PackRuntime, DispatchHook, and their test scaffolding all
// share internal state (packs Vec, gate, event_store) that cannot be cleanly split
// without exposing private fields or duplicating the scaffolding. Inline tests cover
// collision detection and dispatch path that require direct access to VerbRegistry
// internals. Split plan: when the verb surface reaches a stable v1 API, extract
// VerbRegistryBuilder into `pack/builder.rs` and gate/event logic into `pack/dispatch.rs`.
//! Pack runtime trait and verb registry.
//!
//! `PackRuntime` mirrors `Pack`'s const associated items as methods for object safety.
//! Build a [`VerbRegistry`] via `VerbRegistryBuilder::build()`; registration is builder-only.

#[cfg(test)]
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(test)]
use crate::operations::LinkSpec;
#[cfg(test)]
use crate::runtime::NamespaceToken;
#[cfg(test)]
use async_trait::async_trait;
#[cfg(test)]
use khive_gate::{AllowAllGate, GateRef};
use khive_gate::{AuditEvent, GateDecision, GateRequest};
#[cfg(test)]
use khive_storage::EventView;
use khive_storage::{Event, EventStore, SubstrateKind};
use khive_types::{EventKind, EventOutcome, Namespace};
use serde_json::Value;

pub use khive_types::{
    json_type_name, EdgeEndpointRule, EndpointKind, EntityTypeDef, HandlerDef, IdResolutionMode,
    NoteEmbeddingPolicy, NoteEmbeddingPolicySpec, NoteKindSpec, NoteLifecycleSpec,
    PackColumnAddition, PackColumnAffinity, PackSchemaPlan, ParamDef, VerbCategory,
    VerbPresentationPolicy, Visibility, RESERVED_ENVELOPE_ARGS,
};
// Backward-compat re-export.
#[allow(deprecated)]
pub use khive_types::VerbDef;

/// Name of the pack providing the shared CRUD verbs and the general-purpose
/// note kinds those verbs exist to serve.
///
/// Its note kinds are the ones any caller may author freely through `create`
/// and `update`; every other pack's note kinds are records maintained by that
/// pack's own verbs. Used by
/// [`VerbRegistry::pack_owned_note_kinds`].
pub const GENERIC_CRUD_PACK: &str = "kg";

/// Stable advisory code emitted when a successful inspection cannot persist
/// its dispatch audit because the configured audit backend is read-only.
pub const AUDIT_PERSISTENCE_SKIPPED_READ_ONLY: &str = "audit_persistence_skipped_read_only";

const FULL_UUID_IDENTIFIER_HELP: &str = "A complete UUID spelling accepted by the consuming \
    parameter directly names one globally unique record; direct UUID lookup is not a namespace \
    search. Strict identifier responses use canonical lowercase dashed UUIDs.";
const SHORT_PREFIX_IDENTIFIER_HELP: &str = "A short UUID prefix is at least 8 hexadecimal \
    characters without dashes that do not parse as a complete UUID. It is a resolution, not a \
    direct identifier; a 32-character compact UUID is complete input instead. Its lookup scope \
    belongs to the consuming parameter — see `identifier_resolution.resolution_modes` for the \
    exhaustive per-mode rule, and each `uuid`/`array of uuid` parameter's own description for \
    which mode it uses. A prefix can be missing or ambiguous.";
const IDENTIFIER_PARAMETER_HELP: &str = "A parameter that requires a full UUID rejects prefixes \
    and explains the resolution consequence. Its corresponding response field remains a \
    canonical full UUID so the value can be submitted again.";

/// Single-source, per-[`IdResolutionMode`] contract text.
///
/// Every `uuid`/`array of uuid` [`ParamDef`] declares which of these modes its
/// handler actually implements (see [`IdResolutionMode`]'s own doc comment).
/// [`VerbRegistry::describe_verb`] renders the SAME text in two places: once
/// per matching parameter's description, and once in the top-level
/// `identifier_resolution.resolution_modes` map — so the wording can never
/// drift between the two call sites, and a caller reading only the top-level
/// envelope still sees every mode that exists on the wire, not just the ones
/// this particular verb happens to use.
///
/// `None` for [`IdResolutionMode::NotApplicable`]: nothing is appended to a
/// non-identifier parameter's description, and it is never listed in
/// `resolution_modes`.
fn resolution_mode_contract(mode: IdResolutionMode) -> Option<&'static str> {
    match mode {
        IdResolutionMode::NotApplicable => None,
        IdResolutionMode::UnscopedById => Some(
            "ID contract (unscoped by-ID, ADR-007 Rev 6): a full UUID and a short hex prefix \
             (8+ hex chars) both resolve with no namespace filter — the caller already knows \
             the specific record, and authorization is the Gate's seam, not resolution's. A \
             prefix matching nothing or matching more than one record is rejected. Used by \
             get/update/delete/merge/link (link's source_id/target_id resolve through the same \
             unfiltered path as the four record-level by-ID verbs), GTD's lifecycle id \
             parameters, and brain's feedback target_id.",
        ),
        IdResolutionMode::PrefixScopedToPrimary => Some(
            "ID contract (prefix scoped to primary namespace): a full UUID resolves as given, \
             with no namespace check performed by this resolver. A short hex prefix (8+ hex \
             chars) is resolved by searching only the caller's primary namespace, and is \
             rejected if it matches nothing or matches more than one record there.",
        ),
        IdResolutionMode::FullAndPrefixScopedToPrimary => Some(
            "ID contract (full UUID and prefix both scoped to primary namespace): both a full \
             UUID and a short hex prefix (8+ hex chars) are validated against the caller's \
             primary namespace — a record that exists but belongs to a different namespace \
             resolves as not found. A prefix matching more than one record in that namespace \
             is rejected as ambiguous.",
        ),
        IdResolutionMode::FullUuidOnlyScopedToPrimary => Some(
            "ID contract (full UUID only, scoped to primary namespace): only a complete UUID \
             is accepted — a short hex prefix is rejected outright because this field stores \
             an explicit stable reference — and the UUID is validated against the caller's own \
             (primary) namespace; a record that exists in a different namespace resolves as \
             not found.",
        ),
        IdResolutionMode::UnscopedFullUuidOnly => Some(
            "ID contract (full UUID only, unscoped): only a complete UUID is accepted — a \
             short hex prefix is rejected outright — and no namespace check is performed on \
             this parameter itself; any namespace scoping comes from the enclosing operation, \
             not from this identifier.",
        ),
        IdResolutionMode::EdgeOrEventTarget => Some(
            "ID contract (list target by kind): kind=event accepts only a full subject UUID; \
             prefixes and names are rejected without graph resolution. Event rows remain \
             scoped to the authorized event namespace. For kind=edge, a full UUID resolves as \
             given; a unique 8+ hex prefix or entity name resolves in the primary namespace.",
        ),
    }
}

/// Stable wire key for an [`IdResolutionMode`], used as the key under
/// `identifier_resolution.resolution_modes`.
fn resolution_mode_key(mode: IdResolutionMode) -> &'static str {
    match mode {
        IdResolutionMode::NotApplicable => "not_applicable",
        IdResolutionMode::UnscopedById => "unscoped_by_id",
        IdResolutionMode::PrefixScopedToPrimary => "prefix_scoped_to_primary",
        IdResolutionMode::FullAndPrefixScopedToPrimary => "full_and_prefix_scoped_to_primary",
        IdResolutionMode::FullUuidOnlyScopedToPrimary => "full_uuid_only_scoped_to_primary",
        IdResolutionMode::UnscopedFullUuidOnly => "unscoped_full_uuid_only",
        IdResolutionMode::EdgeOrEventTarget => "edge_or_event_target",
    }
}

/// Shared identifier-resolution contract included in every operation help schema.
pub fn identifier_resolution_help() -> Value {
    let modes: serde_json::Map<String, Value> = [
        IdResolutionMode::UnscopedById,
        IdResolutionMode::PrefixScopedToPrimary,
        IdResolutionMode::FullAndPrefixScopedToPrimary,
        IdResolutionMode::FullUuidOnlyScopedToPrimary,
        IdResolutionMode::UnscopedFullUuidOnly,
        IdResolutionMode::EdgeOrEventTarget,
    ]
    .into_iter()
    .map(|mode| {
        (
            resolution_mode_key(mode).to_string(),
            Value::String(
                resolution_mode_contract(mode)
                    .expect("every non-NotApplicable mode has contract text")
                    .to_string(),
            ),
        )
    })
    .collect();

    serde_json::json!({
        "full_uuid": FULL_UUID_IDENTIFIER_HELP,
        "short_prefix": SHORT_PREFIX_IDENTIFIER_HELP,
        "parameter_rule": IDENTIFIER_PARAMETER_HELP,
        "resolution_modes": modes,
    })
}

mod traits;
pub use traits::{
    DispatchHook, KindHook, NoteUpdateEffect, PackByIdResolver, PackRuntime, SchemaPlan,
};

#[cfg(test)]
use crate::error::DispatchError;
use crate::error::{AuditObligationFailure, RuntimeError};
use crate::KhiveRuntime;

mod builder;
pub use builder::{PackMetadataRegistry, VerbRegistryBuilder};

mod catalog;
mod dispatch;
mod registry_access;
mod request_identity;
pub(crate) use request_identity::is_special_relation;
pub use request_identity::{
    InterceptedDispatchResult, PackSchemaCollisionError, RequestIdentity, VerbRegistry,
    VerifiedActor,
};

/// Relations `validate_edge_relation_endpoints`
/// (`crates/khive-runtime/src/operations.rs`) resolves in its own dedicated
/// branch — before the generic pack-rule branch (`pack_rule_allows`) is ever
/// reached. For these three relations the validator additionally accepts
/// any `note -> note` pair unconditionally, regardless of note kind
/// (ADR-002 §"Versioning" and §"Epistemic"), and never consults pack
/// `EDGE_RULES` at all, on either substrate.
pub(crate) const SPECIAL_RELATIONS: &[khive_types::EdgeRelation] = &[
    khive_types::EdgeRelation::Supersedes,
    khive_types::EdgeRelation::Supports,
    khive_types::EdgeRelation::Refutes,
];

mod loading;
pub use loading::{
    ChannelIngestCapability, IngestAuditStore, PackFactory, PackInstall, PackLoadError,
    PackRegistration, PackRegistry,
};

/// Pack names entitled to a [`ChannelIngestCapability`] grant at registration.
pub(crate) const CHANNEL_INGEST_CAPABLE_PACKS: &[&str] = &["comm"];

mod audit;
use audit::{
    append_audit_event_best_effort, build_audit_storage_event, fold_audit_obligation,
    link_audit_success_from_result, masked_audit_event, persist_git_digest_receipt,
    GitDigestReceiptOutcome,
};
pub use audit::{
    audit_admission_refused_obligation_count, audit_admission_refused_obligation_last_at_ms,
    audit_admission_unresolved_obligation_count, audit_admission_unresolved_obligation_last_at_ms,
    resolve_explicit_namespace,
};
pub(crate) use audit::{audit_append_failure_count, audit_obligation_append_failure_count};

// INLINE TEST JUSTIFICATION: tests here exercise VerbRegistry collision detection,
// gate enforcement, and dispatch ordering that depend on direct access to the
// registry's private `packs` Vec and gate field. Moving them to tests/ would
// require pub-exporting registry internals. Broad behavioral dispatch tests
// live in tests/integration.rs.
#[cfg(test)]
#[path = "pack_tests.rs"]
pub(crate) mod tests;

// ---- Inter-pack dependency checking ----

#[cfg(test)]
#[path = "pack/dep_tests.rs"]
mod dep_tests;

// ── Note-update hook sequencing tests ───────────────────────────
//
// These tests exercise the DISPATCHER (`VerbRegistry::prepare_note_update_hook`),
// not any one pack's hook. The probe below overrides `normalize_note_update`
// and `validate_note_update`, which since #2956 are the only two halves a pack
// can implement — there is no sequencing method on the trait — so the only way
// both can run, in order, is through the registry's own sequencing.

#[cfg(test)]
#[path = "pack/note_update_sequencing_tests.rs"]
mod note_update_sequencing_tests;

// ── Dispatch hook tests ─────────────────────────────────────────

#[cfg(test)]
#[path = "pack/hook_tests.rs"]
mod hook_tests;

// ── help=true tests ──────────────────────────────────────────────

#[cfg(test)]
#[path = "pack/help_tests.rs"]
mod help_tests;

#[cfg(test)]
#[path = "gate_argument_contract_tests.rs"]
mod gate_argument_contract_tests;
