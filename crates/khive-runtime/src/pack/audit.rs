#[cfg(doc)]
use super::VerbRegistry;
use super::{
    json_type_name, Arc, AuditEvent, AuditObligationFailure, Event, EventKind, EventOutcome,
    EventStore, GateDecision, GateRequest, Namespace, RuntimeError, SubstrateKind, Value,
};

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
pub(super) fn masked_audit_event(
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
pub(super) fn build_audit_storage_event(
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
pub(super) enum GitDigestReceiptOutcome {
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
pub(super) async fn persist_git_digest_receipt(
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
pub(super) async fn append_audit_event_best_effort(
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
pub(super) fn fold_audit_obligation<T>(
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
pub(super) fn link_audit_success_from_result(
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
