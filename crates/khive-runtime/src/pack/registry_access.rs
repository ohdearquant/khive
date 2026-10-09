use std::sync::Arc;

use khive_gate::{GateDecision, GateRequest};
use khive_storage::EventStore;
use khive_types::{EventOutcome, Namespace};
use serde_json::Value;

use crate::error::RuntimeError;
use crate::runtime::NamespaceToken;
use crate::KhiveRuntime;

use super::{
    append_audit_event_best_effort, audit_admission_refused_obligation_count,
    audit_admission_refused_obligation_last_at_ms, audit_admission_unresolved_obligation_count,
    audit_admission_unresolved_obligation_last_at_ms, build_audit_storage_event,
    fold_audit_obligation, masked_audit_event, VerbRegistry, AUDIT_PERSISTENCE_SKIPPED_READ_ONLY,
};
#[cfg(doc)]
use super::{PackRegistry, RequestIdentity, VerbCategory, VerbRegistryBuilder};

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
    /// - `tool.check` persists a policy decision receipt. `git.receipts`,
    ///   `git.gates`, `git.status` and `git.log` dispatch that same check,
    ///   so their read results also carry a required decision write.
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
    pub(super) const ADMISSION_DEGRADE_SAFE_VERBS: &'static [(&'static str, &'static str)] = &[
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
        // git
        // Canonical get project check plus bounded cursor SELECT; no domain writes.
        ("git", "git.ingest_cursor"),
        // blob
        ("blob", "blob.get"),
        ("blob", "blob.stat"),
        // brain
        ("brain", "brain.event_counts"),
        ("brain", "brain.event_page"),
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
        ("kg", "count"),
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
    pub(super) fn admission_degrade_safe_sorted() -> &'static [(&'static str, &'static str)] {
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
    pub(super) fn admission_degrade_safe(&self, verb: &str) -> bool {
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
}
