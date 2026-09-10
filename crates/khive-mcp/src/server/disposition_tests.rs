//! ADR-133 A3: prove dispositions against actual domain storage, not a stand-in vector.

use super::*;
use khive_runtime::audit_batch::{AuditBatchConfig, AuditBatchControl, AuditTerminalReason};
use khive_runtime::{AuditObligationFailure, DomainDisposition, Namespace};
use khive_storage::event::IdempotentEventBatchResult;
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, StorageResult,
};
use std::sync::atomic::{AtomicBool, AtomicUsize};

/// Domain handlers keep their real SQLite stores. Only the registry's separate
/// audit append is rejected, after preflight has accepted the actual audit row.
struct ControlledAuditStore {
    inner: Arc<dyn EventStore>,
    reject: AtomicBool,
    rejected: AtomicUsize,
    barrier: Option<Arc<AuditBarrier>>,
}

struct AuditBarrier {
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

struct ReleaseAuditOnDrop(Arc<AuditBarrier>);

impl Drop for ReleaseAuditOnDrop {
    fn drop(&mut self) {
        // Also release on an assertion panic: no fixture leaves a detached
        // audit driver permanently waiting for a global sleep/failpoint.
        self.0.release.add_permits(16);
    }
}

#[async_trait::async_trait]
impl EventStore for ControlledAuditStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.inner.append_event(event).await
    }
    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        self.inner.append_events(events).await
    }
    async fn get_event(&self, id: uuid::Uuid) -> StorageResult<Option<Event>> {
        self.inner.get_event(id).await
    }
    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        self.inner.query_events(filter, page).await
    }
    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        self.inner.count_events(filter).await
    }
    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        self.inner.preflight_event(event)
    }
    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        if let Some(barrier) = &self.barrier {
            barrier.entered.add_permits(1);
            barrier
                .release
                .acquire()
                .await
                .expect("audit barrier open")
                .forget();
        }
        if self.reject.load(Ordering::SeqCst) {
            self.rejected.fetch_add(1, Ordering::SeqCst);
            return Err(khive_storage::StorageError::InvalidInput {
                capability: StorageCapability::Events,
                operation: "a3-audit-append".into(),
                message: "deterministic audit-store rejection after domain dispatch".into(),
            });
        }
        self.inner.append_events_idempotent(events).await
    }
}

struct ErrorPack;

impl khive_types::Pack for ErrorPack {
    const NAME: &'static str = "a3fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_runtime::HandlerDef] = &[
        khive_runtime::HandlerDef {
            name: "a3_handler_invalid",
            description: "raise InvalidInput inside the handler",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Directive,
            params: &[],
        },
        khive_runtime::HandlerDef {
            name: "a3_handler_secret",
            description: "invoke the actual secret gate inside the handler",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Directive,
            params: &[],
        },
        khive_runtime::HandlerDef {
            name: "a3_nested_refusal",
            description: "propagate a nested dispatch refusal through its parent handler",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Directive,
            params: &[],
        },
        khive_runtime::HandlerDef {
            name: "hidden",
            description: "operator-only probe that must never run over the wire",
            visibility: khive_runtime::Visibility::Subhandler,
            category: khive_runtime::VerbCategory::Directive,
            params: &[],
        },
    ];
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for ErrorPack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        registry: &VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "a3_handler_invalid" => Err(RuntimeError::InvalidInput(
                "raised inside the handler".into(),
            )),
            "a3_handler_secret" => {
                // Same fake token as the secret gate's own positive control.
                khive_runtime::secret_gate::check("AKIAFAKEKEY1234567890")?;
                panic!("the actual secret detector must reject its positive control")
            }
            "a3_nested_refusal" => registry.dispatch("a3_missing_child", json!({})).await,
            "hidden" => panic!("wire subhandler guard was bypassed"),
            other => panic!("unexpected fixture handler {other}"),
        }
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    server: KhiveMcpServer,
    audit: Arc<ControlledAuditStore>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::with_audit_config(
            AuditBatchConfig {
                max_commit_attempts: std::num::NonZeroU8::new(1).unwrap(),
                ..AuditBatchConfig::default()
            },
            None,
        )
    }

    fn with_audit_config(config: AuditBatchConfig, barrier: Option<Arc<AuditBarrier>>) -> Self {
        let dir = tempfile::tempdir().expect("A3 temporary database directory");
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(dir.path().join("domain.sqlite")),
            embedding_model: None,
            packs: vec!["kg".into(), "comm".into()],
            ..RuntimeConfig::default()
        })
        .expect("file-backed domain runtime");
        let token = runtime.authorize(Namespace::local()).expect("local token");
        let audit = Arc::new(ControlledAuditStore {
            inner: runtime.events(&token).expect("actual audit backend"),
            reject: AtomicBool::new(false),
            rejected: AtomicUsize::new(0),
            barrier,
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register_trusted(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register_trusted(khive_pack_comm::CommPack::new(runtime.clone()));
        builder.register(ErrorPack);
        builder.with_event_store(audit.clone());
        builder.with_audit_batch_config(config);
        let server = KhiveMcpServer::from_registry(builder.build().expect("A3 registry"));
        Self {
            runtime,
            server,
            audit,
            _dir: dir,
        }
    }

    async fn request(&self, ops: &str) -> Value {
        let output = self
            .server
            .dispatch_request_inner(
                RequestParams {
                    plan: None,
                    ops: ops.into(),
                    presentation: Some("verbose".into()),
                    format: Some("json".into()),
                    ..Default::default()
                },
                true,
                None,
                DispatchOrigin::Local,
            )
            .await
            .expect("operation errors belong in the request envelope");
        serde_json::from_str(&output).expect("canonical JSON response")
    }

    async fn stats(&self) -> Value {
        let response = self.request("stats()").await;
        assert_eq!(response["results"][0]["ok"], true, "{response}");
        response["results"][0]["result"].clone()
    }
}

fn committed_result(entry: &Value) -> &Value {
    assert_eq!(entry["ok"], false, "{entry}");
    assert_eq!(entry["error"]["kind"], "obligation", "{entry}");
    assert_eq!(entry["error"]["domain_disposition"], "committed", "{entry}");
    entry["error"]
        .get("domain_result")
        .expect("committed failure retains its canonical result")
}

/// `dual_write_message` persists both message notes in one atomic unit. Root
/// sends use the outbound UUID as thread_id; only the inbound copy carries
/// outbound_ref. Verify the stored relationship without consulting an error
/// envelope, so the same check remains a control when that envelope is mutated.
fn assert_stored_send_pair(
    rows: &[khive_storage::Note],
    outbound_id: uuid::Uuid,
) -> [uuid::Uuid; 2] {
    let full_id = outbound_id.as_hyphenated().to_string();
    let outbound = rows
        .iter()
        .find(|row| row.id == outbound_id)
        .expect("canonical outbound message is physically stored");
    let outbound_props = outbound.properties.as_ref().expect("outbound properties");
    assert_eq!(outbound_props["direction"], "outbound");
    assert!(outbound_props.get("outbound_ref").is_none());
    let linked: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.properties
                .as_ref()
                .and_then(|props| props.get("outbound_ref"))
                .and_then(Value::as_str)
                == Some(full_id.as_str())
        })
        .collect();
    assert_eq!(
        linked.len(),
        1,
        "exactly one physical inbound copy links to {full_id}"
    );
    let inbound = linked[0];
    assert_ne!(
        outbound.id, inbound.id,
        "the send creates two distinct notes"
    );
    assert_eq!(inbound.properties.as_ref().unwrap()["direction"], "inbound");
    for row in [outbound, inbound] {
        assert_eq!(row.kind, "message");
        assert_eq!(row.namespace, "local");
        assert_eq!(row.content, "a3-identical-replay");
        let props = row.properties.as_ref().unwrap();
        assert_eq!(props["from"], "local");
        assert_eq!(props["to"], "local");
        // VerbRegistryBuilder's unset actor uses ActorRef::anonymous(), whose
        // id is "local"; comm.send passes that id to both physical copies.
        assert_eq!(props["from_actor"], "local");
        assert_eq!(props["to_actor"], "local");
        assert_eq!(props["thread_id"], full_id);
        assert_eq!(props["read"], false);
    }
    [outbound.id, inbound.id]
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn a3_committed_comm_send_preserves_real_row_and_identical_replay_creates_another() {
    let fixture = Fixture::new();
    let before = fixture.stats().await;
    fixture.audit.reject.store(true, Ordering::SeqCst);
    let ops = r#"comm.send(to="local", content="a3-identical-replay")"#;
    let first = fixture.request(ops).await;
    let token = fixture.runtime.authorize(Namespace::local()).unwrap();
    let notes = fixture.runtime.notes(&token).unwrap();
    let persisted = notes
        .query_notes(
            "local",
            Some("message"),
            PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .expect("domain control query bypasses the failing audit path");
    assert_eq!(persisted.items.len(), 2, "the domain send must persist both physical notes even if the mutation removes its error fields");
    let physical_outbound_id = persisted
        .items
        .iter()
        .find(|row| {
            row.properties
                .as_ref()
                .and_then(|props| props.get("direction"))
                .and_then(Value::as_str)
                == Some("outbound")
        })
        .expect("the storage control must find an outbound note")
        .id;
    let first_pair = assert_stored_send_pair(&persisted.items, physical_outbound_id);
    eprintln!("domain storage control: persisted a3-identical-replay before checking error fields");
    let first_result = committed_result(&first["results"][0]);
    assert_eq!(
        first["results"][0]["error"]["code"], "store_failure",
        "{first}"
    );
    let first_id = uuid::Uuid::parse_str(
        first_result["full_id"]
            .as_str()
            .expect("canonical full message id"),
    )
    .unwrap();
    assert_eq!(
        first_id, first_pair[0],
        "domain_result names the actual outbound UUID, not the inbound copy"
    );
    assert_eq!(
        first_result["full_id"],
        first_pair[0].as_hyphenated().to_string()
    );
    assert_eq!(first_result["thread_id"], first_result["full_id"]);

    // Deliberately violate the consumer rule. This documents current replay
    // behaviour rather than manufacturing an idempotency guarantee.
    let second = fixture.request(ops).await;
    let second_result = committed_result(&second["results"][0]);
    let second_id = uuid::Uuid::parse_str(second_result["full_id"].as_str().unwrap()).unwrap();
    assert_ne!(
        first_id, second_id,
        "identical replay currently creates a second domain row"
    );
    let replayed = notes
        .query_notes(
            "local",
            Some("message"),
            PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .expect("replay storage control bypasses the failing audit path");
    assert_eq!(
        replayed.items.len(),
        4,
        "identical replay creates a second complete physical pair"
    );
    assert_eq!(
        assert_stored_send_pair(&replayed.items, first_id),
        first_pair
    );
    let second_pair = assert_stored_send_pair(&replayed.items, second_id);
    assert_eq!(
        second_result["full_id"],
        second_pair[0].as_hyphenated().to_string()
    );
    assert_eq!(second_result["thread_id"], second_result["full_id"]);
    let pair_ids: std::collections::BTreeSet<_> =
        first_pair.into_iter().chain(second_pair).collect();
    assert_eq!(
        pair_ids.len(),
        4,
        "the two sends must own four distinct physical notes"
    );
    assert_eq!(
        pair_ids,
        replayed.items.iter().map(|row| row.id).collect(),
        "both linked pairs account for every stored message"
    );
    for original in &persisted.items {
        let current = notes
            .get_note(original.id)
            .await
            .unwrap()
            .expect("the original physical copy remains persisted after replay");
        assert_eq!(
            &current, original,
            "replay must not alter either original copy"
        );
    }
    assert!(
        fixture.audit.rejected.load(Ordering::SeqCst) > 0,
        "the fixture must actually reach the rejecting audit append"
    );
    assert_eq!(before["notes"], 0);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn a3_committed_noncomm_create_preserves_exact_entity_id() {
    let fixture = Fixture::new();
    fixture.stats().await;
    fixture.audit.reject.store(true, Ordering::SeqCst);
    let response = fixture
        .request(r#"create(kind="entity", entity_kind="concept", name="a3-persisted-entity")"#)
        .await;
    let result = committed_result(&response["results"][0]);
    assert_eq!(
        response["results"][0]["error"]["code"], "store_failure",
        "{response}"
    );
    let id = uuid::Uuid::parse_str(result["id"].as_str().expect("canonical entity id")).unwrap();
    let token = fixture.runtime.authorize(Namespace::local()).unwrap();
    let row = fixture
        .runtime
        .get_entity(&token, id)
        .await
        .expect("actual entity row committed before the audit failure");
    assert_eq!(row.id, id);
    assert_eq!(row.name, "a3-persisted-entity");
    assert!(fixture.audit.rejected.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn a3_predispatch_refusals_leave_real_stats_unchanged() {
    let fixture = Fixture::new();
    let before = fixture.stats().await;
    for ops in [
        "a3_missing_verb()",
        "stats(namespace=42)",
        "a3fixture.hidden()",
    ] {
        let response = fixture.request(ops).await;
        let entry = &response["results"][0];
        assert_eq!(entry["ok"], false, "{ops}: {response}");
        assert_eq!(
            entry["error"]["domain_disposition"], "not_committed",
            "{ops}: {response}"
        );
        assert!(
            entry["error"].get("domain_result").is_none(),
            "{ops}: {response}"
        );
        assert_eq!(
            fixture.stats().await,
            before,
            "refused operation changed domain stats: {ops}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn a3_handler_errors_and_nested_child_refusal_remain_unknown() {
    let fixture = Fixture::new();
    for ops in [
        "a3_handler_invalid()",
        "a3_handler_secret()",
        "a3_nested_refusal()",
    ] {
        let response = fixture.request(ops).await;
        let entry = &response["results"][0];
        assert_eq!(entry["ok"], false, "{ops}: {response}");
        assert_eq!(
            entry["error"]["domain_disposition"], "unknown",
            "{ops}: {response}"
        );
        assert!(
            entry["error"].get("domain_result").is_none(),
            "{ops}: {response}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn a3_failed_chain_marks_only_unreached_operations_aborted() {
    let fixture = Fixture::new();
    let before = fixture.stats().await;
    let response = fixture.request(r#"a3_handler_invalid() | create(kind="entity", entity_kind="concept", name="must-not-exist") | comm.send(to="local", content="must-not-exist")"#).await;
    assert_eq!(
        response["results"][0]["error"]["domain_disposition"],
        "unknown"
    );
    for entry in response["results"].as_array().unwrap().iter().skip(1) {
        assert_eq!(entry["ok"], false);
        assert_eq!(entry["aborted"], true);
        assert_eq!(entry["domain_disposition"], "not_committed");
        assert!(entry.get("error").is_none());
    }
    assert_eq!(
        response["summary"],
        json!({"total": 3, "succeeded": 0, "failed": 1, "aborted": 2})
    );
    assert_eq!(fixture.stats().await, before);
}

fn obligation_error(result: Value) -> RuntimeError {
    RuntimeError::AuditObligation {
        failure: Box::new(AuditObligationFailure::new(
            "a3",
            AuditTerminalReason::StoreFailure,
        )),
        domain_result: result,
    }
}

#[test]
#[serial_test::serial(config_ledger)]
fn a3_obligation_projection_retains_the_complete_canonical_value() {
    let expected =
        json!({"id": uuid::Uuid::new_v4().to_string(), "nested": {"items": [1, {"kept": true}]}});
    let error = runtime_error_value(
        obligation_error(expected.clone()),
        DomainDisposition::Committed,
    );
    assert_eq!(error["kind"], "obligation");
    assert_eq!(error["code"], "store_failure");
    assert_eq!(error["domain_disposition"], "committed");
    assert_eq!(error["domain_result"], expected);
}

#[test]
#[serial_test::serial(config_ledger)]
fn a3_deep_error_domain_result_is_omitted_before_recursive_operations() {
    // Construction and rejection must both be iterative. Ordinary Value clone,
    // serialization or Drop at this depth is itself the regression signal.
    let mut deep = Value::Bool(true);
    for _ in 0..khive_request::NESTING_DEPTH_LIMIT + 50_000 {
        let mut map = serde_json::Map::new();
        map.insert("nested".into(), deep);
        deep = Value::Object(map);
    }
    let projected = runtime_error_value(obligation_error(deep), DomainDisposition::Committed);
    let entry = failure_entry("a3", projected, DomainDisposition::Committed);
    let error = &entry["error"];
    assert_eq!(error["domain_disposition"], "committed");
    assert_eq!(error["code"], "result_too_deep");
    assert!(error.get("domain_result").is_none());
    let serialized =
        serde_json::to_string(&error).expect("bounded rejected error serializes safely");
    assert!(serialized.len() < 4096);
}

async fn wait_for_audit_state(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("audit state barrier was never reached");
}

fn occupant_row(verb: &str) -> khive_runtime::audit_batch::PreparedAuditRow {
    khive_runtime::audit_batch::PreparedAuditRow {
        event: Event::new(
            "local",
            verb,
            khive_types::EventKind::Audit,
            khive_types::SubstrateKind::Event,
            "test:a3",
        )
        .with_outcome(khive_types::EventOutcome::Success),
        producer: khive_runtime::audit_batch::AuditProducer::ConfigLocked,
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn a3_degrade_safe_read_keeps_success_under_both_transient_reasons() {
    use khive_runtime::pack::{
        audit_admission_refused_obligation_count, audit_admission_unresolved_obligation_count,
    };
    for saturated in [true, false] {
        let barrier = Arc::new(AuditBarrier {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        });
        let release_on_exit = ReleaseAuditOnDrop(barrier.clone());
        let fixture = Fixture::with_audit_config(
            AuditBatchConfig {
                max_pending_rows: std::num::NonZeroUsize::new(if saturated { 1 } else { 2 })
                    .unwrap(),
                admission_deadline: std::time::Duration::from_millis(250),
                ..AuditBatchConfig::default()
            },
            Some(barrier.clone()),
        );
        let batch = fixture
            .server
            .registry
            .audit_batch_handle()
            .expect("configured audit batch");
        let occupant_batch = batch.clone();
        let occupant =
            tokio::spawn(async move { occupant_batch.submit(occupant_row("a3.occupant")).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), barrier.entered.acquire())
            .await
            .expect("audit append not entered")
            .expect("entered semaphore open")
            .forget();
        assert!(batch.test_snapshot().in_flight_generation.is_some());
        let filler = if saturated {
            let filler_batch = batch.clone();
            let task =
                tokio::spawn(async move { filler_batch.submit(occupant_row("a3.filler")).await });
            wait_for_audit_state(|| batch.test_snapshot().pending_rows == 1).await;
            Some(task)
        } else {
            None
        };
        let before = batch.test_snapshot();
        let refused_before = audit_admission_refused_obligation_count();
        let unresolved_before = audit_admission_unresolved_obligation_count();
        let response = fixture.request("whoami()").await;
        let entry = &response["results"][0];
        assert_eq!(entry["ok"], true, "saturated={saturated}: {response}");
        assert!(entry.get("result").is_some());
        assert!(entry.get("domain_disposition").is_none());
        assert!(entry.get("error").is_none());
        let after = batch.test_snapshot();
        if saturated {
            assert_eq!(
                after.submitted_rows, before.submitted_rows,
                "refused audit row must never enter the batch"
            );
            assert_eq!(
                audit_admission_refused_obligation_count(),
                refused_before + 1
            );
            assert_eq!(
                audit_admission_unresolved_obligation_count(),
                unresolved_before
            );
        } else {
            assert_eq!(
                after.submitted_rows,
                before.submitted_rows + 1,
                "deadline-expired row was enqueued and remains unresolved"
            );
            assert_eq!(
                audit_admission_unresolved_obligation_count(),
                unresolved_before + 1
            );
            assert_eq!(audit_admission_refused_obligation_count(), refused_before);
        }
        drop(release_on_exit);
        tokio::time::timeout(std::time::Duration::from_secs(5), batch.quiesce())
            .await
            .expect("released batch did not quiesce")
            .expect("released audit rows commit");
        let _ = occupant.await.expect("occupant task must finish");
        if let Some(filler) = filler {
            let _ = filler.await.expect("filler task must finish");
        }
        assert!(batch.test_snapshot().is_idle());
    }
}

// Parse JSON macro fields structurally. Rust's AST deliberately leaves macro
// bodies opaque, so visiting ExprMacro alone would miss a new raw envelope.
enum JsonSyntax {
    Object(Vec<(Option<String>, JsonSyntax)>),
    Array(Vec<JsonSyntax>),
    Rust(syn::Expr),
}

impl syn::parse::Parse for JsonSyntax {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        if input.peek(syn::token::Brace) {
            let body;
            syn::braced!(body in input);
            let mut fields = Vec::new();
            while !body.is_empty() {
                let key = if body.peek(syn::LitStr) {
                    Some(body.parse::<syn::LitStr>()?.value())
                } else {
                    let _ = body.parse::<syn::Ident>()?;
                    None
                };
                body.parse::<syn::Token![:]>()?;
                fields.push((key, body.parse()?));
                if !body.is_empty() {
                    body.parse::<syn::Token![,]>()?;
                }
            }
            Ok(Self::Object(fields))
        } else if input.peek(syn::token::Bracket) {
            let body;
            syn::bracketed!(body in input);
            let mut items = Vec::new();
            while !body.is_empty() {
                items.push(body.parse::<JsonSyntax>()?);
                if !body.is_empty() {
                    body.parse::<syn::Token![,]>()?;
                }
            }
            Ok(Self::Array(items))
        } else {
            Ok(Self::Rust(input.parse()?))
        }
    }
}

impl JsonSyntax {
    fn field(&self, key: &str) -> Option<&Self> {
        let Self::Object(fields) = self else {
            return None;
        };
        fields
            .iter()
            .find_map(|(name, value)| (name.as_deref() == Some(key)).then_some(value))
    }
    fn bool_is(&self, expected: bool) -> bool {
        matches!(self, Self::Rust(syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Bool(value), .. })) if value.value == expected)
    }
}

fn literal_key(expr: &syn::Expr) -> Option<String> {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(value),
            ..
        }) => Some(value.value()),
        syn::Expr::MethodCall(call)
            if call.args.is_empty() && (call.method == "into" || call.method == "to_string") =>
        {
            literal_key(&call.receiver)
        }
        _ => None,
    }
}

fn is_test_only(attrs: &[syn::Attribute]) -> bool {
    // `cfg(test)` and `cfg(all(unix,test))` are absent from production. A
    // cfg(any(test, feature=...)) item remains in the census: it can exist in
    // a non-test build and must not become an escape hatch.
    fn requires_test(meta: &syn::Meta) -> bool {
        match meta {
            syn::Meta::Path(path) => path.is_ident("test"),
            syn::Meta::List(list) if list.path.is_ident("all") => list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .expect("valid cfg(all) syntax")
                .iter()
                .any(requires_test),
            syn::Meta::List(list) if list.path.is_ident("any") => list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .expect("valid cfg(any) syntax")
                .iter()
                .all(requires_test),
            _ => false,
        }
    }
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && requires_test(&attr.parse_args::<syn::Meta>().expect("valid cfg syntax"))
    })
}

#[derive(Default)]
struct ErrorConstructorCensus {
    source: String,
    modules: Vec<String>,
    impl_type: Option<String>,
    function: String,
    function_depth: usize,
    offenders: Vec<String>,
    constructors_seen: usize,
    runtime_variants: std::collections::BTreeSet<String>,
}

impl ErrorConstructorCensus {
    fn reject(&mut self, reason: &str) {
        let mut location = self.modules.clone();
        if let Some(ty) = &self.impl_type {
            location.push(ty.clone());
        }
        location.push(self.function.clone());
        self.offenders.push(format!(
            "{}::{}: {reason}",
            self.source,
            location.join("::")
        ));
    }
    fn root_function(&self, source: &str, function: &str) -> bool {
        self.source == source
            && self.modules.is_empty()
            && self.impl_type.is_none()
            && self.function == function
            && self.function_depth == 1
    }
    fn central_entry_builder(&self) -> bool {
        self.root_function("khive-mcp/src/server.rs", "failure_entry")
            || self.root_function("khive-mcp/src/server.rs", "aborted_entry")
    }
    fn inspect_json(&mut self, parsed: &JsonSyntax) {
        let summary = ["total", "succeeded", "failed", "aborted"]
            .iter()
            .all(|key| parsed.field(key).is_some())
            && parsed.field("ok").is_none();
        if parsed.field("ok").is_some_and(|value| value.bool_is(false))
            || (parsed.field("ok").is_some() && parsed.field("error").is_some())
            || (parsed.field("aborted").is_some() && !summary)
        {
            self.constructors_seen += 1;
            if !self.central_entry_builder() {
                self.reject("raw failed/aborted JSON envelope bypasses the typed constructors");
            }
        }
        match parsed {
            JsonSyntax::Object(fields) => {
                for (_, value) in fields {
                    self.inspect_json(value);
                }
            }
            JsonSyntax::Array(items) => {
                for value in items {
                    self.inspect_json(value);
                }
            }
            // json! delegates expressions to Rust. Parentheses or a block
            // around a nested constructor must not make it invisible.
            JsonSyntax::Rust(expr) => syn::visit::Visit::visit_expr(self, expr),
        }
    }
    fn inspect_literal_pairs<'a>(&mut self, values: impl IntoIterator<Item = &'a syn::Expr>) {
        let keys: std::collections::BTreeSet<_> = values
            .into_iter()
            .filter_map(|expr| {
                let syn::Expr::Tuple(tuple) = expr else {
                    return None;
                };
                (tuple.elems.len() == 2)
                    .then(|| literal_key(&tuple.elems[0]))
                    .flatten()
            })
            .collect();
        let summary = ["total", "succeeded", "failed", "aborted"]
            .iter()
            .all(|key| keys.contains(*key))
            && !keys.contains("ok");
        if keys.contains("error") || (keys.contains("aborted") && !summary) {
            self.constructors_seen += 1;
            if !self.central_entry_builder() {
                self.reject("raw literal-pair error/aborted map bypasses the typed constructors");
            }
        }
    }
    fn inspect_opaque_tokens(&mut self, mut cursor: syn::buffer::Cursor<'_>) {
        use syn::parse::Parser;
        // select!/other macro grammars are not Rust expressions. Inspect token
        // groups without stringifying source or silently ignoring their bodies.
        // A constructor hidden in an unsupported grammar must be moved to an
        // ordinary scanned helper, even if it currently supplies good data.
        let remaining = cursor.token_stream();
        if let Ok(values) =
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                .parse2(remaining)
        {
            self.inspect_literal_pairs(values.iter());
        }
        while !cursor.eof() {
            if let Some((ident, after_ident)) = cursor.ident() {
                if ident == "McpError"
                    || ident == "ErrorData"
                    || ident == "DaemonResponseFrame"
                    || ident == "DaemonDispatchError"
                {
                    self.constructors_seen += 1;
                    self.reject("MCP/frame constructor hidden inside an opaque macro; move it to a scanned helper");
                }
                if ident == "json" {
                    if let Some((bang, after_bang)) = after_ident.punct() {
                        if bang.as_char() == '!' {
                            if let Some((body, _, _, after_group)) = after_bang.any_group() {
                                let parsed = syn::parse2::<JsonSyntax>(body.token_stream())
                                    .expect("nested JSON macro must parse");
                                self.inspect_json(&parsed);
                                cursor = after_group;
                                continue;
                            }
                        }
                    }
                }
                cursor = after_ident;
            } else if let Some((body, _, _, after_group)) = cursor.any_group() {
                self.inspect_opaque_tokens(body);
                cursor = after_group;
            } else {
                cursor = cursor
                    .token_tree()
                    .expect("nonempty macro cursor must advance")
                    .1;
            }
        }
    }
    fn classified_data(&self, expr: &syn::Expr) -> bool {
        match expr {
            syn::Expr::Call(call) => {
                let syn::Expr::Path(path) = call.func.as_ref() else {
                    return false;
                };
                let Some(name) = path.path.segments.last() else {
                    return false;
                };
                if name.ident == "Some" {
                    call.args.len() == 1 && self.classified_data(&call.args[0])
                } else {
                    name.ident == "error_with_disposition"
                        && path.path.segments.len() == 1
                        && self.source == "khive-mcp/src/server.rs"
                        && self.modules.is_empty()
                }
            }
            syn::Expr::Macro(mac)
                if mac
                    .mac
                    .path
                    .segments
                    .last()
                    .is_some_and(|name| name.ident == "json") =>
            {
                syn::parse2::<JsonSyntax>(mac.mac.tokens.clone())
                    .expect("error-detail JSON macro must parse")
                    .field("domain_disposition")
                    .is_some()
            }
            // Limit passthrough to the actual helpers that normalize this
            // carrier. An arbitrary object named error_detail is no proof.
            syn::Expr::Field(field) => {
                matches!(&field.member, syn::Member::Named(name) if name == "error_detail")
                    && (self.root_function("khive-mcp/src/daemon.rs", "daemon_mcp_error")
                        || self.root_function("khive-mcp/src/daemon.rs", "protocol_mismatch_error")
                        || self.root_function(
                            "khive-runtime/src/daemon.rs",
                            "handle_conn_with_shutdown",
                        ))
            }
            _ => false,
        }
    }

    fn classify_runtime_pattern(&mut self, pattern: &syn::Pat) {
        let path = match pattern {
            syn::Pat::Ident(binding) if binding.subpat.is_some() => {
                self.classify_runtime_pattern(&binding.subpat.as_ref().unwrap().1);
                return;
            }
            syn::Pat::Paren(paren) => {
                self.classify_runtime_pattern(&paren.pat);
                return;
            }
            syn::Pat::Or(or) => {
                for case in &or.cases {
                    self.classify_runtime_pattern(case);
                }
                return;
            }
            syn::Pat::Path(path) => &path.path,
            syn::Pat::Struct(pattern) => &pattern.path,
            syn::Pat::TupleStruct(pattern) => &pattern.path,
            _ => {
                self.reject(
                    "runtime error classification contains a catch-all or unsupported pattern",
                );
                return;
            }
        };
        assert!(
            path.segments
                .iter()
                .any(|segment| segment.ident == "RuntimeError"),
            "runtime error arm must name its variant"
        );
        self.runtime_variants
            .insert(path.segments.last().unwrap().ident.to_string());
    }
}

const EXTERNAL_ERROR_MODULES: &[(&str, &str, &str)] = &[(
    "khive-mcp/src/daemon.rs",
    "executable",
    "khive-mcp/src/daemon/executable.rs",
)];

impl<'ast> syn::visit::Visit<'ast> for ErrorConstructorCensus {
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if !is_test_only(&item.attrs) {
            let current = match item.self_ty.as_ref() {
                syn::Type::Path(ty) => ty
                    .path
                    .segments
                    .last()
                    .map(|segment| segment.ident.to_string()),
                _ => Some("<non-path-impl>".into()),
            };
            let previous = std::mem::replace(&mut self.impl_type, current);
            syn::visit::visit_item_impl(self, item);
            self.impl_type = previous;
        }
    }
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if is_test_only(&item.attrs) {
            return;
        }
        if item.content.is_none()
            && !(self.modules.is_empty()
                && item.attrs.is_empty()
                && EXTERNAL_ERROR_MODULES
                    .iter()
                    .any(|(source, module, _)| self.source == *source && item.ident == *module))
        {
            // External modules must join the scanned source set. Reject path
            // overrides and nested declarations that would change that target.
            self.reject(&format!("unscanned production submodule {}; add its source to the census before delegating envelope construction", item.ident));
        }
        self.modules.push(item.ident.to_string());
        syn::visit::visit_item_mod(self, item);
        self.modules.pop();
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if is_test_only(&item.attrs) {
            return;
        }
        let previous = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        self.function_depth += 1;
        syn::visit::visit_item_fn(self, item);
        self.function_depth -= 1;
        self.function = previous;
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if is_test_only(&item.attrs) {
            return;
        }
        let previous = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        self.function_depth += 1;
        syn::visit::visit_impl_item_fn(self, item);
        self.function_depth -= 1;
        self.function = previous;
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac
            .path
            .segments
            .last()
            .is_some_and(|name| name.ident == "json")
        {
            let parsed = syn::parse2::<JsonSyntax>(mac.tokens.clone()).expect(
                "production JSON macro must parse; do not silently omit an unsupported constructor",
            );
            self.inspect_json(&parsed);
        } else {
            let tokens = syn::buffer::TokenBuffer::new2(mac.tokens.clone());
            self.inspect_opaque_tokens(tokens.begin());
        }
    }
    fn visit_expr_array(&mut self, expr: &'ast syn::ExprArray) {
        self.inspect_literal_pairs(expr.elems.iter());
        syn::visit::visit_expr_array(self, expr);
    }
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "insert" && call.args.len() == 2 {
            if let Some(key) = literal_key(&call.args[0]) {
                if key == "error" || key == "aborted" {
                    self.constructors_seen += 1;
                    // The sole copy-through is search_incomplete's existing
                    // error, bound from entry.get("error") in this transform.
                    // New raw branches here still have to use the normalizer.
                    let preserved = self
                        .root_function("khive-mcp/src/server.rs", "frame_budget_omission")
                        && (self.classified_data(&call.args[1])
                            || matches!(&call.args[1], syn::Expr::MethodCall(copy)
                            if copy.method == "clone" && copy.args.is_empty() &&
                                matches!(copy.receiver.as_ref(), syn::Expr::Path(path) if path.path.is_ident("error"))));
                    if !self.central_entry_builder() && !preserved {
                        self.reject(
                            "raw error/aborted map insertion bypasses the typed constructors",
                        );
                    }
                }
            }
        }
        syn::visit::visit_expr_method_call(self, call);
    }
    fn visit_expr_assign(&mut self, expr: &'ast syn::ExprAssign) {
        if let syn::Expr::Index(index) = expr.left.as_ref() {
            if literal_key(&index.index).is_some_and(|key| key == "error" || key == "aborted")
                && !self.central_entry_builder()
            {
                self.reject("raw indexed error/aborted assignment bypasses the typed constructors");
            }
        }
        syn::visit::visit_expr_assign(self, expr);
    }
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = call.func.as_ref() {
            if path
                .path
                .segments
                .iter()
                .any(|segment| segment.ident == "McpError" || segment.ident == "ErrorData")
            {
                self.constructors_seen += 1;
                if !call
                    .args
                    .last()
                    .is_some_and(|expr| self.classified_data(expr))
                {
                    self.reject("MCP error constructor lacks classified error data");
                }
            }
        }
        syn::visit::visit_expr_call(self, call);
    }
    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
        let field = |name: &str| {
            expr.fields
                .iter()
                .find(|field| matches!(&field.member, syn::Member::Named(ident) if ident == name))
        };
        if expr
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "McpError" || segment.ident == "ErrorData")
        {
            self.constructors_seen += 1;
            if !field("data").is_some_and(|field| self.classified_data(&field.expr)) {
                self.reject("raw MCP error struct lacks classified error data");
            }
        }
        if expr.path.segments.last().is_some_and(|segment| {
            segment.ident == "DaemonDispatchError"
                || (segment.ident == "Self"
                    && self.impl_type.as_deref() == Some("DaemonDispatchError"))
        }) {
            self.constructors_seen += 1;
            let normalizer = self.source == "khive-runtime/src/daemon.rs"
                && self.modules.is_empty()
                && self.impl_type.as_deref() == Some("DaemonDispatchError")
                && self.function == "new"
                && self.function_depth == 1;
            if !normalizer {
                self.reject("raw DaemonDispatchError bypasses its exact normalizing constructor");
            }
        }
        if expr
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "DaemonResponseFrame")
        {
            let success = field("ok").is_some_and(|field| matches!(&field.expr, syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Bool(value), .. }) if value.value));
            if !success {
                self.constructors_seen += 1;
                if !field("error_detail").is_some_and(|field| self.classified_data(&field.expr)) {
                    self.reject("failed daemon frame lacks classified error_detail");
                }
            }
        }
        syn::visit::visit_expr_struct(self, expr);
    }
    fn visit_expr_match(&mut self, expr: &'ast syn::ExprMatch) {
        if self.root_function("khive-mcp/src/server.rs", "runtime_error_value")
            && matches!(expr.expr.as_ref(), syn::Expr::Path(path) if path.path.is_ident("error"))
        {
            for arm in &expr.arms {
                self.classify_runtime_pattern(&arm.pat);
            }
        }
        syn::visit::visit_expr_match(self, expr);
    }
}

fn census_source(path: &std::path::Path) -> ErrorConstructorCensus {
    use syn::visit::Visit;
    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("missing census source {}: {error}", path.display()));
    let file = syn::parse_file(&source)
        .unwrap_or_else(|error| panic!("unparseable census source {}: {error}", path.display()));
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .canonicalize()
        .expect("census workspace exists");
    let canonical = path.canonicalize().expect("census source exists");
    let mut census = ErrorConstructorCensus {
        source: canonical
            .strip_prefix(root)
            .expect("census source is inside its declared workspace")
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    };
    census.visit_file(&file);
    census
}

#[test]
#[serial_test::serial(config_ledger)]
fn a3_production_error_constructor_census_is_closed_and_runtime_match_is_total() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let sources = [
        manifest.join("src/server.rs"),
        manifest.join("src/daemon.rs"),
        manifest.join("../khive-runtime/src/daemon.rs"),
    ];
    let mut emitted = 0;
    let mut runtime_variants = std::collections::BTreeSet::new();
    for source in sources.into_iter().chain(
        EXTERNAL_ERROR_MODULES
            .iter()
            .map(|(_, _, path)| manifest.parent().unwrap().join(path)),
    ) {
        let census = census_source(&source);
        assert!(
            census.offenders.is_empty(),
            "{}:\n{}",
            source.display(),
            census.offenders.join("\n")
        );
        emitted += census.constructors_seen;
        runtime_variants.extend(census.runtime_variants);
    }
    assert!(
        emitted > 0,
        "empty constructor population means the scanner is broken"
    );
    let error_file = manifest.join("../khive-runtime/src/error.rs");
    let source = std::fs::read_to_string(&error_file).expect("runtime error source must exist");
    let ast = syn::parse_file(&source).expect("runtime error source must parse");
    let variants: std::collections::BTreeSet<_> = ast
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Enum(item) if item.ident == "RuntimeError" => Some(
                item.variants
                    .iter()
                    .map(|variant| variant.ident.to_string())
                    .collect(),
            ),
            _ => None,
        })
        .expect("RuntimeError declaration must be present");
    assert_eq!(runtime_variants, variants, "every real runtime variant must have an explicit projection arm; adding a catch-all is not coverage");
}

#[test]
fn a3_constructor_census_rejects_redirected_registered_modules() {
    use syn::visit::Visit;
    for source in [
        r#"#[path = "other.rs"] mod executable;"#,
        r#"mod nested { mod executable; }"#,
    ] {
        let mut census = ErrorConstructorCensus {
            source: "khive-mcp/src/daemon.rs".into(),
            ..Default::default()
        };
        census.visit_file(&syn::parse_file(source).unwrap());
        assert!(
            !census.offenders.is_empty(),
            "module redirected outside the scanned population"
        );
    }
}

#[test]
fn a3_constructor_census_rejects_new_raw_sites_and_unclassified_variants() {
    use syn::visit::Visit;
    for mutation in [
        r#"fn added() { json!({"ok": false, "tool": "x", "error": "lost"}); }"#,
        r#"fn added() { json!({"results": [{"ok": false, "error": "lost"}]}); }"#,
        r#"fn added() { json!({"ok": false, "aborted": true}); }"#,
        r#"fn added() { json!({"ok": ready, "error": payload}); }"#,
        r#"fn added() { json!({"ok": ready, "aborted": skipped}); }"#,
        r#"fn added() { json!({"wrapper": (json!({"ok":false,"error":"lost"}))}); }"#,
        r#"fn added() { serde_json::Map::from_iter([("ok".into(), json!(false)), ("error".into(), json!("lost"))]); }"#,
        r#"fn added() { [("error".to_string(), payload)].into_iter().collect::<serde_json::Map<String, Value>>(); }"#,
        r#"fn added() { serde_json::Map::from_iter(vec![("error".into(), payload)]); }"#,
        r#"fn added() { entry.insert("error".into(), json!("lost")); }"#,
        r#"fn added() { entry["error"] = json!("lost"); }"#,
        r#"fn added() { McpError::internal_error("lost", None); }"#,
        r#"fn added() { McpError { code: code, message: message, data: None }; }"#,
        r#"fn added() { rmcp::ErrorData { code: code, message: message, data: None }; }"#,
        r#"fn added() { McpError::internal_error("lost", Some(arbitrary.error_detail)); }"#,
        r#"fn added() { DaemonDispatchError { message: message, error_detail: json!({}) }; }"#,
        r#"mod added { fn failure_entry() { json!({"ok":false,"error":"lost"}); } }"#,
        r#"fn added() { fn failure_entry() { json!({"ok":false,"error":"lost"}); } }"#,
        r#"fn added() { DaemonResponseFrame { ok: false, error: Some("lost"), error_detail: Some(json!({"message":"lost"})) }; }"#,
        r#"fn runtime_error_value(error: RuntimeError) { match error { _ => json!("lost") } }"#,
        r#"mod unscanned_envelope_builders;"#,
    ] {
        let mut census = ErrorConstructorCensus {
            source: "khive-mcp/src/server.rs".into(),
            ..Default::default()
        };
        census.visit_file(&syn::parse_file(mutation).expect("valid mutation fixture"));
        assert!(
            !census.offenders.is_empty(),
            "scanner accepted unclassified mutation: {mutation}"
        );
    }
    let mut census = ErrorConstructorCensus::default();
    census.visit_file(&syn::parse_file(r#"
        #[cfg(test)] mod tests { fn ignored() { json!({"ok":false,"error":"fixture"}); } }
        fn classified() { McpError::internal_error("message", Some(json!({"domain_disposition":"unknown"}))); }
        fn domain_payload() { json!({"error":"a domain field rather than an envelope"}); }
        fn summary() { json!({"total": n, "succeeded": s, "failed": f, "aborted": a}); }
    "#).unwrap());
    assert!(
        census.offenders.is_empty(),
        "scanner must distinguish production constructors from fixtures/domain data"
    );
    assert!(census.constructors_seen > 0);
}

#[test]
fn a3_opaque_macro_census_rejects_hidden_error_constructors() {
    use syn::visit::Visit;
    for mutation in [
        r#"async fn added() { tokio::select! { _ = pending() => { Err(McpError::internal_error("lost", None)) } } }"#,
        r#"async fn added() { tokio::select! { _ = pending() => { return json!({"ok": false, "error": "lost"}); } }; }"#,
        r#"async fn added() { tokio::select! { _ = pending() => { return json!({"ok": state, "aborted": skipped}); } }; }"#,
        r#"async fn added() { tokio::select! { _ = pending() => { return DaemonResponseFrame { ok: false, error: Some("lost"), error_detail: None }; } }; }"#,
        r#"fn added() { arbitrary_macro!(outer!(json!({"ok": false, "error": "lost"}))); }"#,
    ] {
        let mut census = ErrorConstructorCensus {
            source: "khive-mcp/src/server.rs".into(),
            ..Default::default()
        };
        census.visit_file(&syn::parse_file(mutation).expect("valid opaque macro fixture"));
        assert!(
            !census.offenders.is_empty(),
            "opaque macro hid an unclassified constructor: {mutation}"
        );
    }
    let mut census = ErrorConstructorCensus::default();
    census.visit_file(&syn::parse_file(r#"
        async fn allowed() {
            tokio::select! { _ = pending() => { return cancelled_forward_error(request_id); } }
        }
        fn diagnostic() { tracing::warn!("McpError::internal_error and json!({ok:false}) are text, not Rust constructors"); }
    "#).unwrap());
    assert!(
        census.offenders.is_empty(),
        "ordinary scanned-helper calls and string literals must remain admissible"
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn a3_frame_budget_preserves_committed_and_unknown_failure_provenance() {
    let registry = VerbRegistryBuilder::new().build().unwrap();
    for disposition in [DomainDisposition::Committed, DomainDisposition::Unknown] {
        let error = if disposition == DomainDisposition::Committed {
            runtime_error_value(
                obligation_error(json!({"id": "known", "body": "x".repeat(100_000)})),
                disposition,
            )
        } else {
            runtime_error_value(RuntimeError::InvalidInput("x".repeat(100_000)), disposition)
        };
        let entry = failure_entry("a3", error, disposition);
        let omitted = frame_budget_omission(&entry, &registry);
        assert_eq!(omitted["ok"], false);
        assert_eq!(
            omitted["error"]["domain_disposition"],
            disposition.as_str(),
            "{omitted}"
        );
        assert!(omitted["error"].get("domain_result").is_none());
        if disposition == DomainDisposition::Committed {
            assert_eq!(omitted["error"]["code"], "response_frame_budget_exceeded");
        }
    }
}

#[cfg(unix)]
#[derive(Default)]
struct A3NativeCapture {
    dispatch_payload: Option<String>,
    daemon_response_json: Option<String>,
    daemon_request: Option<Value>,
}

/// Observe the real MCP adapter; never synthesize an operation result or retry it.
#[cfg(unix)]
#[derive(Clone)]
struct A3CapturingDispatch {
    server: KhiveMcpServer,
    calls: Arc<AtomicUsize>,
    capture: Arc<std::sync::Mutex<A3NativeCapture>>,
}

#[cfg(unix)]
#[async_trait::async_trait]
impl khive_runtime::daemon::DaemonDispatch for A3CapturingDispatch {
    fn plan(&self, ops: &str) -> String {
        self.server.plan_ops(ops)
    }

    async fn dispatch(
        &self,
        _ops: String,
        _presentation: Option<String>,
        _presentation_per_op: Option<Vec<Option<String>>>,
        _format: Option<String>,
        _format_per_op: Option<Vec<Option<String>>>,
        _from_wire: bool,
        _identity: Option<khive_runtime::RequestIdentity>,
    ) -> Result<String, String> {
        panic!("the native handler must use dispatch_with_error_detail")
    }

    async fn dispatch_with_error_detail(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
    ) -> Result<String, khive_runtime::daemon::DaemonDispatchError> {
        assert_eq!(self.calls.fetch_add(1, Ordering::SeqCst), 0, "no replay");
        let payload = self
            .server
            .dispatch_with_error_detail(
                ops,
                presentation,
                presentation_per_op,
                format,
                format_per_op,
                from_wire,
                identity,
            )
            .await?;
        assert!(self
            .capture
            .lock()
            .unwrap()
            .dispatch_payload
            .replace(payload.clone())
            .is_none());
        Ok(payload)
    }

    async fn warm_all(&self) {}

    fn namespace(&self) -> &str {
        self.server.default_namespace()
    }

    fn config_id(&self) -> &str {
        self.server.config_id()
    }
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn a3_same_committed_failure_crosses_mcp_request_and_native_frame_once() {
    use khive_runtime::daemon::{
        read_frame, serve_connection_for_test, write_frame, DaemonRequestFrame, DaemonResponseFrame,
    };
    use std::sync::Mutex;

    // request_with_forward accepts a function pointer. This private, one-use
    // handoff carries its dispatcher into that spawned forward task, then is
    // immediately emptied. No daemon environment or lifecycle state is changed.
    static FORWARD: Mutex<Option<A3CapturingDispatch>> = Mutex::new(None);
    struct ClearForward;
    impl Drop for ClearForward {
        fn drop(&mut self) {
            *FORWARD.lock().unwrap() = None;
        }
    }
    fn native_forward(
        frame: DaemonRequestFrame,
        _packs: Option<Vec<String>>,
        replay_read_only: bool,
    ) -> ForwardFuture {
        assert!(!replay_read_only, "comm.send must not be replayed");
        let dispatcher = FORWARD.lock().unwrap().take().expect("one native forward");
        Box::pin(async move {
            let capture = Arc::clone(&dispatcher.capture);
            let config_id = frame.config_id.clone();
            let namespace = frame.namespace.clone();
            let encoded = serde_json::to_vec(&frame).unwrap();
            capture.lock().unwrap().daemon_request =
                Some(serde_json::from_slice(&encoded).unwrap());
            let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
            let handler = tokio::spawn(serve_connection_for_test(server, dispatcher));
            let exchange = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                write_frame(&mut client, &encoded).await?;
                read_frame(&mut client).await
            })
            .await;
            if !matches!(&exchange, Ok(Ok(_))) {
                handler.abort();
            }
            let joined = handler.await;
            let raw = exchange
                .expect("bounded native exchange")
                .expect("native frame I/O");
            joined.expect("native connection task must finish");
            let response: DaemonResponseFrame = serde_json::from_slice(&raw).unwrap();
            capture.lock().unwrap().daemon_response_json = Some(String::from_utf8(raw).unwrap());
            crate::daemon::map_response_for_test(response, &config_id, &namespace)
        })
    }

    let fixture = Fixture::new();
    assert_eq!(fixture.stats().await["notes"], 0);
    fixture.audit.reject.store(true, Ordering::SeqCst);
    let calls = Arc::new(AtomicUsize::new(0));
    let capture = Arc::new(Mutex::new(A3NativeCapture::default()));
    let _clear = ClearForward;
    assert!(FORWARD
        .lock()
        .unwrap()
        .replace(A3CapturingDispatch {
            server: fixture.server.clone(),
            calls: Arc::clone(&calls),
            capture: Arc::clone(&capture),
        })
        .is_none());
    let ops = r#"comm.send(to="local", content="a3-identical-replay")"#;
    let mcp_payload = fixture
        .server
        .request_with_forward(
            RequestParams {
                plan: None,
                ops: ops.into(),
                presentation: Some("verbose".into()),
                format: Some("json".into()),
                request_id: Some(133_006),
                ..Default::default()
            },
            native_forward,
        )
        .await
        .expect("the MCP request retains the failed operation in its envelope");
    assert!(FORWARD.lock().unwrap().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.audit.rejected.load(Ordering::SeqCst), 1);

    let token = fixture.runtime.authorize(Namespace::local()).unwrap();
    let persisted = fixture
        .runtime
        .notes(&token)
        .unwrap()
        .query_notes(
            "local",
            Some("message"),
            PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        persisted.items.len(),
        2,
        "one send creates one physical pair"
    );
    let mcp_response: Value = serde_json::from_str(&mcp_payload).unwrap();
    let result = committed_result(&mcp_response["results"][0]);
    let full_id = result["full_id"].as_str().unwrap();
    let outbound_id = uuid::Uuid::parse_str(full_id).unwrap();
    assert_eq!(outbound_id.as_hyphenated().to_string(), full_id);
    let pair = assert_stored_send_pair(&persisted.items, outbound_id);

    let capture = capture.lock().unwrap();
    let dispatch_payload = capture.dispatch_payload.as_ref().unwrap();
    let native_json = capture.daemon_response_json.as_ref().unwrap();
    let native: DaemonResponseFrame = serde_json::from_str(native_json).unwrap();
    assert!(
        native.ok,
        "the request envelope contains the per-op failure"
    );
    assert!(native.error.is_none());
    assert!(native.error_detail.is_none());
    assert_eq!(native.request_id, Some(133_006));
    assert_eq!(native.result.as_ref(), Some(dispatch_payload));
    assert_eq!(&mcp_payload, dispatch_payload);
    let native_envelope: Value = serde_json::from_str(native.result.as_ref().unwrap()).unwrap();
    assert_eq!(
        native_envelope["results"][0]["error"],
        mcp_response["results"][0]["error"]
    );

    // Explicit opt-in evidence export for the separately invoked Python
    // capture-replay checker. A normal Rust test has no Python dependency.
    // Never overwrite an earlier receipt or create an implicit output path.
    if let Some(path) = std::env::var_os("A3_NATIVE_CAPTURE_PATH") {
        use std::io::Write;
        let evidence = json!({
            "schema": "khive-a3-native-capture-v1",
            "request_ops": ops,
            "daemon_request": capture.daemon_request,
            "dispatch_payload": dispatch_payload,
            "mcp_payload": mcp_payload,
            "daemon_response_json": native_json,
            "native_dispatches": calls.load(Ordering::SeqCst),
            "audit_rejections": fixture.audit.rejected.load(Ordering::SeqCst),
            "physical_note_ids": pair.map(|id| id.as_hyphenated().to_string()),
            "physical_notes": persisted.items,
        });
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        file.write_all(&serde_json::to_vec_pretty(&evidence).unwrap())
            .unwrap();
        file.write_all(b"\n").unwrap();
    }
}

#[test]
#[serial_test::serial(config_ledger)]
fn a_gate_refusal_projects_its_audit_receipt_beside_the_denial_text() {
    let id = uuid::Uuid::new_v4();
    let error = runtime_error_value(
        RuntimeError::PermissionDenied {
            verb: "create".into(),
            reason: "denied for test".into(),
            receipt: Box::new(khive_runtime::DenialReceipt {
                audit_event_id: Some(id),
                audit_outcome: khive_runtime::DenialAuditOutcome::Committed,
            }),
        },
        DomainDisposition::NotCommitted,
    );
    assert_eq!(error["kind"], "runtime_error");
    assert_eq!(error["code"], "permission_denied");
    assert_eq!(
        error["message"],
        "permission denied for verb \"create\": denied for test"
    );
    assert_eq!(error["verb"], "create");
    assert_eq!(error["reason"], "denied for test");
    assert_eq!(error["audit_event_id"], id.to_string());
    assert_eq!(error["audit_outcome"], "committed");

    let unaudited = runtime_error_value(
        RuntimeError::permission_denied("authorize", "gate denied"),
        DomainDisposition::NotCommitted,
    );
    assert_eq!(unaudited["audit_event_id"], Value::Null);
    assert_eq!(unaudited["audit_outcome"], "not_audited");
    assert!(unaudited["message"]
        .as_str()
        .unwrap()
        .starts_with("permission denied for verb"));
}
#[test]
fn ordered_fences_refusal_has_no_domain_commit() {
    for index in [None, Some("0"), Some("1")] {
        let mut details = vec![
            ("reason", "fence_conflict".to_owned()),
            ("key", "lease".to_owned()),
            ("expected_version", "1".to_owned()),
        ];
        if let Some(index) = index {
            details.push(("index", index.to_owned()));
        }
        let error = khive_types::KhiveError::conflict("note fence precondition failed")
            .with_details(khive_types::Details::new_owned(details));
        let value = runtime_error_value(error.into(), DomainDisposition::Unknown);
        assert_eq!(value["domain_disposition"], "not_committed");
        assert_eq!(value["details"].get("index").and_then(Value::as_str), index);
    }
}
