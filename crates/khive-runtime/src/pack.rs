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

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::operations::{LinkSpec, Resolved};
use crate::runtime::NamespaceToken;
#[cfg(test)]
use async_trait::async_trait;
#[cfg(test)]
use khive_gate::{AllowAllGate, GateRef};
use khive_gate::{AuditEvent, GateDecision, GateRequest};
use khive_storage::{Event, EventStore, EventView, SubstrateKind};
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

use crate::validation::ValidationRule;

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

use crate::error::{AuditObligationFailure, DispatchError, RuntimeError};
use crate::KhiveRuntime;

mod builder;
pub use builder::{PackMetadataRegistry, VerbRegistryBuilder};

mod request_identity;
pub(crate) use request_identity::is_special_relation;
use request_identity::{edge_endpoint_table, extract_table_names};
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

impl VerbRegistry {
    /// Select the owning pack's backend for a note-kind KG read. The caller
    /// keeps its already-authorized token; this only selects storage.
    pub fn kg_note_read_runtime_for_kind<'a>(
        &'a self,
        runtime: &'a KhiveRuntime,
        kind: &str,
    ) -> &'a KhiveRuntime {
        let Some(resolver) = &self.kg_read_resolver else {
            return runtime;
        };
        let Some(owner) = self
            .packs
            .iter()
            .find(|pack| pack.note_kinds().contains(&kind))
        else {
            return runtime;
        };
        resolver.runtime_for_pack(owner.name())
    }

    /// Resolve a KG entity/note handle across the configured backend inventory.
    ///
    /// The caller must supply its dispatch-authorized token. By-ID reads do not
    /// filter the stored namespace (ADR-007); no new token is minted here. With
    /// ordinary single-runtime registration, retain the supplied runtime's
    /// existing behavior. This does not route mutations or pack-private records.
    pub async fn resolve_kg_read_by_id(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        id: uuid::Uuid,
        include_deleted: bool,
    ) -> Result<Option<crate::Resolved>, RuntimeError> {
        match &self.kg_read_resolver {
            Some(resolver) => resolver.by_id(token, id, include_deleted).await,
            None if include_deleted => runtime.resolve_by_id_including_deleted(token, id).await,
            None => runtime.resolve_by_id(token, id).await,
        }
    }

    /// Find the unique configured backend holding an entity for deletion.
    /// Includes tombstones so soft deletion cannot hide a duplicate owner.
    /// The dispatch-authorized token is preserved; lookup is namespace-agnostic.
    pub async fn resolve_entity_delete_runtime(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        id: uuid::Uuid,
    ) -> Result<Option<KhiveRuntime>, RuntimeError> {
        match &self.kg_read_resolver {
            Some(resolver) => resolver.entity_runtime(token, id).await,
            None => {
                let store = runtime.entities(token)?;
                let entity = store.get_entity_including_deleted(id).await?;
                Ok(entity.map(|_| runtime.clone()))
            }
        }
    }

    /// Clean main-backend attachments after no live or tombstoned owner remains.
    /// A live or tombstoned entity on any configured backend keeps its roots.
    pub async fn cleanup_deleted_entity_attachments(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        id: uuid::Uuid,
    ) -> Result<bool, RuntimeError> {
        if self
            .resolve_entity_delete_runtime(runtime, token, id)
            .await?
            .is_some()
        {
            return Ok(false);
        }
        runtime.delete_entity_attachments_on_core(id).await
    }

    /// Recheck a merged-entity read against the kept id before returning it.
    /// The submitted argument shape is the verb's ordinary shape with the
    /// effective id substituted. The dispatch's original check remains its
    /// own audit row; this consultation records the effective target as a
    /// second row without changing the public GateRequest schema.
    pub async fn authorize_effective_kg_read(
        &self,
        token: &NamespaceToken,
        verb: &str,
        mut effective_args: Value,
        effective_id: uuid::Uuid,
    ) -> Result<(), RuntimeError> {
        if let Some(namespace) = token.gate_explicit_namespace() {
            effective_args["namespace"] = Value::String(namespace.to_owned());
        }
        let gate_req = GateRequest::new(
            token.actor().clone(),
            token.gate_namespace().clone(),
            verb,
            effective_args,
        );
        let decision = khive_gate::check_with_mailbox_policy(self.gate.as_ref(), &gate_req);
        match decision {
            Ok(decision) => {
                let audit = masked_audit_event(&gate_req, &decision, self.gate.impl_name());
                tracing::info!(
                    audit_event = %serde_json::to_string(&audit)
                        .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
                    effective_target_id = %effective_id,
                    "gate.check"
                );
                let denied = matches!(&decision, GateDecision::Deny { .. });
                let receipt = if let Some(store) = &self.event_store {
                    let event = build_audit_storage_event(
                        &gate_req,
                        &audit,
                        if denied {
                            EventOutcome::Denied
                        } else {
                            EventOutcome::Success
                        },
                        Some(crate::cost_unit::base_resource_payload(token.request_id())),
                    )
                    .with_target(effective_id);
                    if denied {
                        self.append_gate_denied_row(store, event, verb).await
                    } else {
                        let outcome = append_audit_event_best_effort(
                            self.audit_batch.as_ref(),
                            store,
                            event,
                            verb,
                            crate::audit_batch::AuditProducer::EffectiveTargetCheck,
                            false,
                        )
                        .await;
                        fold_audit_obligation(Ok(()), outcome, |_| Value::Null)?;
                        crate::error::DenialReceipt::no_store()
                    }
                } else {
                    crate::error::DenialReceipt::no_store()
                };
                match decision {
                    GateDecision::Allow { .. } => Ok(()),
                    GateDecision::Deny { reason } => Err(RuntimeError::PermissionDenied {
                        verb: verb.to_string(),
                        reason,
                        receipt: Box::new(receipt),
                    }),
                }
            }
            Err(error) => Err(self
                .gate_unavailable_error(&gate_req, &error, token.request_id(), Some(effective_id))
                .await),
        }
    }

    /// Resolve a prefix across the same inventory, rejecting distinct UUIDs.
    ///
    /// Retains the local prefix scanner's entity/note/event/edge collision domain,
    /// including sidecar events. The returned UUID is not a substrate assertion:
    /// consumers must still fetch/type-check it. All backend failures propagate.
    pub async fn resolve_kg_read_prefix(
        &self,
        runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        prefix: &str,
        include_deleted: bool,
    ) -> Result<Option<uuid::Uuid>, RuntimeError> {
        match &self.kg_read_resolver {
            Some(resolver) => resolver.prefix(prefix, include_deleted).await,
            None if include_deleted => {
                runtime
                    .resolve_prefix_unfiltered_including_deleted(prefix)
                    .await
            }
            None => runtime.resolve_prefix_unfiltered(prefix).await,
        }
    }

    /// This registry's construction-baked default namespace.
    ///
    /// Used as the fallback when a request carries no [`RequestIdentity`]
    /// override (ADR-096 Fork 1) and by transports that need to advertise
    /// their own resolved identity when forwarding to a warm daemon.
    pub fn default_namespace(&self) -> &str {
        &self.default_namespace
    }

    /// This registry's construction-baked actor identity label, if configured
    /// (ADR-057). `None` means dispatch mints `ActorRef::anonymous()` absent a
    /// per-request [`RequestIdentity`] override (ADR-096 Fork 1).
    pub fn actor_id(&self) -> Option<&str> {
        self.actor_id.as_deref()
    }

    /// This registry's construction-baked extra read-visibility namespaces
    /// (ADR-007 Rev 4 Rule 3b), used absent a per-request [`RequestIdentity`]
    /// override (ADR-096 Fork 1).
    pub fn visible_namespaces(&self) -> &[Namespace] {
        &self.visible_namespaces
    }

    /// This registry's configured audit `EventStore`, if any (ADR-094).
    ///
    /// Lets background tasks that hold a `VerbRegistry` but do not go through
    /// `dispatch` (e.g. the email channel poll loop) append best-effort
    /// lifecycle events to the same sink gate-check audit rows use, without
    /// threading a second `Option<Arc<dyn EventStore>>` field through every
    /// caller. `None` means either the historical tracing-only default or an
    /// intentionally read-only audit backend; callers that need to distinguish
    /// those cases use [`Self::audit_persistence_advisory`].
    pub fn event_store(&self) -> Option<Arc<dyn EventStore>> {
        self.event_store.clone()
    }

    /// Process-lifetime audit-batch health counters for this registry's
    /// ADR-133 seam, if one is configured. `None` exactly when
    /// [`Self::event_store`] is `None` — the same condition under which no
    /// batch exists to report on. The `db_diagnostics` verb feeds this into
    /// `KhiveRuntime::db_diagnostics_with_audit_metrics` so an operator can
    /// see flush failures and pure-observability degradation instead of the
    /// permanently-unavailable placeholder a bare `KhiveRuntime` reports.
    ///
    /// `admission_refused_obligations` and `admission_unresolved_obligations`
    /// are sourced separately from [`audit_admission_refused_obligation_count`]
    /// and [`audit_admission_unresolved_obligation_count`] rather than from
    /// `batch.health_metrics()`: they count a decision made in
    /// `append_audit_event_best_effort` (ADR-103 Amendment 3 / ADR-133
    /// Amendment 1), not a property of the batch itself, so they are
    /// process-wide like the rest of this struct's fields rather than
    /// per-`AuditBatch`.
    pub fn audit_batch_metrics(&self) -> Option<khive_db::diagnostics::RuntimeAuditBatchMetrics> {
        self.audit_batch.as_ref().map(|batch| {
            let m = batch.health_metrics();
            khive_db::diagnostics::RuntimeAuditBatchMetrics {
                flush_failures: m.flush_failures,
                degraded_rows: m.degraded_rows,
                degraded: m.degraded,
                admission_refused_obligations: audit_admission_refused_obligation_count(),
                admission_refused_obligations_last_at_ms:
                    audit_admission_refused_obligation_last_at_ms(),
                admission_unresolved_obligations: audit_admission_unresolved_obligation_count(),
                admission_unresolved_obligations_last_at_ms:
                    audit_admission_unresolved_obligation_last_at_ms(),
            }
        })
    }

    /// Test/diagnostic-only accessor for the underlying ADR-133 audit-batch
    /// seam. `None` when no `EventStore` was configured (the batch is lazily
    /// constructed from one). Exposed so admission-pressure mechanism tests
    /// can saturate and drain the SAME instance a real dispatch uses
    /// (#2117, #2147, #2208, #2217) instead of testing a
    /// look-alike.
    pub fn audit_batch_handle(&self) -> Option<Arc<crate::audit_batch::AuditBatch>> {
        self.audit_batch.clone()
    }

    /// Stop admitting new audit rows and wait for every already-accepted row
    /// to reach a terminal state (ADR-133).
    ///
    /// A no-op returning `Ok(())` when no `EventStore` — and therefore no
    /// audit-batch seam — is configured. Callers that own this registry's
    /// shutdown sequence should call this before tearing down the writer or
    /// database so no accepted audit row is silently dropped mid-flight.
    pub async fn shutdown_audit_batch(
        &self,
    ) -> Result<(), crate::audit_batch::AuditTerminalReason> {
        use crate::audit_batch::AuditBatchControl;
        match &self.audit_batch {
            Some(audit_batch) => audit_batch.close_and_drain().await,
            None => Ok(()),
        }
    }

    /// Advisory for a dispatch whose configured audit sink is read-only.
    ///
    /// The MCP transport places this beside successful per-operation results;
    /// `None` means audit persistence is configured normally or was never
    /// configured at all.
    pub fn audit_persistence_advisory(&self) -> Option<Value> {
        self.audit_store_read_only.then(|| {
            serde_json::json!({
                "code": AUDIT_PERSISTENCE_SKIPPED_READ_ONLY,
                "severity": "warning",
                "component": "audit_event_store",
                "reason": "read_only_backend",
                "message": "operation completed, but its dispatch audit event was not persisted because the audit backend is read-only",
            })
        })
    }

    /// Explicit, fail-closed opt-in for admission-pressure audit degradation
    /// (#2147/#2217). `VerbCategory::Assertive` alone is NOT a
    /// sound proxy for "safe to drop this dispatch's own audit row under
    /// audit-lane admission pressure": several Assertive handlers have
    /// their own durable or accounting-bearing side effects. The reviewed
    /// exclusions are:
    /// - `memory.recall` dispatches `brain.record_serve` as a background
    ///   write; degrading `memory.recall`'s row raises the risk that a
    ///   serve goes unaccounted for if the ledger dispatch itself later
    ///   also races admission pressure.
    /// - `db_diagnostics` may backfill WAL frames via a PASSIVE checkpoint
    ///   probe — physical I/O, not a pure in-memory read.
    /// - `knowledge.search`, `knowledge.suggest`, and auto
    ///   `knowledge.compose` may start persistent ANN consumer/checkpoint
    ///   maintenance from their nominal read path.
    /// - `git.checkout`, `git.diff` and `git.reconcile` persist a durable
    ///   receipt on every dispatch (checkout and diff also write a manifest
    ///   or diff blob), so their accounting row is not droppable.
    ///
    /// What membership here means, precisely: the verb performs no domain
    /// mutation, so its OWN per-dispatch audit/accounting row may be dropped
    /// under transient admission pressure without the caller losing a
    /// meaningful result (ADR-103 Amendment 3, ADR-133 Amendment 1). It does
    /// NOT mean the handler is free of every event-plane write: `search`
    /// still fires its own best-effort `SearchExecuted` telemetry, and
    /// `context` still records a one-time `ConfigLocked` event, both on
    /// independent code paths this mechanism never touches — those events
    /// commit or fail on their own terms, unaffected by whether this
    /// dispatch's own audit row degrades.
    ///
    /// Every entry here MUST be declared `VerbCategory::Assertive` in its
    /// named pack's live vocabulary. The
    /// `admission_degrade_safe_assertive_census_matches_live_pack_sources`
    /// test below scans every pack that currently declares public Assertive
    /// handlers and requires every such handler to be classified exactly
    /// once as safe or as a known incidental writer. A new Assertive verb
    /// therefore fails closed both at runtime and in the source census until
    /// it receives an explicit side-effect review.
    ///
    /// Entries are `(owning pack name, verb)` pairs, not bare verb names:
    /// [`Self::admission_degrade_safe`] requires the handler actually
    /// resolved for `verb` to belong to the exact pack named here. A verb
    /// name alone is not a sound key — any pack registered through the same
    /// [`PackRegistry`]/[`VerbRegistryBuilder`] path can declare a handler
    /// under any name it likes, including one that collides with a name on
    /// this list, and unique-verb-name validation only rejects that
    /// collision when the real owning pack is *also* loaded. A deployment
    /// that omits the real pack (or loads a third-party pack instead) would
    /// let a same-named write-performing handler inherit degrade-safety it
    /// never earned. Binding to the pack closes that gap.
    const ADMISSION_DEGRADE_SAFE_VERBS: &'static [(&'static str, &'static str)] = &[
        // agent
        ("agent", "agent.observe"),
        // exec (reads of the blob store, the run receipt and event tables, or
        // the resolved configuration; the writers are exec.tree and
        // exec.tree_put, Declarations, and exec.run, a Directive)
        ("exec", "exec.tree_get"),
        ("exec", "exec.tree_diff"),
        ("exec", "exec.receipt"),
        ("exec", "exec.runs"),
        ("exec", "exec.events"),
        ("exec", "exec.identity"),
        // git (receipt list, allowlist, working-tree and history reads;
        // checkout, diff and reconcile persist receipts and are excluded)
        ("git", "git.receipts"),
        ("git", "git.gates"),
        ("git", "git.status"),
        ("git", "git.log"),
        // Canonical get project check plus bounded cursor SELECT; no domain writes.
        ("git", "git.ingest_cursor"),
        // blob
        ("blob", "blob.get"),
        ("blob", "blob.stat"),
        // brain
        ("brain", "brain.event_counts"),
        ("brain", "brain.profiles"),
        ("brain", "brain.profile"),
        ("brain", "brain.resolve"),
        ("brain", "brain.bindings"),
        // comm
        ("comm", "comm.delivered"),
        ("comm", "comm.transport_status"),
        ("comm", "comm.inbox"),
        ("comm", "comm.unread"),
        ("comm", "comm.thread"),
        ("comm", "comm.health"),
        ("comm", "comm.probe"),
        // gtd
        ("gtd", "gtd.census"),
        ("gtd", "gtd.next"),
        ("gtd", "gtd.tasks"),
        // kg
        ("kg", "get"),
        ("kg", "list"),
        ("kg", "stats"),
        ("kg", "search"),
        ("kg", "neighbors"),
        ("kg", "traverse"),
        ("kg", "context"),
        ("kg", "query"),
        ("kg", "resolve"),
        ("kg", "whoami"),
        // scan runs the secret gate over caller-supplied text in process: no
        // store read, no store write, no event.
        ("kg", "scan"),
        ("kg", "verbs"),
        ("kg", "stream.read"),
        ("kg", "stream.stat"),
        // knowledge (ANN-maintaining search/suggest/compose are excluded)
        ("knowledge", "knowledge.get"),
        ("knowledge", "knowledge.list"),
        ("knowledge", "knowledge.stats"),
        ("knowledge", "knowledge.fold"),
        ("knowledge", "knowledge.topic"),
        // moodboard
        ("moodboard", "moodboard.model"),
        ("moodboard", "moodboard.search"),
        ("moodboard", "moodboard.preference"),
        // schedule
        ("schedule", "schedule.agenda"),
        // session
        ("session", "session.list"),
        ("session", "session.resume"),
        ("session", "session.export"),
        ("session", "session.search"),
        // Fixed SQL reads and file metadata only; no domain or maintenance write.
        ("session", "session.stats"),
        // tool (registry, grant and policy reads; tool.suggest runs the same
        // hybrid search as the kg search and context verbs above)
        ("tool", "tool.suggest"),
        ("tool", "tool.describe"),
        ("tool", "tool.list"),
        ("tool", "tool.check"),
        ("tool", "tool.requests"),
        ("tool", "tool.policies"),
    ];

    /// Sorted copy of [`Self::ADMISSION_DEGRADE_SAFE_VERBS`], built once, so
    /// [`VerbRegistryBuilder::build`] can decide each trusted handler's
    /// eligibility with a binary search instead of a linear scan over every
    /// entry. Consulted exactly once per registry, at `build()` time — see
    /// [`Self::admission_degrade_safe`] for why no per-dispatch scan exists
    /// anymore. The source list above stays grouped by pack (with a `//
    /// <pack>` comment per group) for human review; this is a derived,
    /// lookup-shaped view of the same data, not a second source of truth.
    fn admission_degrade_safe_sorted() -> &'static [(&'static str, &'static str)] {
        static SORTED: std::sync::LazyLock<Vec<(&'static str, &'static str)>> =
            std::sync::LazyLock::new(|| {
                let mut pairs = VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS.to_vec();
                pairs.sort_unstable();
                pairs
            });
        &SORTED
    }

    /// Whether `verb` is both declared [`VerbCategory::Assertive`] (the
    /// speech-act tag for handlers that "retrieve and present facts" rather
    /// than committing a domain change) AND explicitly opted in to
    /// admission-pressure audit degradation via
    /// [`Self::ADMISSION_DEGRADE_SAFE_VERBS`] under the exact pack that
    /// registered it, AND declared by a pack the composition root actually
    /// vouches for (see [`VerbRegistryBuilder::register_boxed`]'s doc).
    /// Unknown, non-opted-in, wrong-pack, or untrusted-pack verbs are
    /// conservatively `false` — fail-closed, so a new Assertive handler (or
    /// one registered by a pack other than the one the allowlist names, or
    /// one registered through [`VerbRegistryBuilder::register`] rather than
    /// the trusted path) hard-fails its audit obligation like any write
    /// until someone deliberately reviews it and adds it to the allowlist.
    ///
    /// `pack.name()` is a value the `PackRuntime` trait object reports about
    /// itself — any pack registered through the public
    /// [`VerbRegistryBuilder::register`] path can claim any name, including
    /// one on the allowlist, whether or not the pack that name actually
    /// belongs to is also loaded (verb names are unique per registry, so an
    /// impostor's same-named handler is only reachable when the real pack
    /// is absent). Binding eligibility to registration-time trust — decided
    /// by the *caller*, never by the pack instance — is why this checks
    /// `degrade_safe_verbs` rather than resolving `pack.name()` at query
    /// time; [`VerbRegistryBuilder::build`] already excluded every untrusted
    /// pack's handlers from that set.
    ///
    /// The whole decision is precomputed once in `VerbRegistryBuilder::build`
    /// into [`VerbRegistry::degrade_safe_verbs`] — a verb name is unique
    /// across `Visibility::Verb` handlers within one registry
    /// (`validate_unique_verb_names`), so this is a single hash-set lookup,
    /// not a per-dispatch scan over every registered pack's handler list.
    ///
    /// Used only to decide whether a dispatch's own audit-obligation row may
    /// degrade to best-effort on transient audit-lane admission pressure
    /// (`append_audit_event_best_effort`) — a read that performed no domain
    /// write must not fail the caller just because the audit lane is
    /// momentarily saturated. Never used for permission checking, transport
    /// routing, or return-shape selection.
    fn admission_degrade_safe(&self, verb: &str) -> bool {
        self.degrade_safe_verbs.contains(verb)
    }

    /// Transport replay eligibility from the shared operation-effects table,
    /// restricted to trusted canonical public handlers. A read that persists a
    /// fresh serve or telemetry row is excluded because the request id is
    /// correlation, not deduplication.
    /// Custom and mounted handlers cannot inherit safety from a name/category.
    pub fn is_read_replay_safe(&self, verb: &str) -> bool {
        self.read_replay_safe_verbs.contains(verb)
    }

    /// White-box accessor for [`Self::admission_degrade_safe`], needed
    /// because the admission-pressure regression tests in
    /// `tests/read_verb_admission_exhaustion.rs` compile as a separate
    /// external binary and cannot reach a crate-private method directly —
    /// the same reason [`audit_admission_refused_obligation_count`] and
    /// `AuditBatch::test_snapshot` are `pub` rather than `pub(crate)`.
    #[cfg(any(test, feature = "test-internals"))]
    pub fn admission_degrade_safe_probe(&self, verb: &str) -> bool {
        self.admission_degrade_safe(verb)
    }

    /// Return the help schema envelope for a verb.
    ///
    /// Walks registered packs for the first matching `HandlerDef` and returns a
    /// structured JSON envelope. Subhandlers carry `callable_via_mcp: false`.
    /// Every envelope carries the shared `identifier_resolution` contract.
    /// `link`'s envelope additionally carries `endpoint_rules` — the composed
    /// per-relation source/target allowlist (issue #964) — so batch callers can
    /// defer to the kernel's own table instead of re-implementing it locally.
    /// Every `uuid`/`array of uuid` parameter description has its
    /// declared [`IdResolutionMode`]'s contract text appended — the same
    /// text rendered under `identifier_resolution.resolution_modes` — so the
    /// full-UUID-vs-short-prefix rule is stated once per mode (in
    /// `resolution_mode_contract`) and inherited by every matching param,
    /// instead of restating it per param across every `HandlerDef` in every
    /// pack. Parameters whose mode is [`IdResolutionMode::NotApplicable`]
    /// (every non-identifier parameter) are left unchanged.
    /// Unknown verbs return `RuntimeError::InvalidInput`. Full shape documented
    /// in `docs/protocol.md` §Request Schema.
    pub fn describe_verb(&self, verb: &str) -> Result<Value, RuntimeError> {
        for pack in self.packs.iter() {
            for handler in pack.handlers().iter() {
                if handler.name == verb {
                    let category = format!("{:?}", handler.category);
                    let params_arr: Vec<Value> = handler
                        .params
                        .iter()
                        .map(|p| {
                            let description = match resolution_mode_contract(p.resolution_mode) {
                                Some(contract) => format!("{} {}", p.description, contract),
                                None => p.description.to_string(),
                            };
                            serde_json::json!({
                                "name": p.name,
                                "type": p.param_type,
                                "required": p.required,
                                "description": description,
                            })
                        })
                        .collect();
                    // Subhandlers are not callable via the MCP request surface;
                    // the help payload must match the behaviour the dispatch
                    // path enforces so callers reading `help=true` before
                    // probing see accurate availability.
                    if matches!(handler.visibility, Visibility::Subhandler) {
                        return Ok(serde_json::json!({
                            "verb": verb,
                            "pack": pack.name(),
                            "description": handler.description,
                            "category": category,
                            "params": params_arr,
                            "identifier_resolution": identifier_resolution_help(),
                            "visibility": "internal",
                            "callable_via_mcp": false,
                            "note": "This is an internal subhandler. Calling it via the MCP \
                                     request surface returns permission denied. It can only be \
                                     invoked by internal runtime callers.",
                        }));
                    }
                    let mut envelope = serde_json::json!({
                        "verb": verb,
                        "pack": pack.name(),
                        "description": handler.description,
                        "category": category,
                        "params": params_arr,
                        "identifier_resolution": identifier_resolution_help(),
                    });
                    // A pack that authored its own schema keeps it; every other
                    // verb gets one derived from the declarations the runtime
                    // already holds, so a bridged model has a schema to read
                    // instead of parsing the prose `params[].type`.
                    if let Some(schema) = pack.input_schema(verb) {
                        envelope["input_schema"] = schema;
                    } else {
                        let described: Vec<(String, String)> = params_arr
                            .iter()
                            .map(|p| {
                                (
                                    p["name"].as_str().unwrap_or_default().to_string(),
                                    p["description"].as_str().unwrap_or_default().to_string(),
                                )
                            })
                            .collect();
                        if let Some(schema) =
                            crate::input_schema::derive_input_schema(handler.params, &described)
                        {
                            envelope["input_schema"] = schema;
                        }
                    }
                    if verb == "link" {
                        envelope["endpoint_rules"] = Value::Array(edge_endpoint_table(&self.packs));
                    }
                    return Ok(envelope);
                }
            }
        }
        // Verb-visibility handler names, precomputed at build() time (internal
        // subhandlers are excluded so they are not advertised in the
        // unknown-verb error).
        Err(RuntimeError::UnknownVerb(format!(
            "unknown verb {verb:?}; available: {}",
            self.available_verbs.join(", ")
        )))
    }

    /// Check whether the gate permits writes into `ns`.
    ///
    /// Performs a gate evaluation with verb `"authorize"` before any background
    /// loop is spawned (ADR-056 §6).  Returns `Ok(())` when the gate allows the
    /// namespace, or `Err(RuntimeError::PermissionDenied{..})` when denied.
    /// Gate errors (implementation failures) are surfaced as
    /// `RuntimeError::Internal` carrying the stable classified reason; the
    /// bounded, masked backend detail goes to the server-side log here, since
    /// callers log the returned error.
    pub fn authorize_namespace(&self, ns: Namespace) -> Result<(), RuntimeError> {
        let actor = crate::actor_identity::resolve_actor(self.actor_id.as_deref());
        let req = GateRequest::new(actor, ns, "authorize", serde_json::Value::Null);
        match self.gate.check(&req) {
            Ok(decision) if decision.is_allow() => Ok(()),
            Ok(GateDecision::Deny { reason }) => {
                Err(RuntimeError::permission_denied("authorize", reason))
            }
            Ok(_) => Err(RuntimeError::permission_denied("authorize", "gate denied")),
            Err(e) => {
                tracing::warn!(
                    error = %crate::secret_gate::bounded_masked_log_text(&e.to_string()),
                    "authorize_namespace: gate check failed (fail-closed)"
                );
                Err(RuntimeError::Internal(format!(
                    "gate error: {}",
                    e.wire_reason()
                )))
            }
        }
    }

    /// Gate and execute an operation handled outside normal pack dispatch.
    ///
    /// Multi-backend transports use this to route an operation through a
    /// coordinator while retaining [`Self::dispatch_with_identity`]'s gate and
    /// audit lifecycle. Deny is authoritative, gate errors fail closed, and an
    /// allowed audit is persisted after the intercepted operation resolves so
    /// its outcome and duration reflect the operation result. Successful
    /// `git.digest` interception uses the same strict durable-receipt exception
    /// as normal pack dispatch.
    pub async fn dispatch_intercepted_with_identity<F, Fut>(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
        dispatch: F,
    ) -> Result<Value, RuntimeError>
    where
        F: FnOnce(Namespace) -> Fut,
        Fut: std::future::Future<Output = Result<Value, RuntimeError>>,
    {
        self.dispatch_intercepted_with_metadata_with_identity(
            verb,
            params,
            identity,
            |namespace| async move {
                dispatch(namespace)
                    .await
                    .map(|result| InterceptedDispatchResult::new(result, ()))
            },
        )
        .await
        .map(|outcome| outcome.result)
    }

    /// Gate and execute an intercepted operation whose transport needs typed
    /// metadata in addition to the canonical verb result.
    ///
    /// Audit accounting always receives `outcome.result`; `outcome.metadata`
    /// crosses the dispatch seam unchanged for the transport to place beside
    /// that result in its own envelope.
    pub async fn dispatch_intercepted_with_metadata_with_identity<M, F, Fut>(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
        dispatch: F,
    ) -> Result<InterceptedDispatchResult<M>, RuntimeError>
    where
        F: FnOnce(Namespace) -> Fut,
        Fut: std::future::Future<Output = Result<InterceptedDispatchResult<M>, RuntimeError>>,
    {
        self.dispatch_intercepted_with_metadata_and_disposition(verb, params, identity, dispatch)
            .await
            .map_err(DispatchError::into_source)
    }

    /// Append the `GateDenied` row of a refused dispatch and report what the
    /// caller may cite: the row's id when it committed, otherwise why not.
    async fn append_gate_denied_row(
        &self,
        store: &Arc<dyn EventStore>,
        event: Event,
        verb: &str,
    ) -> crate::error::DenialReceipt {
        let audit_event_id = event.id;
        match append_audit_event_best_effort(
            self.audit_batch.as_ref(),
            store,
            event,
            verb,
            crate::audit_batch::AuditProducer::GateDenied,
            false,
        )
        .await
        {
            Ok(()) => crate::error::DenialReceipt {
                audit_event_id: Some(audit_event_id),
                audit_outcome: crate::error::DenialAuditOutcome::Committed,
            },
            Err(failure) => crate::error::DenialReceipt {
                audit_event_id: None,
                audit_outcome: crate::error::DenialAuditOutcome::NotCommitted(failure.wire_code()),
            },
        }
    }

    /// Execute an intercepted operation while retaining this boundary's failure provenance.
    /// Successful canonical results and typed metadata are returned unchanged.
    pub async fn dispatch_intercepted_with_metadata_and_disposition<M, F, Fut>(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
        dispatch: F,
    ) -> Result<InterceptedDispatchResult<M>, DispatchError>
    where
        F: FnOnce(Namespace) -> Fut,
        Fut: std::future::Future<Output = Result<InterceptedDispatchResult<M>, RuntimeError>>,
    {
        self.dispatch_intercepted_with_token_and_disposition(verb, params, identity, |token| {
            dispatch(token.gate_namespace().clone())
        })
        .await
    }

    /// Intercept an operation with the sealed caller token minted after the
    /// gate decision. Coordinated reads retain the resolved actor and their
    /// existing namespace selection without reconstructing identity from args.
    pub async fn dispatch_intercepted_with_token_and_disposition<M, F, Fut>(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
        dispatch: F,
    ) -> Result<InterceptedDispatchResult<M>, DispatchError>
    where
        F: FnOnce(NamespaceToken) -> Fut,
        Fut: std::future::Future<Output = Result<InterceptedDispatchResult<M>, RuntimeError>>,
    {
        let request_id = identity.and_then(|id| id.request_id);
        let gate_req = self
            .gate_request_with_identity(verb, params, identity)
            .map_err(DispatchError::before_dispatch)?;
        let gate_decision = khive_gate::check_with_mailbox_policy(self.gate.as_ref(), &gate_req);
        let mut deferred_audit = match gate_decision {
            Ok(decision) => {
                let audit = masked_audit_event(&gate_req, &decision, self.gate.impl_name());
                tracing::info!(
                    audit_event = %serde_json::to_string(&audit)
                        .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
                    "gate.check"
                );
                if let GateDecision::Deny { reason } = decision {
                    let receipt = match &self.event_store {
                        Some(store) => {
                            let event = build_audit_storage_event(
                                &gate_req,
                                &audit,
                                EventOutcome::Denied,
                                Some(crate::cost_unit::base_resource_payload(request_id)),
                            );
                            // The dispatch returns `PermissionDenied` below
                            // whether or not this row commits — a deny never
                            // reports success — so a commit failure has no
                            // caller-visible outcome to fold into; the receipt
                            // on the refusal says whether the row the caller
                            // could cite exists.
                            self.append_gate_denied_row(store, event, verb).await
                        }
                        None => crate::error::DenialReceipt::no_store(),
                    };
                    return Err(DispatchError::before_dispatch(
                        RuntimeError::PermissionDenied {
                            verb: verb.to_string(),
                            reason,
                            receipt: Box::new(receipt),
                        },
                    ));
                }
                Some(audit)
            }
            Err(err) => {
                return Err(DispatchError::before_dispatch(
                    self.gate_unavailable_error(&gate_req, &err, request_id, None)
                        .await,
                ));
            }
        };

        let started = Instant::now();
        let token = self.mint_intercepted_read_token(&gate_req, params, identity);
        let mut result = dispatch(token).await;
        let domain_succeeded = result.is_ok();
        let duration_us = started.elapsed().as_micros() as i64;
        let receipt_outcome = if verb == "git.digest" && result.is_ok() {
            let resource = result.as_ref().ok().map(|outcome| {
                crate::cost_unit::resource_payload(
                    verb,
                    &gate_req.args,
                    &outcome.result,
                    || 0,
                    request_id,
                )
            });
            // The receipt helper operates on the canonical verb result. Move
            // that value out temporarily so it can turn receipt failures into
            // the outer dispatch error without discarding successful typed
            // transport metadata.
            let mut receipt_result: Result<Value, RuntimeError> = match result.as_mut() {
                Ok(outcome) => Ok(std::mem::take(&mut outcome.result)),
                Err(_) => unreachable!("git.digest receipt path is guarded by result.is_ok()"),
            };
            let outcome = persist_git_digest_receipt(
                self.event_store.as_ref(),
                self.audit_batch.as_ref(),
                &gate_req,
                deferred_audit.as_ref(),
                &mut receipt_result,
                duration_us,
                resource,
            )
            .await;
            match receipt_result {
                Ok(receipted_result) => {
                    if let Ok(intercepted) = &mut result {
                        intercepted.result = receipted_result;
                    }
                }
                Err(error) => result = Err(error),
            }
            Some(outcome)
        } else {
            None
        };
        if receipt_outcome.is_none()
            || receipt_outcome == Some(GitDigestReceiptOutcome::BuildRejected)
        {
            if let Some(audit) = deferred_audit.take() {
                let audit_outcome = self
                    .persist_intercepted_audit(
                        verb,
                        &gate_req,
                        audit,
                        result.as_ref().map(|outcome| &outcome.result),
                        duration_us,
                        request_id,
                    )
                    .await;
                result = fold_audit_obligation(result, audit_outcome, |outcome| outcome.result);
            }
        }
        result.map_err(|error| DispatchError::after_handler(error, domain_succeeded))
    }

    async fn persist_intercepted_audit(
        &self,
        verb: &str,
        gate_req: &GateRequest,
        audit: AuditEvent,
        result: Result<&Value, &RuntimeError>,
        duration_us: i64,
        request_id: Option<u64>,
    ) -> Result<(), AuditObligationFailure> {
        let Some(store) = &self.event_store else {
            return Ok(());
        };
        let event = match result {
            Ok(value) if verb == "link" && gate_req.args.get("links").is_none() => {
                let resource = crate::cost_unit::resource_payload(
                    verb,
                    &gate_req.args,
                    value,
                    || 0,
                    request_id,
                );
                match link_audit_success_from_result(audit.clone(), value) {
                    Some((edge_id, mut payload)) => {
                        if let Value::Object(ref mut map) = payload {
                            map.insert("resource".to_string(), resource);
                        }
                        Event::new(
                            gate_req.namespace.as_str(),
                            gate_req.verb.as_str(),
                            EventKind::Audit,
                            SubstrateKind::Event,
                            format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
                        )
                        .with_outcome(EventOutcome::Success)
                        .with_target(edge_id)
                        .with_payload(payload)
                        .with_payload_schema_version(2)
                        .with_duration_us(duration_us)
                    }
                    None => build_audit_storage_event(
                        gate_req,
                        &audit,
                        EventOutcome::Success,
                        Some(resource),
                    )
                    .with_duration_us(duration_us),
                }
            }
            Ok(value) => build_audit_storage_event(
                gate_req,
                &audit,
                EventOutcome::Success,
                Some(crate::cost_unit::resource_payload(
                    verb,
                    &gate_req.args,
                    value,
                    || 0,
                    request_id,
                )),
            )
            .with_duration_us(duration_us),
            Err(_) => build_audit_storage_event(
                gate_req,
                &audit,
                EventOutcome::Error,
                Some(crate::cost_unit::base_resource_payload(request_id)),
            )
            .with_duration_us(duration_us),
        };
        let producer = if result.is_ok() {
            crate::audit_batch::AuditProducer::DispatchSucceeded
        } else {
            crate::audit_batch::AuditProducer::DispatchFailed
        };
        append_audit_event_best_effort(
            self.audit_batch.as_ref(),
            store,
            event,
            verb,
            producer,
            self.admission_degrade_safe(verb),
        )
        .await
    }

    /// A create refusal may reveal its key holder only when the same caller can list it.
    pub fn allows_note_key_disclosure(
        &self,
        token: &NamespaceToken,
        kind: &str,
        key: &str,
    ) -> bool {
        let request = GateRequest::new(
            token.actor().clone(),
            token.namespace().clone(),
            "list",
            serde_json::json!({"kind":"note", "note_kind":kind, "key_prefix":key}),
        );
        self.gate
            .check(&request)
            .is_ok_and(|decision| decision.is_allow())
    }

    fn mint_intercepted_read_token(
        &self,
        gate_req: &GateRequest,
        params: &Value,
        identity: Option<&RequestIdentity>,
    ) -> NamespaceToken {
        // Preserve the coordinator's existing namespace selection: the gate
        // namespace is primary, and explicit namespace input stays narrow.
        let visible = if params.get("namespace").is_some() {
            Vec::new()
        } else {
            let mut visible = match identity {
                Some(identity) => identity
                    .visible_namespaces
                    .iter()
                    .filter_map(|namespace| Namespace::parse(namespace).ok())
                    .collect(),
                None => self.visible_namespaces.clone(),
            };
            visible.push(Namespace::local());
            visible
        };
        NamespaceToken::mint_with_visibility(
            gate_req.namespace.clone(),
            visible,
            gate_req.actor.clone(),
        )
        .with_gate_namespace(gate_req.namespace.clone())
        .with_gate_explicit_namespace(
            params
                .get("namespace")
                .and_then(Value::as_str)
                .map(str::to_owned),
        )
        .with_request_id(identity.and_then(|identity| identity.request_id))
        .with_process_ref(match identity {
            Some(identity) => identity.process_ref.clone(),
            None => crate::config::process_ref_from_env(),
        })
    }

    fn gate_request_with_identity(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
    ) -> Result<GateRequest, RuntimeError> {
        let default_namespace = identity
            .map(|id| id.namespace.as_str())
            .unwrap_or(self.default_namespace.as_str());
        let namespace = resolve_explicit_namespace(params, default_namespace)?;
        let actor_id = identity
            .map(|id| id.actor_id.as_deref())
            .unwrap_or(self.actor_id.as_deref());
        let actor = crate::actor_identity::resolve_actor(actor_id);
        // GateRequest.args deliberately captures submitted dispatch arguments.
        // The handler's canonicalization and kind hooks have not run; a policy
        // requiring their effective values belongs after that handler work.
        let req = GateRequest::new(actor, namespace, verb, params.clone());
        crate::mailbox_view::validate_mailbox_request(&req)?;
        Ok(req)
    }

    async fn gate_unavailable_error(
        &self,
        gate_req: &GateRequest,
        error: &khive_gate::GateError,
        request_id: Option<u64>,
        effective_target: Option<uuid::Uuid>,
    ) -> RuntimeError {
        let audit = AuditEvent::gate_unavailable(gate_req, self.gate.impl_name())
            .with_operation_attribution(
                khive_storage::operation_context::current_operation_attribution(),
            );
        tracing::info!(
            audit_event = %serde_json::to_string(&audit)
                .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
            "gate.check"
        );
        tracing::warn!(
            verb = %gate_req.verb,
            error = %crate::secret_gate::bounded_masked_log_text(&error.to_string()),
            "gate check failed (fail-closed)"
        );
        if let Some(store) = &self.event_store {
            let mut event = build_audit_storage_event(
                gate_req,
                &audit,
                EventOutcome::Error,
                Some(crate::cost_unit::base_resource_payload(request_id)),
            );
            if let Some(target) = effective_target {
                event = event.with_target(target);
            }
            let _ = append_audit_event_best_effort(
                self.audit_batch.as_ref(),
                store,
                event,
                gate_req.verb.as_str(),
                crate::audit_batch::AuditProducer::GateUnavailable,
                false,
            )
            .await;
        }
        RuntimeError::GateUnavailable {
            verb: gate_req.verb.clone(),
            // Caller-visible: a stable, classified reason derived from the
            // `GateError` variant only. `error`'s `Display` text is logged
            // above (server-side, via `tracing::warn!`) and must never be
            // interpolated here — a gate backend's error message can embed
            // connection details, addresses, or credentials.
            reason: error.wire_reason().to_string(),
        }
    }

    /// Dispatch a verb to the first pack that handles it.
    ///
    /// Routes through the gate, then invokes the matching pack handler. When
    /// `params["help"] == true`, short-circuits to `describe_verb` with no side effects.
    /// Gate errors fail closed. Full dispatch flow documented in `docs/protocol.md`.
    ///
    /// Equivalent to `self.dispatch_with_identity(verb, params, None)` — uses
    /// this registry's construction-baked `default_namespace` / `actor_id` /
    /// `visible_namespaces`.
    pub async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError> {
        self.dispatch_with_identity(verb, params, None).await
    }

    /// Dispatch a verb, optionally overriding this registry's baked identity
    /// scalars for exactly this call (ADR-096 Fork 1).
    ///
    /// `identity = None` behaves exactly like [`Self::dispatch`]. `identity =
    /// Some(id)` uses `id.namespace` / `id.actor_id` / `id.visible_namespaces`
    /// in place of `self.default_namespace` / `self.actor_id` /
    /// `self.visible_namespaces` for this call's namespace resolution, gate
    /// request, and token minting. The registry's own fields are never mutated,
    /// so concurrent calls with different (or no) identity are independent.
    /// See `docs/api/pack.md#dispatch_with_identity` for why this enables one warm
    /// registry to serve many attribution identities over a shared backend.
    pub async fn dispatch_with_identity(
        &self,
        verb: &str,
        params: Value,
        identity: Option<RequestIdentity>,
    ) -> Result<Value, RuntimeError> {
        self.dispatch_with_disposition(verb, params, identity)
            .await
            .map_err(DispatchError::into_source)
    }

    /// Dispatch with provenance for this operation's own domain result.
    /// Errors returned by a nested dispatch remain handler errors at this boundary.
    pub async fn dispatch_with_disposition(
        &self,
        verb: &str,
        params: Value,
        identity: Option<RequestIdentity>,
    ) -> Result<Value, DispatchError> {
        // help=true interception: short-circuit before gate/pack.
        if params.get("help").and_then(Value::as_bool) == Some(true) {
            let result = match self.describe_verb(verb) {
                Ok(value) => Ok(value),
                Err(error) => match self.mounted_verb_catalog().await {
                    Ok(catalog) => catalog
                        .into_iter()
                        .find(|entry| entry["verb"] == verb)
                        .ok_or(error),
                    Err(error) => Err(error),
                },
            };
            return result.map_err(DispatchError::before_dispatch);
        }
        // Resolve namespace before `params` is moved into pack.dispatch, so the
        // post-dispatch hook can reference it.
        //
        // Absent `namespace` and a present-but-malformed `namespace` are
        // different cases. A present non-string value (null, number, bool,
        // array, object) is explicit caller input that failed to parse and
        // must fail closed, not silently coerce to the default namespace.
        // Only a genuinely absent key defaults. Shared with the multi-backend
        // coordinator intercept via `resolve_explicit_namespace` so every MCP
        // ingress path applies the same fail-closed rule.
        let explicit_namespace = params.get("namespace").is_some_and(Value::is_string);
        // The caller-supplied correlation id (khive#948), if any. Read once
        // here so it is in scope for every audit-append site below,
        // including the ones that run before pack dispatch is attempted.
        let request_id: Option<u64> = identity.as_ref().and_then(|id| id.request_id);
        // Thread the configured actor identity into the gate request so the
        // gate can distinguish human vs agent callers at the dispatch seam.
        // Resolved once via the shared actor-identity policy and reused for
        // token minting below, so the gate's notion of "who is the caller"
        // and the storage token's notion can never drift apart.
        let gate_req = self
            .gate_request_with_identity(verb, &params, identity.as_ref())
            .map_err(DispatchError::before_dispatch)?;
        let ns = gate_req.namespace.clone();
        let resolved_actor = gate_req.actor.clone();

        // Consult the gate.
        //
        // - Ok(Allow) → proceed to pack dispatch (tracing + optional EventStore).
        // - Ok(Deny) → emit audit, persist if store configured, return PermissionDenied.
        // - Err(_) → emit an outage audit and return GateUnavailable.
        let gate_decision = khive_gate::check_with_mailbox_policy(self.gate.as_ref(), &gate_req);
        let (gate_blocked, mut deferred_audit) = match gate_decision {
            Ok(decision) => {
                let is_deny = matches!(decision, GateDecision::Deny { .. });

                // Emit audit event via tracing.
                let audit = masked_audit_event(&gate_req, &decision, self.gate.impl_name());
                tracing::info!(
                    audit_event = %serde_json::to_string(&audit)
                        .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
                    "gate.check"
                );

                // Drain any process-lifetime `OnceLock` config locks queued
                // since the last dispatch and persist them as `ConfigLocked`
                // events, riding this same audit-persistence gate. The
                // namespace/actor stamped on these rows are whichever
                // dispatch happens to observe the queue non-empty first:
                // an accepted provenance quirk, preferred over threading an
                // `EventStore` handle into every synchronous
                // `OnceLock::get_or_init` call site. The verb column is NOT
                // inherited from that bystander dispatch: a config-lock row
                // wearing an operation verb pollutes verb-filtered queries
                // (e.g. per-verb receipt counts), so these rows carry their
                // own `config.lock` pseudo-verb and remain discoverable by
                // `EventKind::ConfigLocked`.
                if let Some(store) = &self.event_store {
                    if crate::config_ledger::PENDING
                        .swap(false, std::sync::atomic::Ordering::AcqRel)
                    {
                        for (key, value) in crate::config_ledger::drain_config_locked() {
                            let payload = serde_json::json!({ "key": key, "value": value });
                            let storage_event = Event::new(
                                gate_req.namespace.as_str(),
                                "config.lock",
                                EventKind::ConfigLocked,
                                SubstrateKind::Event,
                                format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
                            )
                            .with_payload(payload);
                            // ConfigLocked is pure observability: the helper
                            // never returns `Err` for it, so there is
                            // nothing to fold.
                            let _ = append_audit_event_best_effort(
                                self.audit_batch.as_ref(),
                                store,
                                storage_event,
                                "config.lock",
                                crate::audit_batch::AuditProducer::ConfigLocked,
                                false,
                            )
                            .await;
                        }
                    }
                }

                // Every Allow-outcome audit row defers its append until pack
                // dispatch returns, so the row can carry the measured
                // dispatch time in `duration_us` (persisting before dispatch
                // ran always recorded the `Event::new` default of 0). A
                // singleton `link` call (no `links` bulk array) additionally
                // enriches the deferred row with the created/resolved edge
                // fields (schema v2) once dispatch resolves. Denied calls
                // have no dispatch to wait for and keep the immediate v1
                // append below.
                //
                // Accepted trade-off for ordinary verbs: a crash between this
                // Allow decision and the deferred append loses the audit row.
                // `git.digest` narrows the caller-visible contract below: it
                // never returns success until the deferred receipt append is
                // confirmed, though a process crash can still leave committed
                // ingest writes with no response and no completed receipt.
                let defer_audit = !is_deny;

                // Persist to EventStore immediately only for denied calls;
                // the receipt rides on the refusal so the caller can cite
                // the row.
                let reason = if is_deny {
                    let reason = match decision {
                        GateDecision::Deny { reason } => reason,
                        _ => String::new(),
                    };
                    let receipt = match &self.event_store {
                        Some(store) => {
                            // ADR-103 Decision (a): the closed `work_class` enum
                            // is stamped on every event, denial included -- only
                            // `resource.cost_unit` is scoped to a successful
                            // dispatch by Amendment 1. `base_resource_payload()`
                            // carries `work_class` alone, no `cost_unit` key.
                            let storage_event = build_audit_storage_event(
                                &gate_req,
                                &audit,
                                EventOutcome::Denied,
                                Some(crate::cost_unit::base_resource_payload(request_id)),
                            );
                            // This path always returns `PermissionDenied`
                            // below, so there is no success outcome to fold a
                            // commit failure into; the receipt says whether
                            // the row exists.
                            self.append_gate_denied_row(store, storage_event, verb)
                                .await
                        }
                        None => crate::error::DenialReceipt::no_store(),
                    };
                    Some((reason, receipt))
                } else {
                    None
                };
                let deferred = if defer_audit { Some(audit) } else { None };
                (reason, deferred)
            }
            Err(err) => {
                return Err(DispatchError::before_dispatch(
                    self.gate_unavailable_error(&gate_req, &err, request_id, None)
                        .await,
                ));
            }
        };

        // Hard enforcement: Deny is authoritative.
        if let Some((reason, receipt)) = gate_blocked {
            return Err(DispatchError::before_dispatch(
                RuntimeError::PermissionDenied {
                    verb: verb.to_string(),
                    reason,
                    receipt: Box::new(receipt),
                },
            ));
        }

        // Mint the authorized storage token at the dispatch boundary.
        //
        // Writes pin to `local` by default. Actor identity and config
        // `[actor] id` are attribution and gate-context inputs only: they
        // never route storage. The explicit `namespace=` request param is a
        // precise single-namespace escape: the caller deliberately
        // reads/writes exactly that one set; it is NOT widened by `visible_namespaces`.
        //
        // When actor_id is configured, mint a token carrying that actor
        // label so that comm.inbox applies the to_actor filter for directed delivery.
        // Otherwise, use ActorRef::anonymous() and inbox falls back to party-line.
        // `actor_id_str` already reflects the per-request identity override
        // when supplied (resolved above into `resolved_actor`, mirrored into
        // the gate request). Reusing the same value here guarantees the
        // gate's actor and the storage token's actor can never diverge.
        //
        // On the default (no explicit `namespace=`) path, the read scope
        // widens to `['local'] ∪ visible_namespaces` (baked, or the
        // per-request override). `'local'` is always included
        // (mint_with_visibility deduplicates). Writes remain pinned to
        // `'local'`. Per-actor distinctions use view-layer tag filters
        // (assignee, actor_id, from/to), not namespace partitions. `ns`/
        // `explicit_namespace` were already validated above: reuse them
        // instead of re-reading `params["namespace"]` with `as_str()`, which
        // would silently drop malformed non-string values again.
        let token = if explicit_namespace {
            // Explicit escape: precise single-namespace scope, read+write. NOT widened.
            NamespaceToken::mint_with_visibility(ns.clone(), vec![], resolved_actor)
        } else {
            // Default path: write namespace = local; read scope = ['local'] ∪ visible_namespaces.
            let primary = Namespace::local();
            let mut extra_visible: Vec<Namespace> = match identity.as_ref() {
                Some(id) => id
                    .visible_namespaces
                    .iter()
                    .filter_map(|s| match Namespace::parse(s) {
                        Ok(parsed) => Some(parsed),
                        Err(e) => {
                            tracing::warn!(
                                namespace = %s,
                                error = %e,
                                "dispatch_with_identity: skipping invalid visible_namespace \
                                 entry from per-request identity"
                            );
                            None
                        }
                    })
                    .collect(),
                None => self.visible_namespaces.clone(),
            };
            // ADR-007 Rev 4 Rule 3b, applied once at the seam every identity
            // path shares: a non-`local` actor reads its own namespace by
            // default (its episodic memories land there), whether the identity
            // came from the config loader, a daemon frame, a scheduled replay
            // or an embedding host. Writes stay pinned to `local` (Rule 0).
            if let Some(actor_namespace) = resolved_actor
                .binding_id()
                .filter(|id| *id != Namespace::LOCAL)
                .and_then(|id| Namespace::parse(id).ok())
            {
                extra_visible.push(actor_namespace);
            }
            extra_visible.push(Namespace::local()); // 'local' always readable; mint dedups
            NamespaceToken::mint_with_visibility(primary, extra_visible, resolved_actor)
        }
        .with_gate_namespace(ns.clone())
        .with_gate_explicit_namespace(
            params
                .get("namespace")
                .and_then(Value::as_str)
                .map(str::to_owned),
        )
        .with_request_id(request_id)
        .with_process_ref(match identity.as_ref() {
            Some(id) => id.process_ref.clone(),
            None => crate::config::process_ref_from_env(),
        });

        for pack in self.packs.iter() {
            let handler_def = pack.handlers().iter().find(|v| v.name == verb);
            let mounted_name = pack.mounted_namespace().and_then(|prefix| {
                verb.strip_prefix(prefix)
                    .and_then(|suffix| suffix.strip_prefix('.'))
            });
            if handler_def.is_some() || mounted_name.is_some() {
                let definition = if let Some(name) = mounted_name {
                    pack.mounted_catalog().await.and_then(|catalog| {
                        catalog
                            .into_iter()
                            .find(|definition| definition.name == name)
                            .map(Some)
                            .ok_or_else(|| RuntimeError::UnknownVerb(verb.to_owned()))
                    })
                } else {
                    Ok(None)
                };
                // Strip `namespace` from params before forwarding to packs.
                // The registry has already consumed it to mint the NamespaceToken.
                //
                // Exception: if the handler's own `params` schema declares
                // `"namespace"` as a valid field (e.g. brain.bind, brain.unbind,
                // brain.bindings, brain.resolve), the field is a *business* argument
                // — not a transport routing key — and must be passed through
                // unchanged. Stripping it would silently default the binding to the
                // "*" wildcard, broadening profile scope across namespaces.
                let handler_accepts_namespace = handler_def
                    .is_some_and(|h| h.params.iter().any(|p| p.name == "namespace"))
                    || definition
                        .as_ref()
                        .ok()
                        .and_then(|value| value.as_ref())
                        .is_some_and(|definition| {
                            definition
                                .input_schema
                                .get("properties")
                                .is_some_and(|properties| properties.get("namespace").is_some())
                        });
                let params = if !handler_accepts_namespace {
                    if let Value::Object(mut map) = params {
                        map.remove("namespace");
                        Value::Object(map)
                    } else {
                        params
                    }
                } else {
                    params
                };
                let dispatch_start = Instant::now();
                let mounted_audit = definition.as_ref().ok().and_then(|v| v.as_ref()).map(|v| {
                    serde_json::json!({"mount": pack.name(), "effect": v.effect, "generation": v.generation})
                });
                let mut result = match definition {
                    Ok(Some(definition)) => {
                        pack.dispatch_mounted(&definition, verb, params, self, &token)
                            .await
                    }
                    Ok(None) => pack.dispatch(verb, params, self, &token).await,
                    Err(error) => Err(error),
                };
                let domain_succeeded = result.is_ok();
                let dispatch_us = dispatch_start.elapsed().as_micros() as i64;

                // Unlike ordinary audit rows, a successful `git.digest`
                // response is returned only after its complete report has
                // been durably persisted as a schema-v2 audit receipt. The
                // receipt helper borrows the deferred audit row so malformed
                // handler output can still fall back to one generic Error
                // audit. Handler errors use that same ordinary path below.
                let git_digest_receipt_outcome = if verb == "git.digest" && result.is_ok() {
                    let resource = result.as_ref().ok().map(|value| {
                        crate::cost_unit::resource_payload(
                            verb,
                            &gate_req.args,
                            value,
                            || pack.registered_embedding_model_names().len() as i64,
                            request_id,
                        )
                    });
                    Some(
                        persist_git_digest_receipt(
                            self.event_store.as_ref(),
                            self.audit_batch.as_ref(),
                            &gate_req,
                            deferred_audit.as_ref(),
                            &mut result,
                            dispatch_us,
                            resource,
                        )
                        .await,
                    )
                } else {
                    None
                };

                // Append the deferred Allow-outcome audit row now that
                // dispatch has resolved, so `duration_us` carries the
                // measured `dispatch_us` instead of the `Event::new` default
                // of 0. A successful singleton `link` call enriches the row
                // with the created/resolved edge (schema v2); anything that
                // cannot be enriched, or is not a singleton `link` call,
                // falls back to the generic v1 audit shape so no audit row
                // is ever dropped for the deferred path.
                let needs_generic_audit = git_digest_receipt_outcome.is_none()
                    || git_digest_receipt_outcome == Some(GitDigestReceiptOutcome::BuildRejected);
                if let (true, Some(audit)) = (needs_generic_audit, deferred_audit.take()) {
                    if let Some(store) = &self.event_store {
                        let is_link_singleton =
                            verb == "link" && gate_req.args.get("links").is_none();
                        // Read-only pass over `result` first: every arm below
                        // only needs `audit_outcome` afterward, and folding a
                        // failure into `result` requires a mutable borrow
                        // that cannot coexist with the `&result` match below.
                        let audit_outcome: Result<(), AuditObligationFailure> = match &result {
                            Ok(ok_val) if is_link_singleton => {
                                // ADR-103 Amendment 1: `link` (singleton or
                                // bulk) has no embedding-bearing path — edges
                                // carry no embedded body — so cost_unit is
                                // always base_weight("link") alone. The
                                // registered-model closure is never invoked
                                // (per_item_weight("link", ..) short-circuits
                                // to 0 before `model_count` reads it).
                                let resource = crate::cost_unit::resource_payload(
                                    verb,
                                    &gate_req.args,
                                    ok_val,
                                    || pack.registered_embedding_model_names().len() as i64,
                                    request_id,
                                );
                                match link_audit_success_from_result(audit.clone(), ok_val) {
                                    Some((edge_id, mut payload)) => {
                                        if let Value::Object(ref mut map) = payload {
                                            map.insert("resource".to_string(), resource);
                                        }
                                        let storage_event = Event::new(
                                            gate_req.namespace.as_str(),
                                            gate_req.verb.as_str(),
                                            EventKind::Audit,
                                            SubstrateKind::Event,
                                            format!(
                                                "{}:{}",
                                                gate_req.actor.kind, gate_req.actor.id
                                            ),
                                        )
                                        .with_outcome(EventOutcome::Success)
                                        .with_target(edge_id)
                                        .with_payload(payload)
                                        .with_payload_schema_version(2)
                                        .with_duration_us(dispatch_us);
                                        append_audit_event_best_effort(
                                            self.audit_batch.as_ref(),
                                            store,
                                            storage_event,
                                            verb,
                                            crate::audit_batch::AuditProducer::DispatchSucceeded,
                                            self.admission_degrade_safe(verb),
                                        )
                                        .await
                                    }
                                    None => {
                                        tracing::warn!(
                                            verb,
                                            "link audit v2 enrichment parse failed; \
                                             falling back to v1 audit shape"
                                        );
                                        let storage_event = build_audit_storage_event(
                                            &gate_req,
                                            &audit,
                                            EventOutcome::Success,
                                            Some(resource),
                                        )
                                        .with_duration_us(dispatch_us);
                                        append_audit_event_best_effort(
                                            self.audit_batch.as_ref(),
                                            store,
                                            storage_event,
                                            verb,
                                            crate::audit_batch::AuditProducer::DispatchSucceeded,
                                            self.admission_degrade_safe(verb),
                                        )
                                        .await
                                    }
                                }
                            }
                            _ => {
                                // The persisted audit outcome must reflect
                                // the dispatch result, not be hardcoded to
                                // Success — otherwise a failed dispatch is
                                // recorded as successful work and disappears
                                // from `outcome=error` queries.
                                //
                                // ADR-103 Amendment 1: `resource.cost_unit` is
                                // computed ONLY on a successful dispatch —
                                // there is no handler `Value` to read
                                // `item_count` from on an error, and the
                                // amendment's "absence has exactly two
                                // meanings" rule requires the field be
                                // omitted, never defaulted to 0, on an
                                // errored dispatch. `work_class` itself is
                                // NOT one of those two omission cases
                                // (ADR-103 Decision (a) stamps it on every
                                // event), so an errored dispatch still gets
                                // `resource: {"work_class": "interactive"}`,
                                // just with no `cost_unit` key.
                                let (outcome, resource) = match &result {
                                    Ok(ok_val) => (
                                        EventOutcome::Success,
                                        Some(crate::cost_unit::resource_payload(
                                            verb,
                                            &gate_req.args,
                                            ok_val,
                                            || pack.registered_embedding_model_names().len() as i64,
                                            request_id,
                                        )),
                                    ),
                                    Err(_) => (
                                        EventOutcome::Error,
                                        Some(crate::cost_unit::base_resource_payload(request_id)),
                                    ),
                                };
                                let producer = if result.is_ok() {
                                    crate::audit_batch::AuditProducer::DispatchSucceeded
                                } else {
                                    crate::audit_batch::AuditProducer::DispatchFailed
                                };
                                let mut storage_event =
                                    build_audit_storage_event(&gate_req, &audit, outcome, resource)
                                        .with_duration_us(dispatch_us);
                                if let Some(metadata) = &mounted_audit {
                                    storage_event.payload["mounted_tool"] = metadata.clone();
                                }
                                append_audit_event_best_effort(
                                    self.audit_batch.as_ref(),
                                    store,
                                    storage_event,
                                    verb,
                                    producer,
                                    self.admission_degrade_safe(verb),
                                )
                                .await
                            }
                        };
                        // Only a would-be-success dispatch can be flipped by
                        // an obligation failure (ADR-133 D2/D3/D4): an
                        // already-erroring dispatch (DispatchFailed producer)
                        // keeps its original error, matching
                        // `fold_audit_obligation`'s contract.
                        result =
                            fold_audit_obligation(result, audit_outcome, std::convert::identity);
                    }
                }

                // Post-dispatch hook: fires on success, opt-in.
                if let (Ok(ref ok_val), Some(hook)) = (&result, &self.dispatch_hook) {
                    let mut dispatch_event = Event::new(
                        ns.as_str(),
                        verb,
                        EventKind::Audit,
                        SubstrateKind::Event,
                        pack.name(),
                    )
                    .with_outcome(EventOutcome::Success)
                    .with_duration_us(dispatch_us);

                    // For recall verbs: extract the first result's id as
                    // target_id so the brain temporal posterior can observe
                    // real hit/miss and latency. Copy the serve-attribution
                    // fields from that same hit so the hook credits the profile
                    // that actually served instead of always crediting default.
                    if verb == "memory.recall" {
                        let first_result =
                            ok_val.as_array().and_then(|arr| arr.first()).or_else(|| {
                                ok_val
                                    .get("results")
                                    .and_then(Value::as_array)
                                    .and_then(|arr| arr.first())
                            });
                        let first_note_id = first_result
                            .and_then(|v| v.get("id"))
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse::<uuid::Uuid>().ok());
                        if let Some(note_id) = first_note_id {
                            dispatch_event = dispatch_event.with_target(note_id);
                        }
                        let mut payload = serde_json::Map::new();
                        if let Some(profile_id) = first_result
                            .and_then(|v| v.get("served_by_profile_id"))
                            .and_then(Value::as_str)
                        {
                            payload.insert(
                                "served_by_profile_id".to_string(),
                                Value::String(profile_id.to_string()),
                            );
                        }
                        if let Some(attribution) = first_result
                            .and_then(|v| v.get("serve_attribution"))
                            .and_then(Value::as_str)
                        {
                            payload.insert(
                                "serve_attribution".to_string(),
                                Value::String(attribution.to_string()),
                            );
                        }
                        dispatch_event = dispatch_event.with_payload(Value::Object(payload));
                        // No first result → target_id stays None (RecallMiss
                        // in brain's event interpreter).
                    }

                    let dispatch_view = EventView {
                        event: dispatch_event,
                        observations: Vec::new(),
                    };
                    let hook = Arc::clone(hook);
                    hook.on_dispatch(&dispatch_view).await;
                }

                // Recently-referenced ring admission: only by-id touches admit
                // an id. Runs unconditionally (not gated on `dispatch_hook`,
                // which is opt-in) because the ring is a core
                // dispatch-boundary capability, not an observer.
                //
                // Keyed on `token.namespace()`, NOT `ns`: `ns` is the
                // gate-resolved namespace, which on the default
                // (non-explicit) dispatch path can be a non-local
                // `default_namespace` (e.g. "foreign") while the storage
                // token that actually created/touched the record is pinned
                // to `local`. The ring must be keyed on the namespace the
                // record actually lives in: the same namespace
                // `resolve_reference`'s ring lookup uses: or admission and
                // lookup silently diverge on any non-local `default_namespace`
                // config.
                if let Ok(ref ok_val) = result {
                    let admissions = crate::reference_ring::ring_admissions_for(verb, ok_val);
                    if !admissions.is_empty() {
                        let actor_key = format!("{}:{}", gate_req.actor.kind, gate_req.actor.id);
                        for (id, name) in admissions {
                            self.reference_ring.admit(
                                token.namespace().as_str(),
                                &actor_key,
                                id,
                                name,
                            );
                        }
                    }
                }

                return result
                    .map_err(|error| DispatchError::after_handler(error, domain_succeeded));
            }
        }

        // No pack owns this verb: the gate allowed it, but no dispatch runs.
        // Persist the deferred audit row now (duration stays at the
        // `Event::new` default of 0 — no dispatch occurred to measure) so an
        // allowed-but-unknown verb is never silently dropped from the audit
        // trail (matches the "no audit row is ever dropped" contract above).
        if let Some(audit) = deferred_audit.take() {
            if let Some(store) = &self.event_store {
                // Dispatch is about to return `UnknownVerb` below (no pack
                // owns this verb), so the persisted outcome must be `Error`,
                // not `Success`. `work_class` is still stamped (ADR-103
                // Decision (a)); `resource.cost_unit` is omitted, matching
                // every other errored-dispatch row.
                let storage_event = build_audit_storage_event(
                    &gate_req,
                    &audit,
                    EventOutcome::Error,
                    Some(crate::cost_unit::base_resource_payload(request_id)),
                );
                // Dispatch already returns `UnknownVerb` below regardless, so
                // — as with the deny paths above — there is no success
                // outcome to fold a commit failure into.
                let _ = append_audit_event_best_effort(
                    self.audit_batch.as_ref(),
                    store,
                    storage_event,
                    verb,
                    crate::audit_batch::AuditProducer::UnknownVerb,
                    false,
                )
                .await;
            }
        }

        // Verb-visibility handler names, precomputed at build() time (internal
        // subhandlers are excluded so they are not advertised in the
        // unknown-verb error).
        Err(DispatchError::before_dispatch(RuntimeError::UnknownVerb(
            format!(
                "unknown verb {verb:?}; available: {}",
                self.available_verbs.join(", ")
            ),
        )))
    }

    /// Dispatch a verb under an out-of-band verified actor identity.
    ///
    /// `verified_actor` is a typed [`VerifiedActor`] (constructor rejects blank
    /// identifiers) — only code holding a `VerbRegistry` handle can supply it.
    /// `dispatch_as` never reads `params["actor"]` to derive the effective actor;
    /// individual verbs may still accept an `actor` field for their own documented
    /// business semantics, unrelated to the acting principal. Every pack handler
    /// that reads "who is calling" resolves it from the `NamespaceToken` the
    /// dispatch boundary mints, so `verified_actor` becomes exactly the principal
    /// those handlers observe.
    ///
    /// Equivalent to `dispatch_with_identity(verb, params, Some(identity))` with
    /// `identity.actor_id = Some(verified_actor)` and every other identity scalar
    /// (namespace, visible namespaces) left at this registry's construction-baked
    /// value. [`Self::dispatch`] and [`Self::dispatch_with_identity`] are unaffected.
    /// See `docs/api/pack.md#dispatch_as` for the embedding-host use case and the
    /// blank-identifier safety rationale.
    pub async fn dispatch_as(
        &self,
        verb: &str,
        params: Value,
        verified_actor: VerifiedActor,
    ) -> Result<Value, RuntimeError> {
        let identity = RequestIdentity {
            namespace: self.default_namespace.clone(),
            actor_id: Some(verified_actor.into_inner()),
            visible_namespaces: self
                .visible_namespaces
                .iter()
                .map(|ns| ns.as_str().to_string())
                .collect(),
            process_ref: crate::config::process_ref_from_env(),
            request_id: None,
        };
        self.dispatch_with_identity(verb, params, Some(identity))
            .await
    }

    /// Registered pack-level by-ID resolvers, in registration order.
    ///
    /// Each element is `(pack_name, resolver)`. The kg `get` and `delete` handlers
    /// iterate this slice to probe pack-private tables when the standard KG
    /// substrates (entity/note/edge/event) return `None` for a given UUID.
    pub fn resolvers(&self) -> &[(String, Box<dyn PackByIdResolver>)] {
        &self.resolvers
    }

    /// The daemon-warm recently-referenced ring (unified-verb draft ADR,
    /// Slice 1). Consumed by `resolve_reference` (Layer 0 stage 2) and by the
    /// `resolve` verb handler; admitted-to by every successful by-id
    /// dispatch (see the admission block in `dispatch_with_identity`).
    pub fn reference_ring(&self) -> &Arc<crate::reference_ring::ReferenceRing> {
        &self.reference_ring
    }

    /// Find a kind hook among the registered packs.
    ///
    /// Walks packs in registration order; the first pack that both owns the
    /// kind (declares it in `note_kinds()` or `entity_kinds()`) and returns
    /// a hook from `kind_hook(kind)` wins. Returns `None` if the kind is
    /// unknown to all packs or no owning pack registered a hook.
    pub fn find_kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        for pack in self.packs.iter() {
            let owns = pack.note_kinds().contains(&kind) || pack.entity_kinds().contains(&kind);
            if owns {
                if let Some(hook) = pack.kind_hook(kind) {
                    return Some(hook);
                }
            }
        }
        None
    }

    /// Every `(entity kind, hook)` pair for which the owning pack declares
    /// the entity kind and registers a `KindHook` — the entity-scoped
    /// subset of [`Self::find_kind_hook`]'s ownership check, computed once.
    ///
    /// `khive-runtime` does not hold a `VerbRegistry` (ownership runs the
    /// other way: packs are constructed FROM a runtime handle), so
    /// `KhiveRuntime::install_entity_kind_hooks` is the extension point
    /// that carries this aggregate to the runtime layer — the transport
    /// calls this after the registry is built, same timing as
    /// [`Self::all_edge_rules`]. `Arc<dyn KindHook>` values returned here
    /// hold no reference back to the pack or registry that produced them
    /// (every production `kind_hook()` implementation constructs a fresh,
    /// stateless hook per call), so installing this aggregate on the
    /// runtime creates no ownership cycle.
    pub fn entity_kind_hooks(&self) -> crate::runtime::EntityKindHooks {
        let mut hooks = Vec::new();
        for pack in self.packs.iter() {
            for kind in pack.entity_kinds().iter().copied() {
                if let Some(hook) = pack.kind_hook(kind) {
                    hooks.push((kind.to_string(), hook));
                }
            }
        }
        hooks
    }

    /// Run the owning kind's shared-note-update normalizer/validator, if it declares one.
    ///
    /// Compatibility wrapper for callers that only need normalization and
    /// validation. Writers use [`Self::prepare_note_update_policy`] and attach
    /// its returned policy so kind-specific property removals reach storage.
    ///
    /// The ordering lives here, at the single dispatch site, rather than in a
    /// [`KindHook`] method a pack could override: a pack implements the two
    /// halves and cannot express a sequence, so it cannot replace the
    /// validator by overriding the sequence. See ADR-017.
    pub async fn prepare_note_update_hook(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &khive_storage::Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        self.prepare_note_update_policy(runtime, token, note, args)
            .await
            .map(|_| ())
    }

    /// Normalize and validate a note update, then carry the owning kind's
    /// property policy into the shared prepared write. Writers must attach the
    /// returned policy to their `NotePatch` or snapshot update preparation;
    /// [`Self::prepare_note_update_hook`] remains the validation-only wrapper.
    pub async fn prepare_note_update_policy(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &khive_storage::Note,
        args: &mut Value,
    ) -> Result<crate::NoteUpdatePolicy, RuntimeError> {
        crate::curation::normalize_note_update_tags(args)?;
        if let Some(hook) = self.find_kind_hook(&note.kind) {
            hook.normalize_note_update(runtime, token, note, args)
                .await?;
            let properties = args.get("properties").filter(|value| !value.is_null());
            hook.validate_note_update(runtime, token, note, properties)
                .await?;
            return Ok(crate::NoteUpdatePolicy::for_kind(
                &note.kind,
                hook.note_update_null_clearing_properties(),
            ));
        }
        Ok(crate::NoteUpdatePolicy::default())
    }

    /// Run the owning kind's shared-note-update property validator, if it
    /// declares one.
    ///
    /// Kept as the validation-only compatibility seam for callers that do not
    /// own a mutable request object. Canonical and atomic CRUD use
    /// [`Self::prepare_note_update_hook`] instead, so a hook's
    /// [`KindHook::normalize_note_update`] can run before its validation does.
    /// Reaching a hook through this seam therefore runs the validator alone:
    /// that is the point of it, and it is why callers that CAN supply a
    /// mutable request should not use it.
    pub async fn validate_note_update_hook(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &khive_storage::Note,
        properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        if let Some(hook) = self.find_kind_hook(&note.kind) {
            hook.validate_note_update(runtime, token, note, properties)
                .await?;
        }
        Ok(())
    }

    /// Run shared-link validators grouped by the owning source-note kind.
    ///
    /// Supplying the whole proposed batch lets a kind hook reject an invariant
    /// violation formed only by multiple entries in that batch. Sources that
    /// are not live notes, or whose kind has no hook, remain the canonical
    /// endpoint validator's responsibility.
    pub async fn validate_link_hooks(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        specs: &[LinkSpec],
    ) -> Result<(), RuntimeError> {
        let mut specs_by_kind: HashMap<String, Vec<LinkSpec>> = HashMap::new();
        for spec in specs {
            let Some(Resolved::Note(source)) = runtime.resolve_by_id(token, spec.source_id).await?
            else {
                continue;
            };
            specs_by_kind
                .entry(source.kind)
                .or_default()
                .push(spec.clone());
        }
        for (kind, kind_specs) in specs_by_kind {
            if let Some(hook) = self.find_kind_hook(&kind) {
                hook.validate_links(runtime, token, &kind_specs).await?;
            }
        }
        Ok(())
    }

    /// Whether any registered pack declares a handler with this verb name.
    ///
    /// A non-dispatch capability check: callers that would otherwise pay a
    /// guaranteed-failed `dispatch` (and its audit write) when an optional
    /// pack is absent can probe first and skip the call entirely.
    pub fn has_verb(&self, verb: &str) -> bool {
        self.handler_by_name.contains_key(verb)
    }

    /// Advisory metadata for synchronous planning and MCP initialization.
    pub fn mounted_verb_snapshot(&self) -> Vec<Value> {
        self.packs
            .iter()
            .flat_map(|pack| {
                pack.mounted_catalog_snapshot()
                    .into_iter()
                    .map(|verb| verb.describe(pack.name()))
            })
            .collect()
    }

    pub async fn mounted_verb_catalog(&self) -> Result<Vec<Value>, RuntimeError> {
        let mut catalog = Vec::new();
        for pack in self.packs.iter() {
            for definition in pack.mounted_catalog().await? {
                catalog.push(definition.describe(pack.name()));
            }
        }
        Ok(catalog)
    }

    /// Apply section evidence through the installed brain instance. Callers must
    /// validate their domain target and authorize their own operation first;
    /// this trusted Rust hook adds no handler to dispatch or the wire catalog.
    pub async fn apply_profile_section_feedback(
        &self,
        token: &NamespaceToken,
        profile_id: &str,
        section_signals: Value,
        target_attribution: Option<String>,
    ) -> Result<Value, RuntimeError> {
        let brain = self
            .packs
            .iter()
            .find(|pack| pack.name() == "brain")
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "profile section feedback requires the brain pack".into(),
                )
            })?;
        brain
            .apply_profile_section_feedback(token, profile_id, section_signals, target_attribution)
            .await
    }

    /// All MCP-exposed handlers across all registered packs (`Visibility::Verb` only).
    ///
    /// Subhandlers (`Visibility::Subhandler`) are excluded — they are internal
    /// pipeline steps not surfaced on the MCP wire. Returned with `'static`
    /// lifetime since pack handlers are `&'static [HandlerDef]` constants.
    pub fn all_verbs(&self) -> Vec<&'static HandlerDef> {
        self.packs
            .iter()
            .flat_map(|p| p.handlers().iter())
            .filter(|h| matches!(h.visibility, Visibility::Verb))
            .collect()
    }

    /// All MCP-exposed handlers paired with the name of the pack that owns them
    /// (`Visibility::Verb` only).
    ///
    /// Subhandlers (`Visibility::Subhandler`) are excluded from the MCP catalog
    /// Use `all_handlers_with_names` when internal handlers must
    /// also be enumerated (e.g. runtime introspection).
    pub fn all_verbs_with_names(&self) -> Vec<(&str, &'static HandlerDef)> {
        self.packs
            .iter()
            .flat_map(|p| p.handlers().iter().map(move |v| (p.name(), v)))
            .filter(|(_, h)| matches!(h.visibility, Visibility::Verb))
            .collect()
    }

    /// All handler definitions across all registered packs, including subhandlers.
    ///
    /// Unlike `all_verbs`, this includes `Visibility::Subhandler` entries. Useful
    /// for runtime introspection (e.g. `list_handlers`) and tooling that needs
    /// the complete handler surface.
    pub fn all_handlers_with_names(&self) -> Vec<(&str, &'static HandlerDef)> {
        self.packs
            .iter()
            .flat_map(|p| p.handlers().iter().map(move |v| (p.name(), v)))
            .collect()
    }

    /// Merged set of note kinds across all registered packs (deduplicated,
    /// first-seen order preserved).
    pub fn all_note_kinds(&self) -> Vec<&'static str> {
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .flat_map(|p| p.note_kinds().iter().copied())
            .filter(|k| seen.insert(*k))
            .collect()
    }

    /// Note kinds owned by a pack, i.e. every kind in [`all_note_kinds`] that
    /// is not one of the generic-CRUD pack's own kinds.
    ///
    /// [`GENERIC_CRUD_PACK`] declares the general-purpose note kinds the shared
    /// CRUD verbs exist to serve (`observation`, `insight`, …); every other
    /// pack's kinds are records that pack's own verbs create and maintain.
    /// Derived from the packs' `NOTE_KINDS` constants, so a pack that adds or
    /// drops a kind moves this set with it — nothing is hardcoded here but the
    /// name of the generic pack itself.
    ///
    /// [`all_note_kinds`]: Self::all_note_kinds
    pub fn pack_owned_note_kinds(&self) -> Vec<&'static str> {
        let generic: std::collections::HashSet<&'static str> = self
            .packs
            .iter()
            .filter(|p| p.name() == GENERIC_CRUD_PACK)
            .flat_map(|p| p.note_kinds().iter().copied())
            .collect();
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .filter(|p| p.name() != GENERIC_CRUD_PACK)
            .flat_map(|p| p.note_kinds().iter().copied())
            .filter(|k| !generic.contains(k) && seen.insert(*k))
            .collect()
    }

    /// Merged set of entity kinds across all registered packs (deduplicated,
    /// first-seen order preserved).
    pub fn all_entity_kinds(&self) -> Vec<&'static str> {
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .flat_map(|p| p.entity_kinds().iter().copied())
            .filter(|k| seen.insert(*k))
            .collect()
    }

    /// Merged set of brain profile consumer kinds requested by registered
    /// packs (deduplicated, first-seen order preserved).
    pub fn all_brain_consumer_kinds(&self) -> Vec<&'static str> {
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .flat_map(|p| p.brain_consumer_kinds().iter().copied())
            .filter(|kind| seen.insert(*kind))
            .collect()
    }

    /// Names of packs in topological load order.
    pub fn pack_names(&self) -> Vec<&str> {
        self.packs.iter().map(|p| p.name()).collect()
    }

    /// Borrow a registered pack's shared host state without reconstructing
    /// that pack. Missing packs, absent state, and type mismatches return None.
    pub fn pack_host_state<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        self.packs
            .iter()
            .find(|pack| pack.name() == name)?
            .host_state()?
            .downcast::<T>()
            .ok()
    }

    /// Declared dependencies for a registered pack.
    pub fn pack_requires(&self, name: &str) -> Option<&'static [&'static str]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.requires())
    }

    /// Note kinds owned by a specific registered pack.
    ///
    /// Returns `None` if no pack with `name` is registered. The slice is
    /// the pack's `NOTE_KINDS` constant — `'static` lifetime, no allocation.
    pub fn pack_note_kinds(&self, name: &str) -> Option<&'static [&'static str]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.note_kinds())
    }

    /// Entity kinds owned by a specific registered pack.
    ///
    /// Returns `None` if no pack with `name` is registered. The slice is
    /// the pack's `ENTITY_KINDS` constant — `'static` lifetime, no allocation.
    pub fn pack_entity_kinds(&self, name: &str) -> Option<&'static [&'static str]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.entity_kinds())
    }

    /// Handlers declared by a specific registered pack.
    ///
    /// Returns `None` if no pack with `name` is registered. Each `HandlerDef`
    /// carries name + description + visibility — sufficient for introspection clients.
    pub fn pack_verbs(&self, name: &str) -> Option<&'static [HandlerDef]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.handlers())
    }

    /// All pack-declared edge endpoint rules across registered packs.
    ///
    /// Order follows topological pack registration; duplicates are *not* deduplicated —
    /// validation only checks membership, and an exact-duplicate rule is a
    /// harmless restatement.
    pub fn all_edge_rules(&self) -> Vec<EdgeEndpointRule> {
        self.packs
            .iter()
            .flat_map(|p| p.edge_rules().iter().copied())
            .collect()
    }

    /// All pack-declared entity-type subtypes across registered packs.
    ///
    /// Order follows topological pack registration; duplicates are *not*
    /// deduplicated here — same posture as [`all_edge_rules`](Self::all_edge_rules).
    /// Consumers compose this with `EntityTypeRegistry::builtin()` via
    /// `EntityTypeRegistry::with_extra` to get the boot-time composed registry.
    pub fn all_entity_types(&self) -> Vec<EntityTypeDef> {
        self.packs
            .iter()
            .flat_map(|p| p.entity_types().iter().cloned())
            .collect()
    }

    /// Collect all `NoteKindSpec` declarations from every loaded pack.
    ///
    /// Used by the runtime for lifecycle introspection and future enforcement.
    pub fn all_note_kind_specs(&self) -> Vec<&'static NoteKindSpec> {
        self.packs
            .iter()
            .flat_map(|p| p.note_kind_specs().iter())
            .collect()
    }

    /// Collect pack-declared embedding policies for registered note kinds.
    pub fn all_note_embedding_policies(&self) -> Vec<NoteEmbeddingPolicySpec> {
        self.packs
            .iter()
            .flat_map(|pack| pack.note_embedding_policies().iter().copied())
            .collect()
    }

    /// All pack-contributed validation rules across registered packs.
    ///
    /// Returns references into the pack-owned `'static` slices — no allocation
    /// beyond the outer `Vec`. Rule IDs are namespaced by pack; callers can
    /// group by `rule.id.split_once('/')` to attribute rules to their packs.
    pub fn all_validation_rules(&self) -> Vec<&'static ValidationRule> {
        self.packs
            .iter()
            .flat_map(|p| p.validation_rules().iter())
            .collect()
    }

    /// Pack-auxiliary schema plans for all registered packs.
    ///
    /// Returns one `SchemaPlan` per pack. Callers (typically the runtime
    /// bootstrap) apply each plan to the pack's assigned backend. Empty plans
    /// are included so the caller can iterate uniformly; callers that want to
    /// skip empty plans should check `plan.is_empty()`. Schema application must
    /// use [`Self::all_schema_plans_with_columns`] to retain column upgrades.
    pub fn all_schema_plans(&self) -> Vec<SchemaPlan> {
        self.packs.iter().map(|p| p.schema_plan()).collect()
    }

    /// Schema plans paired with the same owning pack's nullable-column upgrades.
    ///
    /// Callers applying plans directly must pass both entries to
    /// `StorageBackend::apply_pack_ddl_statements_with_columns`.
    pub fn all_schema_plans_with_columns(
        &self,
    ) -> Vec<(SchemaPlan, &'static [PackColumnAddition])> {
        self.packs
            .iter()
            .map(|pack| (pack.schema_plan(), pack.schema_column_additions()))
            .collect()
    }

    /// Invoke `PackRuntime::register_embedders` on every registered pack.
    ///
    /// Called by the transport during startup, after the registry is built and
    /// before the first verb dispatch, so that custom embedding providers
    /// contributed by packs are reachable via `KhiveRuntime::embedder(name)`.
    ///
    /// Packs whose `register_embedders` is the default no-op pay no overhead.
    /// The method is idempotent when the underlying registry uses last-wins
    /// semantics for duplicate provider names.
    pub fn call_register_embedders(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_embedders(runtime);
        }
    }

    /// Invoke `PackRuntime::register_entity_type_validator` on every registered pack.
    ///
    /// Called by the transport during startup, after the registry is built and
    /// before the first verb dispatch, so that entity-type validation at the
    /// runtime layer is active for all write paths including direct `create_many`
    /// callers that bypass the handler layer.
    ///
    /// Packs whose `register_entity_type_validator` is the default no-op pay
    /// no overhead.
    ///
    /// Composes [`all_entity_types`](Self::all_entity_types) once and passes
    /// the same aggregate to every pack, mirroring how `install_edge_rules`
    /// installs one `all_edge_rules()` aggregate for the whole registry.
    pub fn call_register_entity_type_validators(&self, runtime: &KhiveRuntime) {
        let entity_types = self.all_entity_types();
        for pack in self.packs.iter() {
            pack.register_entity_type_validator_with_types(runtime, &entity_types);
        }
    }

    /// Invoke `PackRuntime::register_note_mutation_hook` on every registered pack.
    ///
    /// Called by the transport during startup, after the registry is built and
    /// before the first verb dispatch, so that note-mutation notifications at
    /// the runtime layer are active for all write paths — including KG's
    /// `update`/`delete` verbs reaching a `kind="memory"` note, which have no
    /// crate-level dependency on `khive-pack-memory`.
    ///
    /// Packs whose `register_note_mutation_hook` is the default no-op pay no
    /// overhead.
    pub fn call_register_note_mutation_hooks(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_note_mutation_hook(runtime);
        }
    }

    /// Install pack-owned note-search candidate sources before warm-up or
    /// dispatch, following the same registration timing as mutation hooks.
    pub fn call_register_note_search_ann_providers(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_note_search_ann_provider(runtime);
        }
    }

    /// Invoke `PackRuntime::register_note_write_validator` on every registered pack.
    ///
    /// Called by the transport during startup with the same timing as
    /// `call_register_note_mutation_hooks`, so note-write validation is active
    /// at the runtime layer for every write path — the generic `create` verb,
    /// direct Rust callers, and proposal apply, none of which dispatch a pack
    /// hook of their own on the note-write.
    pub fn call_register_note_write_validators(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_note_write_validator(runtime);
        }
    }

    /// Invoke `PackRuntime::warm` on every registered pack.
    /// Called by the daemon at boot (in a background task) so expensive in-memory
    /// state (ANN indexes) is pre-loaded without blocking request serving.
    pub async fn call_warm_all(&self) {
        for pack in self.packs.iter() {
            pack.warm().await;
        }
    }

    /// Resolve the presentation policy for a verb name.
    ///
    /// Uses the first registered handler (including subhandlers) with this name
    /// and returns its declared [`VerbPresentationPolicy`].
    /// Returns `Standard` for unknown verbs — unknown verbs will fail at
    /// dispatch anyway, so the fallback here is safe.
    pub fn presentation_policy_for(&self, verb: &str) -> khive_types::VerbPresentationPolicy {
        self.handler_by_name
            .get(verb)
            .map_or(khive_types::VerbPresentationPolicy::Standard, |handler| {
                handler.presentation_policy()
            })
    }

    /// Resolve the declared [`VerbCategory`] for a verb name.
    ///
    /// Uses the first registered handler (including subhandlers) with this name
    /// and returns its speech-act category. Returns `None` for
    /// an unregistered verb name, so a caller deciding transport-level
    /// behavior (e.g. whether a post-dispatch condition is safe to retry)
    /// can fail closed on an unknown verb instead of guessing a category.
    pub fn verb_category(&self, verb: &str) -> Option<VerbCategory> {
        self.handler_by_name
            .get(verb)
            .map(|handler| handler.category)
    }

    /// Verbs classified [`VerbCategory::Assertive`] that nonetheless schedule
    /// can schedule a persisted write on a successful dispatch, so a caller re-issuing
    /// a call in this list after a lost response duplicates that write:
    ///
    /// - `memory.recall` schedules `brain.record_serve`, which inserts a
    ///   serve-ledger row keyed in part on a `served_at` timestamp captured
    ///   fresh at dispatch time — a second dispatch inserts a second row
    ///   rather than colliding with the first.
    /// - `search` (the `kg` pack's bare verb) appends a `search_executed`
    ///   event with a freshly generated id and no natural key at all.
    /// - `telemetry.emit` can append a durable stream record with a fresh
    ///   identity and sequence, depending on the configured channel policy.
    /// - `tool.check` appends a `tool_check_decided` receipt with a fresh
    ///   event id for every evaluated decision (ADR-180 Amendment 6).
    ///
    /// The speech-act category alone cannot rule this out — it describes
    /// what the verb tells the *caller*, not what it schedules against
    /// storage. Adding a verb here (or removing one because its side effect
    /// was made idempotent) is a correctness decision requiring the same
    /// scrutiny as the categorization itself.
    pub const SIDE_EFFECTING_ASSERTIVE_VERBS: &'static [&'static str] =
        &["memory.recall", "search", "telemetry.emit", "tool.check"];

    /// Whether a response lost to the daemon frame budget may be truthfully
    /// advertised as safe to re-issue: the verb is [`VerbCategory::Assertive`]
    /// (no institutional commitment was made) and is not on
    /// `Self::SIDE_EFFECTING_ASSERTIVE_VERBS` (no persisted write to
    /// duplicate on a second dispatch). An unregistered verb name resolves to
    /// `None` from [`Self::verb_category`] and fails closed here.
    ///
    /// Used only by the MCP daemon's frame-budget omission decision; never
    /// for permission checking or return-shape selection.
    pub fn is_retry_safe_after_frame_omission(&self, verb: &str) -> bool {
        matches!(self.verb_category(verb), Some(VerbCategory::Assertive))
            && !Self::SIDE_EFFECTING_ASSERTIVE_VERBS.contains(&verb)
    }

    /// Returns `true` if the named verb exists and is tagged
    /// `Visibility::Subhandler` (internal / operator-only).
    ///
    /// Used by the MCP server to gate subhandler invocation at the wire
    /// boundary without blocking internal callers that invoke the same verbs
    /// through the runtime directly.
    pub fn is_subhandler_verb(&self, verb: &str) -> bool {
        self.handler_by_name
            .get(verb)
            .is_some_and(|handler| matches!(handler.visibility, Visibility::Subhandler))
    }

    /// Apply all non-empty pack-auxiliary schema plans to the given backend.
    ///
    /// This is the centralized startup hook that replaced the previous lazy
    /// per-pack self-bootstrap pattern. Each pack's `SchemaPlan` carries
    /// idempotent `CREATE TABLE IF NOT EXISTS` DDL; calling this more than once
    /// is safe. Plans with neither SQL nor column upgrades are skipped.
    ///
    /// Errors from individual plans are logged via `tracing::warn!` and not
    /// propagated so that a single pack's schema failure does not prevent the
    /// rest from loading. Serving hosts must instead use the fallible
    /// [`Self::apply_schema_plans_with_map`] (with an empty map for one backend)
    /// so a required schema failure cannot leave a pack's verbs unavailable.
    pub fn apply_schema_plans(&self, backend: &khive_db::StorageBackend) {
        if backend.is_read_only() {
            tracing::info!(
                "skipping pack schema plans because the backend is read-only; snapshot schema is used as-is"
            );
            return;
        }
        for (plan, additions) in self.all_schema_plans_with_columns() {
            if plan.is_empty() && additions.is_empty() {
                continue;
            }
            if let Err(e) =
                backend.apply_pack_ddl_statements_with_columns(plan.statements, additions)
            {
                tracing::warn!(
                    pack = plan.pack,
                    error = %e,
                    "failed to apply pack schema plan at startup (non-fatal)"
                );
            }
        }
    }

    /// Pack-auxiliary schema plans with their owning pack names.
    ///
    /// Returns `(pack_name, SchemaPlan)` pairs for every registered pack.
    /// Used by the multi-backend boot path to apply each plan to the pack's
    /// assigned backend rather than a single shared backend. Direct schema
    /// application must use [`Self::all_schema_plans_with_columns`] so column
    /// upgrades are retained.
    pub fn all_schema_plans_named(&self) -> Vec<(&'static str, SchemaPlan)> {
        self.packs
            .iter()
            .map(|p| {
                let plan = p.schema_plan();
                (plan.pack, plan)
            })
            .collect()
    }

    /// Apply pack-auxiliary schema plans using a per-pack backend map.
    ///
    /// For each plan and its owning pack's column additions, applies the full
    /// plan to `backend_for_pack[plan.pack]` when present,
    /// falling back to `default_backend` for any pack not in the map.
    ///
    /// Returns an error when two packs on the same backend declare the same
    /// auxiliary table (ADR-028 §7 collision policy: boot failure naming both
    /// packs and the conflicting table).
    ///
    /// Both single- and multi-backend hosts use this boot path (ADR-028).
    /// An empty map selects the default backend for every pack. Read-only
    /// backends validate declared columns without applying SQL or acquiring a
    /// writer; missing or incompatible columns refuse boot with the pack name.
    pub fn apply_schema_plans_with_map(
        &self,
        backend_for_pack: &HashMap<&str, &khive_db::StorageBackend>,
        default_backend: &khive_db::StorageBackend,
    ) -> Result<(), crate::PackSchemaCollisionError> {
        // Track which pack first claimed each table on each backend.
        // Backend identity is the raw pointer of the underlying connection pool Arc.
        let mut claimed: HashMap<(*const (), String), &'static str> = HashMap::new();

        let plans = self.all_schema_plans_with_columns();
        // Check every declaration before applying any pack DDL. A collision
        // must not leave earlier plans installed on a failed boot.
        for (plan, additions) in &plans {
            if plan.is_empty() && additions.is_empty() {
                continue;
            }
            let pack_name = plan.pack;
            let backend = backend_for_pack
                .get(pack_name)
                .copied()
                .unwrap_or(default_backend);
            let backend_ptr = std::sync::Arc::as_ptr(&backend.pool_arc()) as *const ();

            // Collect DDL table ownership for the full plan set.
            for stmt in plan.statements {
                for table_name in extract_table_names(stmt) {
                    let key = (backend_ptr, table_name.clone());
                    match claimed.entry(key) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(pack_name);
                        }
                        std::collections::hash_map::Entry::Occupied(e) => {
                            let prior_pack = *e.get();
                            return Err(crate::PackSchemaCollisionError {
                                pack_a: prior_pack,
                                pack_b: pack_name,
                                table: table_name,
                            });
                        }
                    }
                }
            }
            for addition in *additions {
                let table_name = addition.table.to_ascii_lowercase();
                let key = (backend_ptr, table_name.clone());
                match claimed.entry(key) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(pack_name);
                    }
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        let prior_pack = *entry.get();
                        // A pack's full CREATE and its upgrades declare the
                        // same table; this is one ownership claim.
                        if prior_pack != pack_name {
                            return Err(crate::PackSchemaCollisionError {
                                pack_a: prior_pack,
                                pack_b: pack_name,
                                table: table_name,
                            });
                        }
                    }
                }
            }
        }

        for (plan, additions) in plans {
            if plan.is_empty() && additions.is_empty() {
                continue;
            }
            let pack_name = plan.pack;
            let backend = backend_for_pack
                .get(pack_name)
                .copied()
                .unwrap_or(default_backend);
            if backend.is_read_only() {
                backend.validate_pack_schema_columns(additions).map_err(|error| {
                    crate::PackSchemaCollisionError {
                        pack_a: pack_name,
                        pack_b: pack_name,
                        table: format!("read-only schema validation failed: {error}; open the database writable to apply the pack schema upgrade"),
                    }
                })?;
                continue;
            }

            backend
                .apply_pack_ddl_statements_with_columns(plan.statements, additions)
                .map_err(|e| crate::PackSchemaCollisionError {
                    pack_a: pack_name,
                    pack_b: pack_name,
                    table: format!("DDL error: {e}"),
                })?;
        }
        Ok(())
    }
}

// ── Inventory-based dynamic pack loading ────────────────────────────────────

/// Output of [`PackFactory::create_install`] — bundles the pack runtime with
/// its optional by-ID resolver and dispatch hook so a factory can hand back
/// all three built from one shared instance (see `BrainPackFactory` for why
/// this matters: the dispatch hook must observe the same state the runtime
/// mutates, not a second unrelated instance).
pub struct PackInstall {
    /// The pack runtime, registered into the builder's pack list.
    pub runtime: Box<dyn PackRuntime>,
    /// Optional by-ID resolver, registered when present.
    pub resolver: Option<Box<dyn PackByIdResolver>>,
    /// Optional post-dispatch observer, wired via `VerbRegistryBuilder::with_dispatch_hook`.
    pub dispatch_hook: Option<Arc<dyn DispatchHook>>,
}

/// Factory for creating pack instances registered via `inventory` at link time.
/// Each pack crate submits a `&'static dyn PackFactory` wrapped in a
/// [`PackRegistration`]; the binary's linker collects them all into a single
/// slice iterable at runtime.
///
/// Implementors must be `Send + Sync + 'static` because the registry is built
/// once and shared across async tasks.
/// Possession-bounded capability for the trusted channel-ingest note path.
///
/// Constructible only inside `khive-runtime` (the field is private), and
/// granted during pack registration exclusively to factories named in
/// `CHANNEL_INGEST_CAPABLE_PACKS`. Every call to
/// [`crate::KhiveRuntime::try_create_note_as_trusted_ingest`] must present a
/// reference to one, so the set of callers able to establish transport-owned
/// message properties is bounded by possession at the composition root, not
/// by a documentation prohibition. Two ways to obtain one: registering
/// through [`PackRegistry::register_packs`]/`register_packs_with_runtimes`
/// under the `comm` name (the allowlisted, automatic path), or a composition
/// root that builds packs directly calling
/// [`ChannelIngestCapability::grant_for_direct_composition`] and passing the
/// result to [`crate::PackRuntime::accept_channel_ingest_capability`] (or a
/// pack's constructor variant that does so) itself. The residual trust
/// assumption is unchanged either way: whoever assembles the
/// `VerbRegistryBuilder` already decides which packs are wired in and already
/// holds a `KhiveRuntime`, so minting the grant explicitly carries no more
/// privilege than that composition already had by choosing to register `comm`
/// at all.
pub struct ChannelIngestCapability {
    pub(crate) _sealed: (),
}

impl ChannelIngestCapability {
    /// Mint a capability for a composition root that constructs
    /// channel-transport packs directly, bypassing
    /// [`PackRegistry::register_packs`] (which grants this automatically).
    ///
    /// See the type-level doc for the trust argument: this carries no more
    /// privilege than the caller already has by virtue of holding a
    /// `KhiveRuntime` and choosing to wire the pack in.
    pub fn grant_for_direct_composition() -> Self {
        Self { _sealed: () }
    }
}

/// Pack names entitled to a [`ChannelIngestCapability`] grant at registration.
pub(crate) const CHANNEL_INGEST_CAPABLE_PACKS: &[&str] = &["comm"];

pub trait PackFactory: Send + Sync + 'static {
    /// Canonical lowercase name for this pack (e.g. `"kg"`, `"gtd"`).
    fn name(&self) -> &'static str;

    /// Names of packs that must be loaded before this one.
    ///
    /// Defaults to empty so pack crates that have no dependencies compile
    /// without changes. [`PackRegistry::register_packs`] validates that every
    /// name listed here is present in the caller's explicit pack list — absent
    /// dependencies are a boot error, not silently auto-added.
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether this pack intentionally exposes no top-level MCP verbs.
    ///
    /// Defaults to `false` so a declared pack whose runtime contributes no
    /// [`Visibility::Verb`] handlers fails at registration instead of silently
    /// disappearing from the served surface. Vocabulary- or ontology-only
    /// packs must opt in explicitly.
    fn intentionally_verbless(&self) -> bool {
        false
    }

    /// Create a new pack instance for the given runtime.
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime>;

    /// Build the full installation bundle for this pack: runtime, optional
    /// resolver, optional dispatch hook.
    ///
    /// Defaults to composing `create` and `create_resolver` with no dispatch
    /// hook, so existing factories compile unchanged. Packs whose dispatch
    /// hook must observe the same instance as the runtime (e.g. `brain`)
    /// override this method instead of `create`, since the default would
    /// otherwise require two independent instances to share state.
    fn create_install(&self, runtime: KhiveRuntime) -> PackInstall {
        let resolver = self.create_resolver(runtime.clone());
        PackInstall {
            runtime: self.create(runtime),
            resolver,
            dispatch_hook: None,
        }
    }

    /// Optionally create a `PackByIdResolver` for this pack.
    ///
    /// Packs that own private SQL tables implement this to hook into
    /// `get(id)` and `delete(id)`. Defaults to `None` so existing packs
    /// compile without changes.
    fn create_resolver(&self, _runtime: KhiveRuntime) -> Option<Box<dyn PackByIdResolver>> {
        None
    }
}

/// Newtype wrapper collected by `inventory` so pack crates can submit
/// `&'static dyn PackFactory` references without the type-ascription syntax
/// that `inventory::submit!` does not support for bare trait-object references.
pub struct PackRegistration(pub &'static dyn PackFactory);

inventory::collect!(PackRegistration);

/// Error returned by [`PackRegistry::register_packs`] when boot validation fails.
#[derive(Debug)]
pub enum PackLoadError {
    /// The requested pack name was not found in the inventory.
    UnknownPack(String),
    /// The requested pack name occurs more than once.
    DuplicatePack(String),
    /// A pack was requested but a declared dependency is absent from the list.
    MissingDependency {
        /// The pack that declared the dependency.
        pack: String,
        /// The dependency that is missing from the requested pack list.
        dep: String,
    },
    /// A declared pack contributed no top-level verbs without explicitly
    /// declaring itself vocabulary/ontology-only.
    NoPublicVerbs {
        /// The declared pack name.
        pack: String,
    },
}

impl std::fmt::Display for PackLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackLoadError::UnknownPack(name) => write!(f, "unknown pack {name:?}"),
            PackLoadError::DuplicatePack(name) => write!(f, "duplicate pack {name:?}"),
            PackLoadError::MissingDependency { pack, dep } => write!(
                f,
                "pack {pack:?} requires {dep:?}, which is not in the requested pack list; \
                 add --pack {dep} before --pack {pack}"
            ),
            PackLoadError::NoPublicVerbs { pack } => write!(
                f,
                "declared pack {pack:?} registers no public verbs; if this pack is \
                 intentionally vocabulary- or ontology-only, its factory must declare \
                 intentionally_verbless() = true"
            ),
        }
    }
}

impl std::error::Error for PackLoadError {}

/// Reject a declared pack whose runtime contributes no [`Visibility::Verb`]
/// handlers unless its factory explicitly opts out via
/// [`PackFactory::intentionally_verbless`].
fn check_pack_has_public_verbs(
    factory: &dyn PackFactory,
    install: &PackInstall,
    name: &str,
) -> Result<(), PackLoadError> {
    if !factory.intentionally_verbless()
        && !install
            .runtime
            .handlers()
            .iter()
            .any(|handler| matches!(handler.visibility, Visibility::Verb))
    {
        return Err(PackLoadError::NoPublicVerbs {
            pack: name.to_string(),
        });
    }
    Ok(())
}

/// Registry of pack factories discovered via `inventory` at link time.
///
/// No instance is needed — all methods are associated functions that walk the
/// globally-collected [`PackRegistration`] slice.
pub struct PackRegistry;

/// Whether [`PackRegistry::build_ingest_registry`] attaches the runtime's
/// event store to the registry it builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestAuditStore {
    /// Mirror `KhiveMcpServer::with_packs` (`khive-mcp/src/server.rs`): a
    /// writable runtime attaches its own event store and refuses to build if
    /// sink initialization fails; a read-only runtime retains no `EventStore`
    /// handle and an advisory travels beside each result instead.
    Attach,
    /// Build the registry with no audit event store, for a caller with no use
    /// for persisted audit rows.
    Detach,
}

impl PackRegistry {
    /// Names of all pack factories discovered via `inventory`.
    pub fn discovered_names() -> Vec<&'static str> {
        inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0.name())
            .collect()
    }

    /// Validate linked pack names and explicit dependencies without creating
    /// runtimes, opening stores, or constructing pack instances.
    ///
    /// Launchers can use this before publishing ownership. Registration uses
    /// the same validation, including when extra factories are supplied.
    pub fn validate_pack_selection(names: &[String]) -> Result<(), PackLoadError> {
        let all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0)
            .collect();
        Self::validate_pack_selection_from(&all, names)
    }

    fn validate_pack_selection_from(
        factories: &[&'static dyn PackFactory],
        names: &[String],
    ) -> Result<(), PackLoadError> {
        let factory_for = |name: &str| factories.iter().copied().find(|f| f.name() == name);
        let mut requested = std::collections::HashSet::new();
        for name in names {
            factory_for(name).ok_or_else(|| PackLoadError::UnknownPack(name.clone()))?;
            if !requested.insert(name.as_str()) {
                return Err(PackLoadError::DuplicatePack(name.clone()));
            }
        }
        for name in names {
            let factory = factory_for(name).unwrap(); // All names were validated above.
            for &dep in factory.requires() {
                if !requested.contains(dep) {
                    return Err(PackLoadError::MissingDependency {
                        pack: name.clone(),
                        dep: dep.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Register the named packs into `builder` using the supplied `runtime`.
    ///
    /// Validates the explicit pack list against `PackFactory::requires()` —
    /// if any requested pack declares a dependency that is absent from `names`,
    /// registration fails (missing dependency is a boot error, not silently
    /// auto-added). Callers must include all required packs explicitly.
    ///
    /// The [`VerbRegistryBuilder::build`] topo-sort enforces correct load order.
    ///
    /// Returns `Ok(())` when all names are recognised and all declared
    /// dependencies are satisfied; returns `Err(PackLoadError)` with a
    /// distinct variants for unknown or duplicate packs and missing dependencies.
    pub fn register_packs(
        names: &[String],
        runtime: KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        // Build a name→factory index once.
        let all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0)
            .collect();
        let factory_for = |name: &str| -> Option<&'static dyn PackFactory> {
            all.iter().copied().find(|f| f.name() == name)
        };

        Self::validate_pack_selection_from(&all, names)?;

        // Register every requested pack; VerbRegistryBuilder::build()
        // performs the topo-sort, so insertion order here does not matter.
        for name in names {
            let factory = factory_for(name.as_str()).unwrap(); // validated above
            let install = factory.create_install(runtime.clone());
            check_pack_has_public_verbs(factory, &install, name)?;
            if CHANNEL_INGEST_CAPABLE_PACKS.contains(&name.as_str()) {
                install
                    .runtime
                    .accept_channel_ingest_capability(ChannelIngestCapability { _sealed: () });
            }
            builder.register_boxed(install.runtime);
            if let Some(resolver) = install.resolver {
                builder.register_resolver(name.clone(), resolver);
            }
            if let Some(hook) = install.dispatch_hook {
                builder.with_dispatch_hook(hook);
            }
        }

        Ok(())
    }

    /// Build a `VerbRegistry` from `runtime`'s own configuration: gate,
    /// default namespace, visible namespaces, actor id, and the configured
    /// pack set, then install the registry's aggregated edge rules back onto
    /// `runtime`. This is the wiring shared by every one-shot CLI ingest path
    /// (`kkernel code-ingest`, `kkernel git-ingest`) that needs a real
    /// registry to dispatch through outside of a live MCP server.
    ///
    /// `audit_store` selects whether the registry gets the runtime's event
    /// store; see [`IngestAuditStore`] for what each variant does.
    ///
    /// This helper carries only the subset every ingest path duplicated
    /// verbatim. The MCP server's own registry construction additionally
    /// wires channel-loop admission, `config_id`, embedder/entity-type/
    /// note-mutation-hook registration, schema-plan application, and the WAL
    /// checkpoint pool handle — all server-only concerns a one-shot CLI pass
    /// has no use for, so `KhiveMcpServer::with_packs` keeps its own
    /// construction rather than calling this helper.
    pub fn build_ingest_registry(
        runtime: &KhiveRuntime,
        audit_store: IngestAuditStore,
    ) -> Result<VerbRegistry, RuntimeError> {
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(runtime.config().gate.clone());
        builder.with_default_namespace(runtime.config().default_namespace.as_str());
        builder.with_visible_namespaces(runtime.config().visible_namespaces.clone());
        builder.with_actor_id(runtime.config().actor_id.clone());
        if audit_store == IngestAuditStore::Attach {
            if runtime.is_read_only() {
                builder.with_read_only_audit_store();
            } else {
                // Attach requires a usable sink; build propagates open failures.
                builder.with_runtime_event_store(runtime)?;
            }
        }
        Self::register_packs(
            &runtime.config().packs.clone(),
            runtime.clone(),
            &mut builder,
        )
        .map_err(|e| RuntimeError::Internal(format!("pack registration failed: {e:?}")))?;
        let registry = builder.build()?;
        runtime.install_edge_rules(registry.all_edge_rules());
        Ok(registry)
    }

    /// Register the named packs into `builder`, routing each pack to its own runtime.
    ///
    /// `runtimes` maps pack name → `KhiveRuntime` (one per backend assignment).
    /// `default_runtime` is used for any pack whose name is not in `runtimes`.
    /// The validation logic (unknown pack, missing dependency) is identical to
    /// [`PackRegistry::register_packs`].
    ///
    /// This is the multi-backend boot path (ADR-028). Single-backend callers
    /// should continue using [`PackRegistry::register_packs`].
    pub fn register_packs_with_runtimes(
        names: &[String],
        runtimes: &HashMap<String, KhiveRuntime>,
        default_runtime: &KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        let all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0)
            .collect();
        Self::register_packs_with_runtimes_from(&all, names, runtimes, default_runtime, builder)
    }

    /// Like [`Self::register_packs_with_runtimes`], but resolves pack names
    /// against the link-time `inventory` registry **plus** `extra_factories` —
    /// pack factories the composition root supplies directly rather than
    /// discovers through `inventory::iter::<PackRegistration>` (ADR-191 D6,
    /// ADR-192 S4: "a pack compiled outside this repository ... extends the
    /// web ontology without any change here" — a host binary that depends on
    /// a pinned khive revision plus an out-of-tree pack crate, or a
    /// composition root registering a credential-provider/request-hook
    /// consumer pack, has no `inventory` presence in *this* binary short of
    /// its own force-link anchor). An inventory-discovered factory always
    /// wins a name collision with an `extra_factories` entry — the linked set
    /// is the trusted default; an extra factory only fills a name inventory
    /// does not already answer.
    ///
    /// This is the seam D6 describes as "kkernel exposes its server
    /// construction as a library entry point that accepts additional pack
    /// factories" — the `kkernel` library entry point itself lives in
    /// `kkernel::compose`, built on this function exactly as
    /// `khive-mcp/src/serve.rs` builds on [`Self::register_packs_with_runtimes`].
    pub fn register_packs_with_runtimes_with_extra_factories(
        extra_factories: &[&'static dyn PackFactory],
        names: &[String],
        runtimes: &HashMap<String, KhiveRuntime>,
        default_runtime: &KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        let mut all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0)
            .collect();
        all.extend(extra_factories.iter().copied());
        Self::register_packs_with_runtimes_from(&all, names, runtimes, default_runtime, builder)
    }

    /// Shared body for [`Self::register_packs_with_runtimes`] and
    /// [`Self::register_packs_with_runtimes_with_extra_factories`]: both
    /// build a `factories` index (inventory-only, or inventory-plus-extra)
    /// and delegate here. `factory_for` resolves by first match, so a
    /// duplicate name earlier in `factories` wins over a later one — the two
    /// public callers above rely on that for their stated collision rule.
    fn register_packs_with_runtimes_from(
        factories: &[&'static dyn PackFactory],
        names: &[String],
        runtimes: &HashMap<String, KhiveRuntime>,
        default_runtime: &KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        let factory_for = |name: &str| -> Option<&'static dyn PackFactory> {
            factories.iter().copied().find(|f| f.name() == name)
        };

        Self::validate_pack_selection_from(factories, names)?;

        builder.kg_read_resolver = Some(Arc::new(crate::kg_read::KgReadResolver::new(
            default_runtime,
            runtimes,
        )));

        for name in names {
            let factory = factory_for(name.as_str()).unwrap();
            let runtime = runtimes
                .get(name.as_str())
                .cloned()
                .unwrap_or_else(|| default_runtime.clone());
            let install = factory.create_install(runtime);
            check_pack_has_public_verbs(factory, &install, name)?;
            if CHANNEL_INGEST_CAPABLE_PACKS.contains(&name.as_str()) {
                install
                    .runtime
                    .accept_channel_ingest_capability(ChannelIngestCapability { _sealed: () });
            }
            builder.register_boxed(install.runtime);
            if let Some(resolver) = install.resolver {
                builder.register_resolver(name.clone(), resolver);
            }
            if let Some(hook) = install.dispatch_hook {
                builder.with_dispatch_hook(hook);
            }
        }

        Ok(())
    }
}

/// Audit target in the submitted args; only `link` also accepts `target` for `target_id`.
fn target_id_from_args(verb: &str, args: &serde_json::Value) -> Option<uuid::Uuid> {
    let alias = args.get("target").filter(|_| verb == "link");
    args.get("target_id")
        .or(alias)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
}

/// Build the [`AuditEvent`] for one gate check, masking `deny_reason` before
/// it can reach either downstream sink.
///
/// `deny_reason` is gate-authored text this crate does not control: a custom
/// `Gate` implementation (a Rego policy, an external backend) can echo
/// request content into why it denied, so the same secret-detection pass
/// applied to backend error text elsewhere in this file also has to run on a
/// denial's stated reason. This can't live on [`AuditEvent`] itself —
/// `khive-gate` cannot depend on `khive-runtime`'s masking, which itself
/// depends on `khive-gate` (see `khive-runtime/Cargo.toml`); a masker inside
/// `AuditEvent::from_check` would be a dependency cycle. So masking happens
/// once, here, immediately after construction and before the event is used
/// anywhere: every call site that turns a [`GateDecision`] into an
/// [`AuditEvent`] must go through this function, never `AuditEvent::from_check`
/// directly, so the `gate.check` tracing line and the row
/// [`build_audit_storage_event`] re-serializes for the event store always see
/// the same masked value rather than each needing its own redaction.
fn masked_audit_event(
    gate_req: &GateRequest,
    decision: &GateDecision,
    gate_impl: &str,
) -> AuditEvent {
    let mut audit = AuditEvent::from_check(gate_req, decision, gate_impl)
        .with_operation_attribution(
            khive_storage::operation_context::current_operation_attribution(),
        );
    if let Some(reason) = audit.deny_reason.take() {
        audit.deny_reason = Some(crate::secret_gate::bounded_masked_log_text(&reason));
    }
    audit
}

/// Build a v1-shape audit storage event from a gate check outcome.
/// See `docs/api/pack.md#build_audit_storage_event` for the `resource` payload contract.
fn build_audit_storage_event(
    gate_req: &GateRequest,
    audit: &AuditEvent,
    outcome: EventOutcome,
    resource: Option<Value>,
) -> Event {
    let mut audit_data = serde_json::to_value(audit).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to serialize AuditEvent for EventStore");
        serde_json::Value::Null
    });
    if let Some(resource) = resource {
        if let Value::Object(ref mut map) = audit_data {
            map.insert("resource".to_string(), resource);
        }
    }
    let mut storage_event = Event::new(
        gate_req.namespace.as_str(),
        gate_req.verb.as_str(),
        EventKind::Audit,
        SubstrateKind::Event,
        format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
    )
    .with_outcome(outcome)
    .with_payload(audit_data);
    storage_event.op_index = audit.op_index;
    storage_event.ref_resolution = audit.ref_resolution;
    if let Some(target_id) = target_id_from_args(&gate_req.verb, &gate_req.args) {
        storage_event = storage_event.with_target(target_id);
    }
    storage_event
}

/// Process-wide pure-observability audit appends whose errors were logged
/// and swallowed — never an obligation-bearing row, which fails its dispatch
/// instead and is counted separately by
/// [`AUDIT_OBLIGATION_APPEND_FAILURES`]/[`audit_obligation_append_failure_count`].
/// Keeping this counter obligation-free preserves its documented contract
/// (`docs/guide/api-reference.md`, `khive-db`'s `WriterContentionDiagnostics::audit_append_failures`
/// doc comment): every unit counted here was swallowed, none was propagated.
static AUDIT_APPEND_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn audit_append_failure_count() -> u64 {
    AUDIT_APPEND_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Process-wide commit failures for obligation-bearing audit rows (ADR-133
/// D2/D3/D4): gate denials, dispatch outcomes, unknown-verb rows, and
/// `git.digest` success receipts. Most call sites fold this failure into the
/// dispatch's own error (a would-be success becomes an error, per
/// [`fold_audit_obligation`]); a denial's own audit row is the one
/// exception — its dispatch already returns `PermissionDenied` independent
/// of whether this row commits, so the failure is logged and counted here
/// but not separately propagated. Disjoint from [`AUDIT_APPEND_FAILURES`] —
/// each failing row is classified by [`crate::audit_batch::classify`] into
/// exactly one of the two classes and increments exactly one of these two
/// counters, never both.
static AUDIT_OBLIGATION_APPEND_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Runtime diagnostics exposes this process-wide counter separately from
/// swallowed audit errors and batch-generation failures (#2784).
pub(crate) fn audit_obligation_append_failure_count() -> u64 {
    AUDIT_OBLIGATION_APPEND_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Process-wide count of `DispatchObligation` rows **refused before they
/// could be enqueued** (`AuditTerminalReason::QueueAdmissionExhausted`) for an
/// [`VerbRegistry::admission_degrade_safe`] verb (#2147/#2217).
/// This is a confirmed, terminal accounting loss: the row never shared a
/// generation with anyone and will never commit. Disjoint from both
/// [`AUDIT_APPEND_FAILURES`] and [`AUDIT_OBLIGATION_APPEND_FAILURES`]: this
/// case is neither. It is not [`AUDIT_APPEND_FAILURES`] — that counter's own
/// contract (`khive-db`'s `WriterContentionDiagnostics::audit_append_failures`
/// doc) says an obligation-bearing row's commit failure "either fail[s] the
/// dispatch... or [is] tracked by the runtime's own separate
/// obligation-failure counter instead", and this dispatch does neither: it
/// reports the caller's already-computed success with no error. It is not
/// [`AUDIT_OBLIGATION_APPEND_FAILURES`] either — that counter's contract is
/// "most call sites fold this failure into the dispatch's own error", which
/// is exactly the propagation this admission-degrade path exists to avoid.
/// Also disjoint from [`AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS`] — that
/// counter's row was enqueued and may still commit; this one's was not.
/// Read in production by [`VerbRegistry::audit_batch_metrics`], which feeds
/// it into `khive_db::diagnostics::RuntimeAuditBatchMetrics::admission_refused_obligations`
/// and from there into the `db_diagnostics` verb's
/// `writer_contention.audit_admission_refused_obligations` field (ADR-103
/// Amendment 3) — an operator can read this counter without a test-only
/// feature gate. The mechanism tests also read it directly, including the
/// admission-pressure regression tests in `tests/read_verb_admission_exhaustion.rs`,
/// which (like `khive-runtime/src/audit_batch.rs`'s own `test_internals`
/// module) need it as `pub`, not `pub(crate)`, since they compile as a
/// separate external binary outside this crate.
///
/// This counter is CUMULATIVE for the life of the process. Nothing decrements
/// it and nothing resolves it: the only writes in the tree are this
/// declaration and one `fetch_add`. A value that does not move therefore means
/// no refusal happened in that window, which is the healthy reading, not a
/// stalled subsystem (#2791). Because a total cannot say when it was last
/// earned, it is paired with
/// [`AUDIT_ADMISSION_REFUSED_OBLIGATIONS_LAST_MS`].
static AUDIT_ADMISSION_REFUSED_OBLIGATIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Wall-clock milliseconds at which [`AUDIT_ADMISSION_REFUSED_OBLIGATIONS`]
/// last moved; `0` means it has never moved in this process. This is the field
/// that makes a static count readable: an old mark beside a non-zero count is
/// history, a recent mark beside the same count is an active condition (#2791).
static AUDIT_ADMISSION_REFUSED_OBLIGATIONS_LAST_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn audit_admission_refused_obligation_count() -> u64 {
    AUDIT_ADMISSION_REFUSED_OBLIGATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `None` until the counter first moves in this process.
pub fn audit_admission_refused_obligation_last_at_ms() -> Option<u64> {
    match AUDIT_ADMISSION_REFUSED_OBLIGATIONS_LAST_MS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        at => Some(at),
    }
}

/// Process-wide count of `DispatchObligation` rows that were **already
/// enqueued but had not resolved by the time the caller's admission wait
/// deadline elapsed** (`AuditTerminalReason::AdmissionDeadlineExpired`) for a
/// succeeded dispatch of any verb (#2147/#2217 introduced the count for
/// [`VerbRegistry::admission_degrade_safe`] reads; writes joined it once a
/// committed write stopped reporting failure over a row that still commits).
/// Unlike [`AUDIT_ADMISSION_REFUSED_OBLIGATIONS`], a row counted here is not
/// a confirmed loss: per `AuditTerminalReason::AdmissionDeadlineExpired`'s own
/// doc, the row may still be committed (or terminally failed) by the
/// generation driver independently of the caller's timeout, so this counter
/// is an upper bound on the eventual undercount, not the undercount itself.
/// Read in production by [`VerbRegistry::audit_batch_metrics`], which feeds
/// it into `khive_db::diagnostics::RuntimeAuditBatchMetrics::admission_unresolved_obligations`
/// and from there into the `db_diagnostics` verb's
/// `writer_contention.audit_admission_unresolved_obligations` field (ADR-103
/// Amendment 3).
///
/// This counter is CUMULATIVE for the life of the process, and its name is the
/// one that misleads: "unresolved obligations" reads as the size of a live set
/// that something drains. There is no such set and no resolver. The only
/// writes in the tree are this declaration and one `fetch_add`, so a value that
/// does not move means no admission deadline expired in that window — the
/// healthy reading (#2791). Each increment records one past event whose row,
/// per `AuditTerminalReason::AdmissionDeadlineExpired`, most likely committed
/// afterwards. Paired with [`AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS_LAST_MS`]
/// so a reader can tell history from an active condition.
static AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Wall-clock milliseconds at which [`AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS`]
/// last moved; `0` means it has never moved in this process (#2791).
static AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS_LAST_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn audit_admission_unresolved_obligation_count() -> u64 {
    AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `None` until the counter first moves in this process.
pub fn audit_admission_unresolved_obligation_last_at_ms() -> Option<u64> {
    match AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS_LAST_MS.load(std::sync::atomic::Ordering::Relaxed)
    {
        0 => None,
        at => Some(at),
    }
}

/// Stamp an admission-obligation counter's "last moved" mark.
///
/// A clock that reads before 1970, or a host clock stepped backwards, must not
/// be able to write `0` and make a counter that HAS moved report that it never
/// did, so a non-positive reading is clamped to 1ms.
fn mark_admission_obligation_counter(mark: &std::sync::atomic::AtomicU64) {
    let now = chrono::Utc::now().timestamp_millis();
    let now = u64::try_from(now).unwrap_or(1).max(1);
    mark.store(now, std::sync::atomic::Ordering::Relaxed);
}

const GIT_DIGEST_RECEIPT_FAILURE: &str =
    "git_digest_receipt_persist_failed: git.digest writes may have committed, but no durable \
     success receipt was confirmed; inspect ingest state before retrying";

/// Tells the dispatch seam whether it should consume the deferred audit or
/// reuse it for the ordinary generic Error row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitDigestReceiptOutcome {
    /// The schema-v2 receipt landed; no second audit row may be appended.
    Persisted,
    /// The handler's nominal success could not be shaped into a receipt. The
    /// helper has converted it to an error, and the original audit remains
    /// available for one generic Error row.
    BuildRejected,
    /// Persistence could not be attempted or its append failed. A second
    /// best-effort append would either be impossible or duplicate the same
    /// known store failure, so the caller must not retry it here.
    PersistenceUnavailable,
}

fn fail_git_digest_receipt(
    result: &mut Result<Value, RuntimeError>,
    failure: AuditObligationFailure,
) {
    let Ok(value) = result else {
        return;
    };
    let domain_result = std::mem::take(value);
    *result = Err(RuntimeError::AuditObligation {
        failure: Box::new(failure),
        domain_result,
    });
}

/// Persist the complete successful `git.digest` report as a schema-v2 audit
/// event and add that event's UUID to the returned report as `receipt_id`.
///
/// This is intentionally strict while every other dispatch audit remains
/// best-effort: a caller must never receive an unqualified digest success if
/// response loss would leave it unable to recover the exact per-pass report.
/// Missing audit/store configuration, an invalid handler report, or an append
/// failure therefore replaces the handler success with a stable safe error.
/// The error does not expose storage paths, source URLs, or command stderr and
/// explicitly warns that ingest writes may already have committed.
async fn persist_git_digest_receipt(
    store: Option<&Arc<dyn EventStore>>,
    audit_batch: Option<&Arc<crate::audit_batch::AuditBatch>>,
    gate_req: &GateRequest,
    audit: Option<&AuditEvent>,
    result: &mut Result<Value, RuntimeError>,
    duration_us: i64,
    resource: Option<Value>,
) -> GitDigestReceiptOutcome {
    let Ok(report) = result else {
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    };
    let Some(store) = store else {
        tracing::error!(
            verb = "git.digest",
            "durable receipt store is not configured"
        );
        fail_git_digest_receipt(
            result,
            AuditObligationFailure::git_digest_receipt("event store is not configured"),
        );
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    };
    let Some(audit) = audit else {
        tracing::error!(
            verb = "git.digest",
            "durable receipt cannot be built because the gate produced no audit decision"
        );
        fail_git_digest_receipt(
            result,
            AuditObligationFailure::git_digest_receipt("gate audit decision is absent"),
        );
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    };

    let Some(report_object) = report.as_object_mut() else {
        tracing::error!(
            verb = "git.digest",
            "digest handler returned a non-object report"
        );
        fail_git_digest_receipt(
            result,
            AuditObligationFailure::git_digest_receipt("handler report is not an object"),
        );
        return GitDigestReceiptOutcome::BuildRejected;
    };
    let Some(project_id) = report_object
        .get("project_id")
        .and_then(Value::as_str)
        .and_then(|raw| raw.parse::<uuid::Uuid>().ok())
    else {
        tracing::error!(
            verb = "git.digest",
            "digest handler report omitted a valid project_id"
        );
        fail_git_digest_receipt(
            result,
            AuditObligationFailure::git_digest_receipt("handler report has no valid project_id"),
        );
        return GitDigestReceiptOutcome::BuildRejected;
    };

    // Allocate the event first so the exact durable key can be embedded in
    // both the caller-visible report and the report snapshot stored in it.
    let mut event = Event::new(
        gate_req.namespace.as_str(),
        gate_req.verb.as_str(),
        EventKind::Audit,
        SubstrateKind::Event,
        format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
    )
    .with_outcome(EventOutcome::Success)
    .with_target(project_id)
    .with_payload_schema_version(2)
    .with_duration_us(duration_us);
    let receipt_id = event.id;
    report_object.insert(
        "receipt_id".to_string(),
        Value::String(receipt_id.to_string()),
    );

    let mut payload = serde_json::to_value(audit).unwrap_or_else(|serialize_err| {
        tracing::error!(
            verb = "git.digest",
            error = %serialize_err,
            "failed to serialize gate audit for durable digest receipt"
        );
        Value::Null
    });
    let Value::Object(payload_object) = &mut payload else {
        tracing::error!(
            verb = "git.digest",
            "gate audit serialization did not produce an object"
        );
        fail_git_digest_receipt(
            result,
            AuditObligationFailure::git_digest_receipt("gate audit payload is not an object"),
        );
        return GitDigestReceiptOutcome::BuildRejected;
    };
    if let Some(resource) = resource {
        payload_object.insert("resource".to_string(), resource);
    }
    payload_object.insert("result".to_string(), report.clone());
    event.payload = payload;

    // Strict path (ADR-133): a git.digest success receipt must still commit
    // exactly once before the caller can see success, so this row waits on
    // its generation's commit through the batch seam rather than
    // best-effort — the batching only changes whether it shares a writer
    // acquisition with concurrent rows, never whether it is durable before
    // the caller observes success.
    let submit_result = if let Some(audit_batch) = audit_batch {
        audit_batch
            .submit_until_resolved(crate::audit_batch::PreparedAuditRow {
                event,
                producer: crate::audit_batch::AuditProducer::GitDigestReceipt,
            })
            .await
            .map(|_outcome| ())
            .map_err(|reason| AuditObligationFailure::new("git.digest", reason))
    } else {
        store
            .append_event(event)
            .await
            .map_err(|error| AuditObligationFailure::from_store("git.digest", error))
    };
    if let Err(mut failure) = submit_result {
        // `GitDigestReceipt` is always `DispatchObligation` (see
        // `crate::audit_batch::classify`) and this failure always
        // propagates below, so it belongs on the obligation counter, not
        // the swallowed-failures one.
        AUDIT_OBLIGATION_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::error!(
            verb = "git.digest",
            error = %failure,
            receipt_id = %receipt_id,
            "durable digest receipt append failed"
        );
        failure.message = format!(
            "{GIT_DIGEST_RECEIPT_FAILURE}; audit submission failed ({})",
            failure.wire_code()
        );
        fail_git_digest_receipt(result, failure);
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    }
    GitDigestReceiptOutcome::Persisted
}

/// Append an audit event, propagating a persistent failure for
/// obligation-bearing producers and swallowing it for pure-observability
/// producers.
///
/// ADR-133 D2/D3/D4: a dispatch must not report success when the row that
/// accounts for, authorizes, or audits it did not commit. Producers
/// classified [`crate::audit_batch::AuditProductionClass::DispatchObligation`]
/// (gate denials, dispatch outcomes, unknown-verb, git.digest receipts)
/// therefore return `Err` here on a persistent commit failure; the caller is
/// responsible for folding that into the dispatch result on the
/// success path — see [`fold_audit_obligation`]. Producers classified
/// [`crate::audit_batch::AuditProductionClass::PureObservability`]
/// (config-lock rows, `memory.recall` execution) degrade gracefully: the
/// failure is logged and counted but never returned, matching the pre-ADR-133
/// best-effort contract.
///
/// Every failure — obligation or observability — increments one of the
/// process-wide diagnostics counters above; the one exception is the
/// admission-degrade case below, which increments one of its own dedicated
/// [`AUDIT_ADMISSION_REFUSED_OBLIGATIONS`] /
/// [`AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS`] counters instead — it is
/// neither a swallowed observability failure nor a propagated obligation
/// failure.
///
/// `degrade_allowlisted` (#2147/#2217) narrows that obligation for
/// one specific case: a *successful* dispatch (`AuditProducer::DispatchSucceeded`)
/// for a verb that [`VerbRegistry::admission_degrade_safe`] has explicitly
/// opted in (Assertive alone is not a sufficient signal — see that method's
/// doc) performs no domain write, so this row's own admission being
/// transiently refused or timed out (`AuditTerminalReason::QueueAdmissionExhausted`
/// / `AdmissionDeadlineExpired`) degrades to best-effort instead of failing
/// the dispatch — the caller-visible read result is preserved. This function
/// derives eligibility from `producer` itself rather than trusting the
/// caller's `degrade_allowlisted` answer in isolation, so a `DispatchFailed`
/// row can never take the degrade path no matter what a caller passes: every
/// failed dispatch and every gate-denial/unknown-verb/git.digest row stays
/// strictly obligation-bearing. A succeeded write degrades on exactly one
/// reason, `AdmissionDeadlineExpired`: its row is already enqueued and its
/// generation commits it independently of the caller's wait, so failing the
/// dispatch would report a committed domain write as failed while changing
/// nothing about the row. `QueueAdmissionExhausted` (refused before enqueue,
/// a confirmed loss) still fails a write's dispatch.
///
/// When the registry has an audit-batch seam configured (it is whenever
/// `store` is), the row routes through
/// [`crate::audit_batch::AuditBatchControl::submit`] instead of taking its
/// own writer-task acquisition — concurrent producers collapse onto one
/// commit per generation. `audit_batch: None` (a `VerbRegistry` predating
/// the seam, or constructed without going through the builder) falls back to
/// the pre-ADR-133 direct append, classified the same way.
async fn append_audit_event_best_effort(
    audit_batch: Option<&Arc<crate::audit_batch::AuditBatch>>,
    store: &Arc<dyn EventStore>,
    event: Event,
    verb: &str,
    producer: crate::audit_batch::AuditProducer,
    degrade_allowlisted: bool,
) -> Result<(), AuditObligationFailure> {
    use crate::audit_batch::{
        classify, AuditBatchControl, AuditProducer, AuditProductionClass, AuditTerminalReason,
    };

    let is_obligation = classify(producer) == AuditProductionClass::DispatchObligation;
    let admission_degrade_eligible =
        degrade_allowlisted && producer == AuditProducer::DispatchSucceeded;
    // A row that was enqueued before the caller's admission wait elapsed is
    // committed by its generation independently of this response, so the
    // only thing failing the dispatch would do is report a committed domain
    // write as failed. That holds for every succeeded dispatch, allowlisted
    // read or not; the refused-before-enqueue arm below stays strict for
    // writes because that one is a confirmed audit loss.
    let enqueued_row_outlives_deadline = producer == AuditProducer::DispatchSucceeded;

    if let Some(audit_batch) = audit_batch {
        let row = crate::audit_batch::PreparedAuditRow { event, producer };
        // khive#2256: for a successful non-degrade-safe operation, the
        // domain effect may already be committed. Once its audit row is
        // enqueued, keep awaiting the generation's real result past the
        // ordinary admission deadline instead of reporting a false failure
        // that invites an unsafe retry. Admission-degrade-safe reads retain
        // their bounded-wait behavior, as do error/denial observations whose
        // caller-visible outcome is already fixed.
        let submit_result =
            if producer == AuditProducer::DispatchSucceeded && !admission_degrade_eligible {
                audit_batch.submit_until_resolved(row).await
            } else {
                audit_batch.submit(row).await
            };
        if let Err(reason) = submit_result {
            if is_obligation {
                // #2147/#2217: a read verb performs no domain write, so
                // when the audit-lane's OWN admission is merely under transient
                // pressure (the row was refused before enqueue, or the caller's
                // wait deadline elapsed on a row that is still likely to commit),
                // failing the read discards a valid result to protect an
                // obligation the read never needed as strictly as a write does.
                // Any other reason (a definite store/durability failure) still
                // fails the dispatch for reads exactly as it does for writes.
                //
                // The two admission-pressure reasons are not the same fact and
                // are counted on separate counters: `QueueAdmissionExhausted`
                // never enqueued, so it is a confirmed terminal loss, while
                // `AdmissionDeadlineExpired` was already enqueued and may still
                // commit later — see `AuditTerminalReason::AdmissionDeadlineExpired`'s
                // own doc.
                if enqueued_row_outlives_deadline
                    && reason == AuditTerminalReason::AdmissionDeadlineExpired
                {
                    AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    mark_admission_obligation_counter(
                        &AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS_LAST_MS,
                    );
                    tracing::warn!(
                        verb,
                        reason = ?reason,
                        degrade_allowlisted,
                        "audit obligation row was still enqueued and unresolved when \
                         the caller's admission wait deadline elapsed; its generation \
                         commits it independently of this response. Dispatch reports \
                         its own committed result (non-fatal)"
                    );
                    return Ok(());
                }
                if admission_degrade_eligible
                    && reason == AuditTerminalReason::QueueAdmissionExhausted
                {
                    AUDIT_ADMISSION_REFUSED_OBLIGATIONS
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    mark_admission_obligation_counter(&AUDIT_ADMISSION_REFUSED_OBLIGATIONS_LAST_MS);
                    tracing::warn!(
                        verb,
                        reason = ?reason,
                        "read verb's audit obligation row was refused before \
                         enqueue under audit-lane admission pressure; dispatch \
                         still reports its own result (non-fatal)"
                    );
                    return Ok(());
                }
                AUDIT_OBLIGATION_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::error!(
                    verb,
                    reason = ?reason,
                    "audit obligation batch submission failed; failing dispatch"
                );
                return Err(AuditObligationFailure::new(verb, reason));
            }
            AUDIT_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                verb,
                reason = ?reason,
                "audit event batch submission failed (non-fatal)"
            );
        }
        return Ok(());
    }

    if let Err(store_err) = store.append_event(event).await {
        if is_obligation {
            AUDIT_OBLIGATION_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                verb,
                error = %store_err,
                "audit obligation store write failed; failing dispatch"
            );
            return Err(AuditObligationFailure::from_store(verb, store_err));
        }
        AUDIT_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            verb,
            error = %store_err,
            "audit event store write failed (non-fatal)"
        );
    }
    Ok(())
}

/// Fold an audit-obligation outcome into a dispatch result.
///
/// A dispatch that would otherwise report success cannot claim it once the
/// row accounting for it fails to commit (ADR-133 D2/D3/D4), so `Ok` becomes
/// the audit's `Err`. A dispatch that already reports failure keeps its
/// original error — the obligation is on never reporting a false success,
/// not on replacing one error with another.
fn fold_audit_obligation<T>(
    result: Result<T, RuntimeError>,
    audit_outcome: Result<(), AuditObligationFailure>,
    domain_value: impl FnOnce(T) -> Value,
) -> Result<T, RuntimeError> {
    match (result, audit_outcome) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(value), Err(failure)) => Err(RuntimeError::AuditObligation {
            failure: Box::new(failure),
            domain_result: domain_value(value),
        }),
        (Err(err), _) => Err(err),
    }
}

/// Schema v2 audit payload for a successful singleton `link` call — additive
/// over v1 via `#[serde(flatten)]`. See `docs/api/pack.md#linkauditsuccessv2`.
#[derive(Debug, Clone, serde::Serialize)]
struct LinkAuditSuccessV2 {
    #[serde(flatten)]
    audit: AuditEvent,
    edge_id: uuid::Uuid,
    source_id: uuid::Uuid,
    target_id: uuid::Uuid,
    relation: String,
    weight: f64,
}

/// Extract edge fields to enrich a successful singleton `link` audit row.
/// Returns `None` on any missing/malformed field (falls back to v1 shape).
/// See `docs/api/pack.md#link_audit_success_from_result`.
fn link_audit_success_from_result(
    audit: AuditEvent,
    result: &serde_json::Value,
) -> Option<(uuid::Uuid, serde_json::Value)> {
    let edge_id = result.get("id")?.as_str()?.parse::<uuid::Uuid>().ok()?;
    let source_id = result
        .get("source_id")?
        .as_str()?
        .parse::<uuid::Uuid>()
        .ok()?;
    let target_id = result
        .get("target_id")?
        .as_str()?
        .parse::<uuid::Uuid>()
        .ok()?;
    let relation = result.get("relation")?.as_str()?.to_string();
    let weight = result.get("weight")?.as_f64()?;
    let enriched = LinkAuditSuccessV2 {
        audit,
        edge_id,
        source_id,
        target_id,
        relation,
        weight,
    };
    let payload = serde_json::to_value(&enriched).ok()?;
    Some((edge_id, payload))
}

/// Resolve and validate a caller-supplied `namespace` argument the same way
/// on every MCP ingress path.
///
/// - Absent `namespace` key → parse `default_namespace`.
/// - Present `namespace: "<string>"` → parse the caller's value.
/// - Present non-string `namespace` (null, number, bool, array, object) →
///   fail closed with `RuntimeError::InvalidInput`. ADR-018 requires this:
///   a malformed explicit value must never be silently coerced to the
///   default namespace.
///
/// Single chokepoint for both `VerbRegistry::dispatch` and the multi-backend
/// coordinator intercept — see `docs/api/pack.md#resolve_explicit_namespace`.
pub fn resolve_explicit_namespace(
    params: &Value,
    default_namespace: &str,
) -> Result<Namespace, RuntimeError> {
    match params.get("namespace") {
        None => Namespace::parse(default_namespace)
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid namespace: {e}"))),
        Some(Value::String(ns_str)) => Namespace::parse(ns_str)
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "invalid namespace: expected string when present, got {}",
            json_type_name(other),
        ))),
    }
}

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
