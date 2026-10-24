use super::*;
use async_trait::async_trait;
use khive_gate::{Gate, GateError};
use khive_storage::{EventFilter, PageRequest};
use khive_types::{HandlerDef, Pack, VerbCategory, Visibility};
use serde_json::json;
use std::sync::Mutex;

use crate::pack::{
    AuditSuccessRowsMode, PackRuntime, RequestIdentity, VerbRegistry, VerbRegistryBuilder,
};
use crate::runtime::NamespaceToken;
use crate::{KhiveRuntime, RuntimeConfig};

// These literals are the test oracle, independent of the production predicate.
const EXPECTED_VERBS: [&str; 6] = [
    "comm.inbox",
    "comm.cleanup_expired_quarantine",
    "whoami",
    "comm.probe",
    "comm.cursor_get",
    "comm.heartbeat",
];

fn expected_retention() -> Value {
    json!({"class": "expendable", "policy": "poll-dispatch-v1", "horizon_seconds": 604800})
}

const fn handler(name: &'static str) -> HandlerDef {
    HandlerDef {
        name,
        description: "retention dispatch fixture",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Declaration,
        params: &[],
    }
}

const COMM: &[HandlerDef] = &[
    handler("comm.inbox"),
    handler("comm.cleanup_expired_quarantine"),
    handler("comm.probe"),
    handler("comm.cursor_get"),
    handler("comm.heartbeat"),
    handler("comm.inbox.extra"),
    handler("comm.inbox_preview"),
];
const KG: &[HandlerDef] = &[handler("whoami"), handler("stats")];
const KNOWLEDGE: &[HandlerDef] = &[handler("knowledge.get")];
const GTD: &[HandlerDef] = &[handler("gtd.tasks")];

struct FixturePack {
    owner: &'static str,
    handlers: &'static [HandlerDef],
    response: Arc<Mutex<Value>>,
    fail: bool,
}

impl Pack for FixturePack {
    const NAME: &'static str = "retention_dispatch_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait]
impl PackRuntime for FixturePack {
    fn name(&self) -> &str {
        self.owner
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        self.handlers
    }

    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        if self.fail {
            Err(RuntimeError::InvalidInput(
                "retention fixture handler error".into(),
            ))
        } else {
            Ok(self.response.lock().unwrap().clone())
        }
    }
}

#[derive(Debug)]
struct FixtureGate {
    deny: bool,
}

impl Gate for FixtureGate {
    fn check(&self, _request: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(if self.deny {
            GateDecision::deny("retention fixture policy denial")
        } else {
            GateDecision::Allow {
                obligations: vec![],
            }
        })
    }
}

struct Rig {
    _runtime: KhiveRuntime,
    store: Arc<dyn EventStore>,
    registry: VerbRegistry,
    response: Arc<Mutex<Value>>,
    fail: bool,
}

fn rig(mode: AuditSuccessRowsMode, outcome: EventOutcome) -> Rig {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        events_split: None,
        actor_id: Some("retention-fixture".into()),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let store = runtime.events(&token).unwrap();
    let response = Arc::new(Mutex::new(
        json!({"outcome": "success", "deleted": 0, "routed_owners_detached": 0}),
    ));
    let mut builder = VerbRegistryBuilder::new();
    builder.with_event_store(store.clone());
    builder.with_audit_success_rows_mode(mode);
    for (owner, handlers) in [
        ("comm", COMM),
        ("kg", KG),
        ("knowledge", KNOWLEDGE),
        ("gtd", GTD),
    ] {
        builder.register_trusted(FixturePack {
            owner,
            handlers,
            response: response.clone(),
            fail: outcome == EventOutcome::Error,
        });
    }
    let mut registry = builder.build().unwrap();
    registry.gate = Arc::new(FixtureGate {
        deny: outcome == EventOutcome::Denied,
    });
    Rig {
        _runtime: runtime,
        store,
        registry,
        response,
        fail: outcome == EventOutcome::Error,
    }
}

impl Rig {
    async fn call(
        &self,
        verb: &str,
        args: Value,
        intercepted: bool,
    ) -> Result<Value, crate::error::DispatchError> {
        let identity = RequestIdentity {
            namespace: "local".into(),
            actor_id: Some("retention-fixture".into()),
            request_id: Some(17),
            ..RequestIdentity::default()
        };
        if intercepted {
            self.registry
                .dispatch_intercepted_with_metadata_and_disposition(
                    verb,
                    &args,
                    Some(&identity),
                    |_| async {
                        let result = if self.fail {
                            Err(RuntimeError::InvalidInput(
                                "retention fixture handler error".into(),
                            ))
                        } else {
                            Ok(self.response.lock().unwrap().clone())
                        };
                        result.map(|value| crate::pack::InterceptedDispatchResult::new(value, ()))
                    },
                )
                .await
                .map(|outcome| outcome.result)
        } else {
            self.registry
                .dispatch_with_disposition(verb, args, Some(identity))
                .await
        }
    }

    async fn rows(&self) -> Vec<Event> {
        let page = self
            .store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 128,
                },
            )
            .await
            .expect("actual SQLite event readback");
        assert!(
            page.items.len() < 128,
            "fixture must read its complete bounded event population"
        );
        page.items
    }

    async fn receipt(&self, verb: &str) -> Event {
        let rows: Vec<_> = self
            .rows()
            .await
            .into_iter()
            .filter(|row| row.kind == EventKind::Audit && row.verb == verb)
            .collect();
        assert_eq!(rows.len(), 1, "one actual generic Audit receipt for {verb}");
        rows.into_iter().next().unwrap()
    }
}

fn assert_stamp(row: &Event) {
    assert_eq!(
        row.payload.get("retention"),
        Some(&expected_retention()),
        "stored generic receipt must contain the complete runtime retention object"
    );
    assert_eq!(
        row.payload["retention"]["horizon_seconds"].as_u64(),
        Some(604800)
    );
}

fn assert_no_stamp(row: &Event) {
    let payload = row
        .payload
        .as_object()
        .expect("real fixture payload object");
    assert!(
        !payload.contains_key("retention"),
        "unlisted or non-generic receipt must not have retention"
    );
}

#[tokio::test]
async fn generic_dispatch_retention_covers_closed_literals_and_all_outcomes() {
    for intercepted in [false, true] {
        for outcome in [
            EventOutcome::Success,
            EventOutcome::Denied,
            EventOutcome::Error,
        ] {
            for verb in EXPECTED_VERBS {
                let rig = rig(AuditSuccessRowsMode::All, outcome);
                assert!(rig
                    .rows()
                    .await
                    .into_iter()
                    .all(|row| row.kind != EventKind::Audit));
                let result = rig.call(verb, json!({}), intercepted).await;
                match outcome {
                    EventOutcome::Success => {
                        result.expect("healthy actual dispatch");
                    }
                    EventOutcome::Denied => assert!(matches!(
                        result.unwrap_err().into_source(),
                        RuntimeError::PermissionDenied { .. }
                    )),
                    EventOutcome::Error => assert!(matches!(
                        result.unwrap_err().into_source(),
                        RuntimeError::InvalidInput(_)
                    )),
                }
                let row = rig.receipt(verb).await;
                assert_eq!(row.kind, EventKind::Audit);
                assert_eq!(row.verb, verb);
                assert_eq!(row.outcome, outcome);
                assert_stamp(&row);
            }
        }
    }
}

#[tokio::test]
async fn generic_dispatch_retention_preserves_unlisted_and_caller_arguments() {
    let forged = json!({"class": "caller", "policy": "caller-policy", "horizon_seconds": 1});
    for intercepted in [false, true] {
        for verb in [
            "knowledge.get",
            "gtd.tasks",
            "stats",
            "comm.inbox.extra",
            "comm.inbox_preview",
        ] {
            let rig = rig(AuditSuccessRowsMode::All, EventOutcome::Success);
            rig.call(verb, json!({"retention": forged.clone()}), intercepted)
                .await
                .expect("healthy unlisted dispatch");
            let row = rig.receipt(verb).await;
            assert_eq!(row.outcome, EventOutcome::Success);
            assert_no_stamp(&row);
        }
        // Ordinary unknown dispatch provides a stored differently-cased Error row.
        if !intercepted {
            let rig = rig(AuditSuccessRowsMode::All, EventOutcome::Success);
            let error = rig
                .call("Comm.Inbox", json!({}), false)
                .await
                .unwrap_err()
                .into_source();
            assert!(matches!(error, RuntimeError::UnknownVerb(_)));
            let row = rig.receipt("Comm.Inbox").await;
            assert_eq!(row.outcome, EventOutcome::Error);
            assert_no_stamp(&row);
        }
        for verb in EXPECTED_VERBS {
            let rig = rig(AuditSuccessRowsMode::All, EventOutcome::Success);
            rig.call(verb, json!({"retention": forged.clone()}), intercepted)
                .await
                .expect("healthy forged-argument dispatch");
            assert_stamp(&rig.receipt(verb).await);
        }
    }
}

#[tokio::test]
async fn generic_dispatch_retention_kind_and_non_generic_survival() {
    let rig = rig(AuditSuccessRowsMode::All, EventOutcome::Success);
    let kinds = [
        EventKind::FeedbackExplicit,
        EventKind::PhaseStarted,
        EventKind::PhaseCompleted,
        EventKind::PhaseCancelled,
        EventKind::ChannelPollStarted,
        EventKind::ChannelPollSucceeded,
        EventKind::ChannelPollFailed,
        EventKind::ChannelBackoffArmed,
        EventKind::ChannelBackoffReset,
        EventKind::ChannelHeartbeatPersistFailed,
    ];
    for kind in kinds {
        for verb in EXPECTED_VERBS {
            let event = Event::new(
                "local",
                verb,
                kind,
                SubstrateKind::Event,
                "retention-fixture",
            )
            .with_payload(json!({"fixture": "independent non-generic row"}));
            let id = event.id;
            rig.store
                .append_event(event)
                .await
                .expect("real non-generic append");
            let row = rig
                .rows()
                .await
                .into_iter()
                .find(|row| row.id == id)
                .expect("actual non-generic row exists");
            assert_eq!(row.kind, kind);
            assert_eq!(row.verb, verb);
            assert_no_stamp(&row);
            assert!(
                !expendable_dispatch_audit(kind, verb),
                "non-audit training/lifecycle kind must not classify as expendable"
            );
        }
    }
    // The direct append API is not the generic dispatch producer even when its
    // Audit key has an allowlisted spelling.
    let event = Event::new(
        "local",
        "comm.inbox",
        EventKind::Audit,
        SubstrateKind::Event,
        "retention-fixture",
    )
    .with_payload(json!({"fixture": "dedicated Audit append"}));
    let id = event.id;
    rig.store.append_event(event).await.unwrap();
    let row = rig
        .rows()
        .await
        .into_iter()
        .find(|row| row.id == id)
        .expect("actual dedicated Audit row exists");
    assert_no_stamp(&row);
}

fn fresh_sampling_process() -> bool {
    const MARKER: &str = "KHIVE_RETENTION_SAMPLING_CHILD";
    if std::env::var_os(MARKER).is_some() {
        return true;
    }
    let selector = "pack::audit::retention_tests::generic_dispatch_retention_retains_sampled_and_forced_success_metadata";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", selector, "--nocapture"])
        .env(MARKER, "1")
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "fresh sampling fixture failed: {status}");
            return false;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("fresh sampling fixture exceeded its deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[tokio::test]
async fn generic_dispatch_retention_retains_sampled_and_forced_success_metadata() {
    if !fresh_sampling_process() {
        return;
    }
    for verb in ["comm.heartbeat", "comm.cleanup_expired_quarantine"] {
        let rig = rig(AuditSuccessRowsMode::Reduced, EventOutcome::Success);
        for (seq, selected_by) in [(0_u64, "sample"), (1, "forced")] {
            *rig.response.lock().unwrap() = if seq == 0 {
                json!({"outcome": "success", "deleted": 0, "routed_owners_detached": 0})
            } else {
                json!({"outcome": "failed", "deleted": 1, "routed_owners_detached": 0})
            };
            rig.call(verb, json!({}), false)
                .await
                .expect("retained sampled/forced dispatch");
            let rows: Vec<_> = rig
                .rows()
                .await
                .into_iter()
                .filter(|row| row.kind == EventKind::Audit && row.verb == verb)
                .collect();
            assert_eq!(rows.len(), usize::try_from(seq + 1).unwrap());
            let row = rows
                .iter()
                .find(|row| row.payload["resource"]["sampling"]["seq"] == seq)
                .expect("actual retained sequence row");
            assert_eq!(
                row.payload["resource"]["sampling"],
                json!({"every": 10, "seq": seq, "selected_by": selected_by})
            );
            assert_stamp(row);
        }
    }
}
