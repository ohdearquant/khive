//! Production-path fixtures for immutable occurrence accounting.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{DateTime, Timelike, Utc};
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::occurrence::*;
use crate::occurrence_store::{OccurrenceStore, Recovery, TestClock};

const NS: &str = "schedule-fixture";
const CHILD_ROOT: &str = "KHIVE_D8_TEST_ROOT";
const CHILD_CASE: &str = "KHIVE_D8_TEST_CASE";
const CHILD_PHASE: &str = "KHIVE_D8_TEST_PHASE";
const TABLES: [&str; 6] = [
    "scheduled_definitions_v3",
    "scheduled_series_v3",
    "scheduled_occurrences_v3",
    "scheduled_attempts_v3",
    "scheduled_receipts_v3",
    "scheduled_notice_outbox_v3",
];

fn child(name: &str, root: &Path, phase: Option<&str>) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("private HOME");
    let locks = root.join("locks");
    std::fs::create_dir_all(&locks).expect("private locks");
    let writer_mode = std::env::var("KHIVE_WRITE_QUEUE")
        .ok()
        .filter(|mode| mode == "0" || mode == "1");
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command.args([
        "--exact",
        &format!("occurrence_tests::{name}"),
        "--nocapture",
        "--test-threads=1",
    ]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") || key == "LATTICE_MODEL_CACHE" {
            command.env_remove(key);
        }
    }
    command
        .env("HOME", &home)
        .env("KHIVE_TEST_HARNESS", "1")
        .env("KHIVE_VOLUME_LOCK_DIR", &locks)
        .env("KHIVE_LOCK", root.join("daemon.lock"))
        .env("KHIVE_RECOVERER_LOCK", root.join("recoverer.lock"))
        .env("KHIVE_SOCKET", root.join("d.sock"))
        .env("KHIVE_PID", root.join("d.pid"))
        .env(CHILD_ROOT, root)
        .env(CHILD_CASE, name);
    if let Some(mode) = writer_mode {
        command.env("KHIVE_WRITE_QUEUE", mode);
    }
    if let Some(phase) = phase {
        command.env(CHILD_PHASE, phase);
    }
    command.output().expect("run exact child test")
}
fn healthy(output: Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "child failed:\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "child did not execute one exact test:\n{stdout}\n{stderr}"
    );
}
fn isolate(name: &str) -> Option<PathBuf> {
    if std::env::var(CHILD_CASE).as_deref() == Ok(name) {
        return Some(PathBuf::from(
            std::env::var_os(CHILD_ROOT).expect("private child root"),
        ));
    }
    let root = tempfile::tempdir().expect("isolated fixture root");
    healthy(child(name, root.path(), None));
    None
}

struct Fixture {
    rt: KhiveRuntime,
    registry: VerbRegistry,
    store: OccurrenceStore,
}
impl Fixture {
    fn open(root: &Path) -> Self {
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(root.join("schedule.db")),
            default_namespace: Namespace::parse(NS).expect("namespace"),
            actor_id: Some("lambda:schedule-fixture".into()),
            packs: vec!["kg".into(), "comm".into(), "schedule".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("file-backed runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder
            .with_default_namespace(NS)
            .with_actor_id(Some("lambda:schedule-fixture".into()));
        builder.register(khive_pack_kg::KgPack::new(rt.clone()));
        builder.register(khive_pack_comm::CommPack::new(rt.clone()));
        builder.register(crate::SchedulePack::new(rt.clone()));
        let registry = builder.build().expect("registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), rt.backend())
            .expect("checked schema install");
        if let Ok(mode) = std::env::var("KHIVE_WRITE_QUEUE") {
            assert!(mode == "0" || mode == "1", "explicit fixture writer mode");
            assert_eq!(
                rt.backend().pool().write_queue_active(),
                mode == "1",
                "actual file-backed writer mode"
            );
        }
        let store = OccurrenceStore::open(&rt).expect("durable store");
        Self {
            rt,
            registry,
            store,
        }
    }
    fn token(&self) -> NamespaceToken {
        self.rt
            .authorize(Namespace::parse(NS).expect("namespace"))
            .expect("fixture token")
    }
    async fn declare(&self, repeat: Option<&str>, catch_up: bool) -> (Uuid, i64) {
        let at = (Utc::now() + chrono::Duration::hours(1))
            .with_nanosecond(0)
            .expect("whole second");
        let mut params = json!({"action":"stats()","at":at.to_rfc3339(),"receipt_version":3});
        if let Some(repeat) = repeat {
            params["repeat"] = json!(repeat);
        }
        if catch_up {
            params["misfire_policy"] = json!("catch_up_once");
            params["max_lateness"] = json!(3600);
        }
        let result =
            crate::handlers::handle_schedule(&self.rt, &self.token(), &self.registry, params)
                .await
                .expect("real declaration");
        let id = Uuid::parse_str(result["full_id"].as_str().expect("schedule UUID")).expect("UUID");
        let series = self
            .store
            .series(id)
            .await
            .expect("series read")
            .expect("series exists");
        let definition = self
            .store
            .definition(&series)
            .await
            .expect("authenticated definition");
        assert_eq!(definition.creator.id, "lambda:schedule-fixture");
        assert_eq!(definition.namespace, NS);
        (id, at.timestamp_micros())
    }
    async fn page(
        &self,
        id: Uuid,
        horizon: i64,
        allowance: usize,
    ) -> crate::occurrence_store::AccountingPage {
        let snapshot = self
            .store
            .series(id)
            .await
            .expect("series")
            .expect("present");
        let definition = self.store.definition(&snapshot).await.expect("definition");
        let prepared = self
            .store
            .prepare_page(&snapshot, &definition, horizon, 300, allowance)
            .expect("prepare page");
        self.store
            .commit_page(prepared)
            .await
            .expect("page transaction")
            .expect("no race")
    }
    async fn inventory(&self, id: Uuid) -> Vec<OccurrenceObservation> {
        let mut items = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .store
                .occurrences(&self.token(), id, None, after, 200)
                .await
                .expect("inventory");
            after = page.items.last().map(|o| (o.key.scheduled_for, o.id));
            items.extend(page.items);
            if !page.has_more {
                return items;
            }
            assert!(after.is_some(), "continuation progressed");
        }
    }
    async fn rows(&self) -> Value {
        let mut reader = self.rt.sql().reader().await.expect("reader");
        let mut all = serde_json::Map::new();
        for table in TABLES {
            let rows = reader
                .query_all(SqlStatement::new(
                    format!("SELECT * FROM {table} ORDER BY rowid"),
                    vec![],
                ))
                .await
                .expect("complete stored rows");
            all.insert(
                table.into(),
                serde_json::to_value(rows).expect("row encoding"),
            );
        }
        Value::Object(all)
    }
    async fn sql(
        &self,
        sql: impl Into<String>,
        params: Vec<SqlValue>,
    ) -> Result<u64, khive_storage::StorageError> {
        self.rt
            .sql()
            .writer()
            .await?
            .execute(SqlStatement::new(sql, params))
            .await
    }
}

#[tokio::test]
async fn projection_refuses_carried_reserved_properties_and_rolls_back_complete_state() {
    let Some(root) =
        isolate("projection_refuses_carried_reserved_properties_and_rolls_back_complete_state")
    else {
        return;
    };
    async fn complete_state(f: &Fixture) -> Value {
        let mut reader = f.rt.sql().reader().await.expect("complete state reader");
        let mut all = serde_json::Map::new();
        for table in TABLES.into_iter().chain(["notes", "events"]) {
            let rows = reader
                .query_all(SqlStatement::new(
                    format!("SELECT * FROM {table} ORDER BY rowid"),
                    vec![],
                ))
                .await
                .expect("complete stored state");
            all.insert(table.into(), serde_json::to_value(rows).unwrap());
        }
        Value::Object(all)
    }
    for (case, key, value) in [
        ("secret-null", "khive:secret_gate", "null"),
        ("secret-object", "khive:secret_gate", r#"{"policy":"held"}"#),
        ("web-null", "khive:web_receipt", "null"),
        ("web-object", "khive:web_receipt", r#"{"receipt":"held"}"#),
    ] {
        let case_root = root.join(case);
        std::fs::create_dir_all(&case_root).unwrap();
        let f = Fixture::open(&case_root);
        let (id, first) = f.declare(None, false).await;
        let _clock = TestClock::fixed(f.store.store_id(), first);
        let path = format!("$.\"{key}\"");
        assert_eq!(
            f.sql(
                "UPDATE notes SET properties=json_set(properties,?1,json(?2)) WHERE id=?3",
                vec![
                    SqlValue::Text(path.clone()),
                    SqlValue::Text(value.into()),
                    SqlValue::Text(id.to_string()),
                ],
            )
            .await
            .expect("install carried reserved property"),
            1
        );
        let snapshot = f.store.series(id).await.unwrap().unwrap();
        let definition = f.store.definition(&snapshot).await.unwrap();
        let prepared = f
            .store
            .prepare_page(&snapshot, &definition, first, 300, 200)
            .expect("prepare a real occurrence");
        let before = complete_state(&f).await;
        let error = f
            .store
            .commit_page(prepared)
            .await
            .expect_err("carried provenance must refuse projection");
        assert!(
            error
                .to_string()
                .contains("schedule display projection refused reserved properties"),
            "{case}: {error}"
        );
        assert_eq!(
            complete_state(&f).await,
            before,
            "{case}: the entire occurrence unit and source row must roll back"
        );
        assert!(f.inventory(id).await.is_empty());
        assert_eq!(
            f.sql(
                "UPDATE notes SET properties=json_remove(properties,?1) WHERE id=?2",
                vec![SqlValue::Text(path), SqlValue::Text(id.to_string())],
            )
            .await
            .expect("remove only the carried reserved property"),
            1
        );
        let retry = f.page(id, first, 200).await;
        assert_eq!(
            retry.created, 1,
            "{case}: real projection route must recover"
        );
        assert!(retry.selected_occurrence_id.is_some());
        assert_eq!(f.inventory(id).await.len(), 1);
        assert_eq!(
            f.store.series(id).await.unwrap().unwrap().revision,
            snapshot.revision + 1,
            "{case}: refused series CAS must not survive rollback"
        );
    }
}

#[tokio::test]
async fn canonical_definition_and_complete_occurrence_key_are_bound() {
    let Some(root) = isolate("canonical_definition_and_complete_occurrence_key_are_bound") else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, _) = f.declare(Some("monthly"), true).await;
    let series = f.store.series(id).await.unwrap().unwrap();
    let original = f.store.definition(&series).await.unwrap();
    let digest = original.digest().unwrap();
    let mut variants = Vec::new();
    let mut v = original.clone();
    v.namespace.push('x');
    variants.push(v);
    let mut v = original.clone();
    v.creator.id.push('x');
    variants.push(v);
    let mut v = original.clone();
    v.recipient.id.push('x');
    variants.push(v);
    let mut v = original.clone();
    v.repeat = Some("daily".into());
    variants.push(v);
    let mut v = original.clone();
    v.monthly_anchor.nanos += 1;
    variants.push(v);
    let mut v = original.clone();
    v.requested_max_lateness_s = Some(3599);
    variants.push(v);
    let mut v = original.clone();
    v.payload = ScheduledPayload::Action {
        action: "stats(namespace=\"other\")".into(),
    };
    variants.push(v);
    let mut v = original.clone();
    v.first_trigger_text.push(' ');
    variants.push(v);
    for variant in variants {
        assert_ne!(variant.digest().unwrap(), digest);
    }
    let canonical = original.canonical_bytes().unwrap();
    assert_eq!(
        DefinitionV3::authenticate(&canonical, &digest).unwrap(),
        original
    );
    assert!(DefinitionV3::authenticate(&canonical, &[0; 32]).is_err());
    let key = OccurrenceKey {
        store_id: Uuid::from_u128(1),
        schedule_id: Uuid::from_u128(2),
        definition_revision: 1,
        scheduled_for: OccurrenceInstant {
            seconds: 100,
            nanos: 123,
        },
    };
    let id = key.id();
    let mut changed = key;
    changed.scheduled_for.nanos += 1;
    assert_ne!(id, changed.id());
    changed = key;
    changed.store_id = Uuid::from_u128(3);
    assert_ne!(id, changed.id());
    changed = key;
    changed.definition_revision = 2;
    assert_ne!(id, changed.id());
    assert!(!key.scheduled_for.within_bound(100_000_001, 0));
    assert!(OccurrenceInstant {
        seconds: 100,
        nanos: 0
    }
    .within_bound(100_000_000, 0));
}

#[tokio::test]
async fn captured_horizon_resumes_all_601_nominal_instants_in_200_row_pages() {
    let Some(root) = isolate("captured_horizon_resumes_all_601_nominal_instants_in_200_row_pages")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, first) = f.declare(Some("every:1s"), true).await;
    let clock = TestClock::fixed(f.store.store_id(), first + 600_000_000);
    let mut total = 0;
    for (pass, expected) in [200, 200, 200, 1].into_iter().enumerate() {
        let page = f
            .page(id, first + 600_000_000 + (pass as i64) * 100_000_000, 200)
            .await;
        assert_eq!(page.created, expected);
        total += page.created;
        let snapshot = f.store.series(id).await.unwrap().unwrap();
        assert_eq!(
            snapshot.horizon,
            Some(OccurrenceInstant::from_micros(first + 600_000_000).unwrap())
        );
    }
    assert_eq!(total, 601);
    let items = f.inventory(id).await;
    assert_eq!(items.len(), 601);
    for (index, item) in items.iter().enumerate() {
        assert_eq!(
            item.key.scheduled_for,
            OccurrenceInstant::from_micros(first + (index as i64) * 1_000_000).unwrap()
        );
        if index < 600 {
            assert_eq!(
                item.terminal.as_ref().unwrap()["reason_code"],
                "superseded_by_catch_up"
            );
        }
    }
    let selected = items.last().unwrap().id;
    let claim = f
        .store
        .claim_candidate(id, selected)
        .await
        .unwrap()
        .unwrap();
    clock.set(first + 600_000_000);
    let intent = match f.store.begin_invocation(&claim).await.unwrap() {
        Admission::Launch(v) => v,
        other => panic!("expected launch: {other:?}"),
    };
    f.store
        .finish(
            &intent,
            ObservedResult::non_invoked(TerminalOutcome::Succeeded, "fixture_completed"),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .len(),
        200
    );
    let mut after = None;
    let mut notices = 0;
    loop {
        let page = f
            .store
            .pending_notices(&f.token(), after, 200)
            .await
            .unwrap();
        after = page.items.last().map(|v| (v.created_at_us, v.id));
        notices += page.items.len();
        if !page.has_more {
            break;
        }
    }
    assert_eq!(notices, 601);
}

#[tokio::test]
async fn exact_200_pre_cut_cancellation_has_no_cut_identity_or_remaining_work() {
    let Some(root) =
        isolate("exact_200_pre_cut_cancellation_has_no_cut_identity_or_remaining_work")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, first) = f.declare(Some("every:1s"), false).await;
    let _clock = TestClock::fixed(f.store.store_id(), first + 200_000_000);
    let cut = f.store.cancel_v3(&f.token(), id).await.unwrap().unwrap();
    assert!(cut.changed);
    assert_eq!(f.page(id, cut.cut_at_us, 200).await.created, 200);
    // A zero-new cleanup is valid whether immediate clamping already completed it or a durable cursor remains.
    let cleanup = f.page(id, cut.cut_at_us, 0).await;
    assert_eq!(cleanup.created, 0);
    assert!(f.store.series(id).await.unwrap().unwrap().next.is_none());
    let items = f.inventory(id).await;
    assert_eq!(items.len(), 200);
    assert!(items
        .iter()
        .all(|o| o.key.scheduled_for < OccurrenceInstant::from_micros(cut.cut_at_us).unwrap()));
    assert!(items
        .iter()
        .all(|o| o.terminal.as_ref().unwrap()["outcome"] == "cancelled"));
    assert!(f
        .store
        .series_work_page(WorkPurpose::AccountingOrRecovery, None, 200, 0)
        .await
        .unwrap()
        .items
        .is_empty());
    assert!(f
        .store
        .series_work_page(WorkPurpose::AccountingOrRecovery, None, 200, 200)
        .await
        .unwrap()
        .items
        .is_empty());
}

#[tokio::test]
async fn zero_new_prepared_window_change_is_committed_without_an_occurrence() {
    let Some(root) = isolate("zero_new_prepared_window_change_is_committed_without_an_occurrence")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, first) = f.declare(Some("every:1s"), true).await;
    let _clock = TestClock::fixed(f.store.store_id(), first + 20_000_000);
    let before = f.store.series(id).await.unwrap().unwrap();
    assert_eq!(f.page(id, first + 20_000_000, 0).await.created, 0);
    let after = f.store.series(id).await.unwrap().unwrap();
    assert_eq!(after.revision, before.revision + 1);
    assert_eq!(
        after.horizon,
        Some(OccurrenceInstant::from_micros(first + 20_000_000).unwrap())
    );
    assert!(f.inventory(id).await.is_empty());
}

#[tokio::test]
async fn one_shot_ready_before_claim_remains_protected_then_settles() {
    let Some(root) = isolate("one_shot_ready_before_claim_remains_protected_then_settles") else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, first) = f.declare(None, false).await;
    let _clock = TestClock::fixed(f.store.store_id(), first);
    let page = f.page(id, first, 200).await;
    assert_eq!(page.created, 1);
    let state = f.store.series(id).await.unwrap().unwrap();
    assert!(state.next.is_none());
    assert_eq!(state.state, "active");
    for statement in [
        "UPDATE notes SET namespace='moved' WHERE id=?1",
        "UPDATE notes SET deleted_at=1 WHERE id=?1",
        "DELETE FROM notes WHERE id=?1",
    ] {
        let error = f
            .sql(statement, vec![SqlValue::Text(id.to_string())])
            .await
            .expect_err("unsettled one-shot lifecycle guard");
        assert!(
            error
                .to_string()
                .contains("cancel active version-3 schedule"),
            "{error}"
        );
    }
    let ready = f
        .store
        .series_work_page(WorkPurpose::AccountingOrRecovery, None, 200, 0)
        .await
        .unwrap();
    assert_eq!(ready.items.len(), 1);
    assert_eq!(ready.items[0].ready_unclaimed, page.selected_occurrence_id);
    let claim = f
        .store
        .claim_candidate(id, page.selected_occurrence_id.unwrap())
        .await
        .unwrap()
        .unwrap();
    let intent = match f.store.begin_invocation(&claim).await.unwrap() {
        Admission::Launch(v) => v,
        other => panic!("{other:?}"),
    };
    f.store
        .finish(
            &intent,
            ObservedResult::non_invoked(TerminalOutcome::Succeeded, "completed"),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        f.store.series(id).await.unwrap().unwrap().state,
        "exhausted"
    );
    assert_eq!(
        f.sql(
            "DELETE FROM notes WHERE id=?1",
            vec![SqlValue::Text(id.to_string())]
        )
        .await
        .unwrap(),
        1
    );
    assert_eq!(f.inventory(id).await.len(), 1);
}

#[tokio::test]
async fn final_admission_is_fresh_inclusive_precise_and_fallible() {
    let Some(root) = isolate("final_admission_is_fresh_inclusive_precise_and_fallible") else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, first) = f.declare(None, true).await;
    let clock = TestClock::fixed(f.store.store_id(), first);
    let selected = f.page(id, first, 200).await.selected_occurrence_id.unwrap();
    let claim = f
        .store
        .claim_candidate(id, selected)
        .await
        .unwrap()
        .unwrap();
    clock.sequence(vec![Err("injected clock read")]);
    let before = f.rows().await;
    let error = f
        .store
        .begin_invocation(&claim)
        .await
        .expect_err("clock failure");
    assert!(error.to_string().contains("injected clock read"));
    assert_eq!(f.rows().await, before);
    clock.set(first + 3_600_000_000);
    assert!(f.store.renew_claim(&claim).await.unwrap());
    clock.sequence(vec![Ok(first + 3_600_000_000)]);
    let intent = match f.store.begin_invocation(&claim).await.unwrap() {
        Admission::Launch(v) => v,
        other => panic!("inclusive deadline: {other:?}"),
    };
    assert_eq!(intent.admission_at_us(), first + 3_600_000_000);
    assert_eq!(clock.count(), 1);
    clock.set(first + 3_600_000_001);
    f.store
        .finish(
            &intent,
            ObservedResult::non_invoked(TerminalOutcome::Succeeded, "complete"),
        )
        .await
        .unwrap();
    let (late, late_first) = f.declare(None, true).await;
    clock.set(late_first);
    let selected = f
        .page(late, late_first, 200)
        .await
        .selected_occurrence_id
        .unwrap();
    let claim = f
        .store
        .claim_candidate(late, selected)
        .await
        .unwrap()
        .unwrap();
    clock.set(late_first + 3_600_000_001);
    assert!(f.store.renew_claim(&claim).await.unwrap());
    assert!(
        matches!(f.store.begin_invocation(&claim).await.unwrap(),Admission::Settled(v) if v.outcome=="missed")
    );
    let (back, back_first) = f.declare(None, true).await;
    clock.set(back_first);
    let selected = f
        .page(back, back_first, 200)
        .await
        .selected_occurrence_id
        .unwrap();
    let claim = f
        .store
        .claim_candidate(back, selected)
        .await
        .unwrap()
        .unwrap();
    clock.set(back_first - 1);
    assert!(
        matches!(f.store.begin_invocation(&claim).await.unwrap(),Admission::Settled(v) if v.reason_code=="clock_order_unavailable")
    );
    let (inside, scheduled) = f.declare(None, true).await;
    clock.set(scheduled + 10);
    let selected = f
        .page(inside, scheduled + 10, 200)
        .await
        .selected_occurrence_id
        .unwrap();
    let claim = f
        .store
        .claim_candidate(inside, selected)
        .await
        .unwrap()
        .unwrap();
    clock.set(scheduled + 9);
    let refused = match f.store.begin_invocation(&claim).await.unwrap() {
        Admission::Settled(v) => v,
        other => panic!("intra-window backstep must refuse: {other:?}"),
    };
    assert_eq!(refused.reason_code, "clock_order_unavailable");
    assert_eq!(refused.final_decision_at_us, Some(scheduled + 9));
    assert_eq!(claim.occurrence().selection_at_us, scheduled + 10);
    let row =
        f.rt.sql()
            .reader()
            .await
            .unwrap()
            .query_row(SqlStatement::new(
                "SELECT launch_intent_id FROM scheduled_attempts_v3 WHERE occurrence_id=?1",
                vec![SqlValue::Text(selected.to_string())],
            ))
            .await
            .unwrap()
            .unwrap();
    assert!(row.opt_uuid("launch_intent_id").unwrap().is_none());
    let (matching, scheduled) = f.declare(None, true).await;
    clock.set(scheduled + 10);
    let selected = f
        .page(matching, scheduled + 10, 200)
        .await
        .selected_occurrence_id
        .unwrap();
    let claim = f
        .store
        .claim_candidate(matching, selected)
        .await
        .unwrap()
        .unwrap();
    clock.sequence(vec![Ok(scheduled + 10)]);
    assert!(
        matches!(f.store.begin_invocation(&claim).await.unwrap(),Admission::Launch(v) if v.admission_at_us()==scheduled+10)
    );
    assert_eq!(clock.count(), 1);
}

#[tokio::test]
async fn claims_intents_and_terminal_outbox_faults_rollback_exact_rows() {
    let Some(root) = isolate("claims_intents_and_terminal_outbox_faults_rollback_exact_rows")
    else {
        return;
    };
    for (index, table) in [
        "scheduled_occurrences_v3",
        "scheduled_receipts_v3",
        "scheduled_notice_outbox_v3",
    ]
    .into_iter()
    .enumerate()
    {
        let case = root.join(index.to_string());
        std::fs::create_dir_all(&case).unwrap();
        let f = Fixture::open(&case);
        let (id, first) = f.declare(Some("every:1s"), true).await;
        let _clock = TestClock::fixed(f.store.store_id(), first + 2_000_000);
        f.sql(format!("CREATE TRIGGER injected_page BEFORE INSERT ON {table} BEGIN SELECT RAISE(FAIL,'injected occurrence page'); END"),vec![]).await.unwrap();
        let before = f.rows().await;
        let snapshot = f.store.series(id).await.unwrap().unwrap();
        let definition = f.store.definition(&snapshot).await.unwrap();
        let prepared = f
            .store
            .prepare_page(&snapshot, &definition, first + 2_000_000, 300, 200)
            .unwrap();
        let error = f
            .store
            .commit_page(prepared)
            .await
            .expect_err("actual writer fault");
        assert!(
            error.to_string().contains("injected occurrence page"),
            "{error}"
        );
        assert_eq!(f.rows().await, before);
        f.sql("DROP TRIGGER injected_page", vec![]).await.unwrap();
        assert_eq!(f.page(id, first + 2_000_000, 200).await.created, 3);
    }
    let case = root.join("claim");
    std::fs::create_dir_all(&case).unwrap();
    let f = Fixture::open(&case);
    let (id, first) = f.declare(None, false).await;
    let _clock = TestClock::fixed(f.store.store_id(), first);
    let selected = f.page(id, first, 200).await.selected_occurrence_id.unwrap();
    f.sql("CREATE TRIGGER injected_claim BEFORE INSERT ON scheduled_attempts_v3 BEGIN SELECT RAISE(FAIL,'injected claim'); END",vec![]).await.unwrap();
    let before = f.rows().await;
    assert!(f
        .store
        .claim_candidate(id, selected)
        .await
        .unwrap_err()
        .to_string()
        .contains("injected claim"));
    assert_eq!(f.rows().await, before);
    f.sql("DROP TRIGGER injected_claim", vec![]).await.unwrap();
    let claim = f
        .store
        .claim_candidate(id, selected)
        .await
        .unwrap()
        .unwrap();
    f.sql("CREATE TRIGGER injected_intent BEFORE UPDATE OF launch_intent_id ON scheduled_attempts_v3 BEGIN SELECT RAISE(FAIL,'injected intent'); END",vec![]).await.unwrap();
    let before = f.rows().await;
    assert!(f
        .store
        .begin_invocation(&claim)
        .await
        .unwrap_err()
        .to_string()
        .contains("injected intent"));
    assert_eq!(f.rows().await, before);
    f.sql("DROP TRIGGER injected_intent", vec![]).await.unwrap();
    let intent = match f.store.begin_invocation(&claim).await.unwrap() {
        Admission::Launch(v) => v,
        other => panic!("{other:?}"),
    };
    f.sql("CREATE TRIGGER injected_terminal BEFORE INSERT ON scheduled_notice_outbox_v3 BEGIN SELECT RAISE(FAIL,'injected terminal outbox'); END",vec![]).await.unwrap();
    let before = f.rows().await;
    let result = ObservedResult::non_invoked(TerminalOutcome::Succeeded, "actual_result");
    assert!(f
        .store
        .finish(&intent, result.clone())
        .await
        .unwrap_err()
        .to_string()
        .contains("injected terminal outbox"));
    assert_eq!(f.rows().await, before);
    f.sql("DROP TRIGGER injected_terminal", vec![])
        .await
        .unwrap();
    assert!(f.store.finish(&intent, result).await.unwrap().is_some());
}

#[tokio::test]
async fn expired_intent_suspends_without_replay_and_reconciliation_adds_no_notice() {
    let Some(root) =
        isolate("expired_intent_suspends_without_replay_and_reconciliation_adds_no_notice")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, first) = f.declare(Some("every:1s"), true).await;
    let clock = TestClock::fixed(f.store.store_id(), first);
    let selected = f.page(id, first, 200).await.selected_occurrence_id.unwrap();
    let claim = f
        .store
        .claim_candidate(id, selected)
        .await
        .unwrap()
        .unwrap();
    clock.set(first + 300_000_000);
    let work = f
        .store
        .series_work_page(WorkPurpose::AccountingOrRecovery, None, 200, 0)
        .await
        .unwrap()
        .items
        .remove(0);
    assert!(
        matches!(f.store.recover_expired(&work).await.unwrap(),Recovery::Reclaimable {occurrence_id,..} if occurrence_id==selected)
    );
    let replacement = f
        .store
        .claim_candidate(id, selected)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replacement.invocation_id(), claim.invocation_id());
    clock.set(first + 300_000_000);
    let intent = match f.store.begin_invocation(&replacement).await.unwrap() {
        Admission::Launch(v) => v,
        other => panic!("{other:?}"),
    };
    clock.set(first + 600_000_000);
    let work = f
        .store
        .series_work_page(WorkPurpose::AccountingOrRecovery, None, 200, 0)
        .await
        .unwrap()
        .items
        .remove(0);
    assert!(
        matches!(f.store.recover_expired(&work).await.unwrap(),Recovery::Settled(v) if v.outcome=="indeterminate")
    );
    assert!(f
        .store
        .claim_candidate(id, selected)
        .await
        .unwrap()
        .is_none());
    let notices = f
        .store
        .pending_notices(&f.token(), None, 200)
        .await
        .unwrap()
        .items
        .len();
    assert_eq!(notices, 1);
    assert_eq!(f.page(id, first + 3_000_000, 200).await.created, 3);
    assert!(f
        .inventory(id)
        .await
        .iter()
        .skip(1)
        .all(|o| o.terminal.as_ref().unwrap()["reason_code"] == "unresolved_prior_launch"));
    let actual = ObservedResult::non_invoked(TerminalOutcome::Succeeded, "late_actual_response");
    assert!(f
        .store
        .append_verified_reconciliation(&intent, actual)
        .await
        .unwrap());
    let receipts = f
        .store
        .receipt(&f.token(), selected, None, 200)
        .await
        .unwrap();
    assert_eq!(receipts.items.len(), 2);
    assert_eq!(
        receipts
            .items
            .iter()
            .find(|r| r["record_kind"] == "terminal")
            .unwrap()["outcome"],
        "indeterminate"
    );
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .len(),
        notices + 3
    );
    assert!(f
        .store
        .series(id)
        .await
        .unwrap()
        .unwrap()
        .suspended_invocation_id
        .is_none());
}

#[tokio::test]
async fn production_schema_matches_namespace_fixture_and_real_move_retains_history() {
    let Some(root) =
        isolate("production_schema_matches_namespace_fixture_and_real_move_retains_history")
    else {
        return;
    };
    assert_eq!(
        include_str!("../sql/occurrence-schema.sql"),
        include_str!("../../khive-db/sql/namespace-schedule-v3-fixture.sql")
    );
    let f = Fixture::open(&root);
    let (id, first) = f.declare(None, false).await;
    let _clock = TestClock::fixed(f.store.store_id(), first + 1_000_000);
    f.store.cancel_v3(&f.token(), id).await.unwrap().unwrap();
    f.page(id, first + 1_000_000, 200).await;
    let before = f.rows().await;
    let request = khive_db::namespace_move::MoveRequest::new(
        NS,
        vec![khive_db::namespace_move::MoveRoute {
            class: khive_db::namespace_move::SubjectClass::Note("scheduled_event".into()),
            target: "moved".into(),
        }],
    );
    f.rt.backend()
        .pool()
        .writer()
        .expect("owned namespace writer")
        .transaction(|conn| {
            khive_db::namespace_move::move_namespace(conn, &request)
                .map_err(|e| khive_db::SqliteError::InvalidData(e.to_string()))
        })
        .expect("real namespace move");
    assert_eq!(f.rows().await, before);
    assert_eq!(f.inventory(id).await.len(), 1);
    let moved = f.rt.authorize(Namespace::parse("moved").unwrap()).unwrap();
    assert!(f
        .store
        .occurrences(&moved, id, None, None, 200)
        .await
        .unwrap()
        .items
        .is_empty());
}

#[tokio::test]
async fn cold_process_resumes_pages_after_last_cancelled_display_note_is_deleted() {
    let name = "cold_process_resumes_pages_after_last_cancelled_display_note_is_deleted";
    if std::env::var(CHILD_CASE).as_deref() != Ok(name) {
        let root = tempfile::tempdir().unwrap();
        for removal in ["hard", "soft", "move"] {
            let case = root.path().join(removal);
            std::fs::create_dir_all(&case).unwrap();
            healthy(child(name, &case, Some("first")));
            healthy(child(name, &case, Some("second")));
            healthy(child(name, &case, Some("last")));
        }
        return;
    }
    let root = PathBuf::from(std::env::var_os(CHILD_ROOT).unwrap());
    let phase = std::env::var(CHILD_PHASE).unwrap();
    let f = Fixture::open(&root);
    if phase == "first" {
        let (id, first) = f.declare(Some("every:1s"), false).await;
        let _clock = TestClock::fixed(f.store.store_id(), first + 601_000_000);
        f.store.cancel_v3(&f.token(), id).await.unwrap().unwrap();
        assert_eq!(f.page(id, first + 601_000_000, 200).await.created, 200);
        match root.file_name().unwrap().to_str().unwrap() {
            "hard" => {
                assert_eq!(
                    f.sql(
                        "DELETE FROM notes WHERE id=?1",
                        vec![SqlValue::Text(id.to_string())]
                    )
                    .await
                    .unwrap(),
                    1
                );
            }
            "soft" => {
                assert_eq!(
                    f.sql(
                        "UPDATE notes SET deleted_at=1 WHERE id=?1",
                        vec![SqlValue::Text(id.to_string())]
                    )
                    .await
                    .unwrap(),
                    1
                );
            }
            "move" => {
                let request = khive_db::namespace_move::MoveRequest::new(
                    NS,
                    vec![khive_db::namespace_move::MoveRoute {
                        class: khive_db::namespace_move::SubjectClass::Note(
                            "scheduled_event".into(),
                        ),
                        target: "moved".into(),
                    }],
                );
                f.rt.backend()
                    .pool()
                    .writer()
                    .unwrap()
                    .transaction(|conn| {
                        khive_db::namespace_move::move_namespace(conn, &request)
                            .map_err(|e| khive_db::SqliteError::InvalidData(e.to_string()))
                    })
                    .unwrap();
            }
            _ => panic!("closed lifecycle fixture"),
        }
        std::fs::write(
            root.join("rows.json"),
            serde_json::to_vec(&f.rows().await).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("phase.json"),
            json!({"id":id,"first":first,"store":f.store.store_id()}).to_string(),
        )
        .unwrap();
    } else {
        let state: Value =
            serde_json::from_slice(&std::fs::read(root.join("phase.json")).unwrap()).unwrap();
        let id = Uuid::parse_str(state["id"].as_str().unwrap()).unwrap();
        let first = state["first"].as_i64().unwrap();
        assert_eq!(
            f.store.store_id().to_string(),
            state["store"].as_str().unwrap()
        );
        let _clock = TestClock::fixed(f.store.store_id(), first + 900_000_000);
        let stored: Value =
            serde_json::from_slice(&std::fs::read(root.join("rows.json")).unwrap()).unwrap();
        assert_eq!(
            f.rows().await,
            stored,
            "restart/read must preserve complete existing rows"
        );
        let passes = if phase == "second" { 1 } else { 2 };
        for _ in 0..passes {
            let work = f
                .store
                .series_work_page(WorkPurpose::AccountingOrRecovery, None, 200, 200)
                .await
                .unwrap();
            for item in work.items {
                assert_eq!(item.series.namespace, NS);
                let page = f
                    .page(item.series.schedule_id, first + 900_000_000, 200)
                    .await;
                assert!(page.created <= 200);
            }
        }
        let inventory = f.inventory(id).await;
        assert_eq!(
            inventory.len(),
            if phase == "second" { 400 } else { 601 },
            "cold auxiliary-discovery final inventory census"
        );
        for (index, item) in inventory.iter().enumerate() {
            assert_eq!(
                item.key.scheduled_for,
                OccurrenceInstant::from_micros(first + index as i64 * 1_000_000).unwrap()
            );
            assert_eq!(item.namespace, NS);
            assert_eq!(item.terminal.as_ref().unwrap()["outcome"], "cancelled");
        }
        let after = f.rows().await;
        for table in [
            "scheduled_definitions_v3",
            "scheduled_occurrences_v3",
            "scheduled_receipts_v3",
            "scheduled_notice_outbox_v3",
        ] {
            let prior = stored[table].as_array().unwrap();
            assert_eq!(&after[table].as_array().unwrap()[..prior.len()], prior);
        }
        std::fs::write(root.join("rows.json"), serde_json::to_vec(&after).unwrap()).unwrap();
        if phase == "last" {
            assert!(f
                .store
                .series_work_page(WorkPurpose::AccountingOrRecovery, None, 200, 0)
                .await
                .unwrap()
                .items
                .is_empty());
        }
    }
}

#[tokio::test]
async fn cold_commit_before_claim_recovers_same_exhausted_candidate() {
    let name = "cold_commit_before_claim_recovers_same_exhausted_candidate";
    if std::env::var(CHILD_CASE).as_deref() != Ok(name) {
        let root = tempfile::tempdir().unwrap();
        let crashed = child(name, root.path(), Some("crash"));
        assert_eq!(crashed.status.code(), Some(71), "{:?}", crashed);
        assert!(root.path().join("ready.json").exists());
        healthy(child(name, root.path(), Some("resume")));
        return;
    }
    let root = PathBuf::from(std::env::var_os(CHILD_ROOT).unwrap());
    let f = Fixture::open(&root);
    if std::env::var(CHILD_PHASE).as_deref() == Ok("crash") {
        let (id, first) = f.declare(None, true).await;
        let _clock = TestClock::fixed(f.store.store_id(), first);
        let selected = f.page(id, first, 200).await.selected_occurrence_id.unwrap();
        assert!(f.store.series(id).await.unwrap().unwrap().next.is_none());
        let rows = f.rows().await;
        assert_eq!(rows["scheduled_attempts_v3"].as_array().unwrap().len(), 0);
        assert_eq!(rows["scheduled_receipts_v3"].as_array().unwrap().len(), 0);
        std::fs::write(
            root.join("ready.json"),
            json!({"id":id,"first":first,"selected":selected}).to_string(),
        )
        .unwrap();
        std::process::exit(71);
    }
    let state: Value =
        serde_json::from_slice(&std::fs::read(root.join("ready.json")).unwrap()).unwrap();
    let id = Uuid::parse_str(state["id"].as_str().unwrap()).unwrap();
    let selected = Uuid::parse_str(state["selected"].as_str().unwrap()).unwrap();
    let first = state["first"].as_i64().unwrap();
    let _clock = TestClock::fixed(f.store.store_id(), first);
    let work = f
        .store
        .series_work_page(WorkPurpose::AccountingOrRecovery, None, 200, 0)
        .await
        .unwrap();
    assert_eq!(work.items.len(), 1);
    assert_eq!(work.items[0].ready_unclaimed, Some(selected));
    let claim = f
        .store
        .claim_candidate(id, selected)
        .await
        .unwrap()
        .unwrap();
    let intent = match f.store.begin_invocation(&claim).await.unwrap() {
        Admission::Launch(v) => v,
        other => panic!("{other:?}"),
    };
    f.store
        .finish(
            &intent,
            ObservedResult::non_invoked(TerminalOutcome::Succeeded, "cold_result"),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.inventory(id).await.len(), 1);
    assert_eq!(
        f.store
            .pending_notices(&f.token(), None, 200)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[test]
fn canonical_bytes_and_binary_occurrence_identity_have_independent_goldens() {
    let actor = DefinitionActor {
        kind: "actor".into(),
        id: "fixture".into(),
    };
    let instant = OccurrenceInstant {
        seconds: 100,
        nanos: 123,
    };
    let definition = DefinitionV3 {
        schema_version: 3,
        schedule_id: Uuid::from_u128(2),
        definition_revision: 1,
        creator: actor.clone(),
        namespace: "ns".into(),
        payload: ScheduledPayload::Action {
            action: "stats()".into(),
        },
        first_trigger_text: "1970-01-01T00:01:40.000000123Z".into(),
        first_trigger: instant,
        repeat: None,
        monthly_anchor: instant,
        misfire_policy: "skip".into(),
        requested_max_lateness_s: None,
        recipient: actor,
    };
    const EXPECTED:&[u8]=br#"{"schema_version":3,"schedule_id":"00000000-0000-0000-0000-000000000002","definition_revision":1,"creator":{"kind":"actor","id":"fixture"},"namespace":"ns","payload":{"kind":"action","action":"stats()"},"first_trigger_text":"1970-01-01T00:01:40.000000123Z","first_trigger":{"seconds":100,"nanos":123},"repeat":null,"monthly_anchor":{"seconds":100,"nanos":123},"misfire_policy":"skip","requested_max_lateness_s":null,"recipient":{"kind":"actor","id":"fixture"}}"#;
    assert_eq!(definition.canonical_bytes().unwrap(), EXPECTED);
    let key = OccurrenceKey {
        store_id: Uuid::from_u128(1),
        schedule_id: Uuid::from_u128(2),
        definition_revision: 1,
        scheduled_for: instant,
    };
    assert_eq!(key.id().to_string(), "09838e54-156e-51d5-9ddc-0e58b64fa97a");
}

#[test]
fn recurrence_preserves_original_calendar_anchor_and_fractional_utc_identity() {
    let parse = |text: &str| text.parse::<DateTime<Utc>>().unwrap();
    let anchor = parse("2024-01-31T12:34:56.123456789Z");
    let monthly = crate::repeat::parse_repeat("monthly").unwrap();
    let mut current = anchor;
    for expected in [
        "2024-02-29T12:34:56.123456789Z",
        "2024-03-31T12:34:56.123456789Z",
        "2024-04-30T12:34:56.123456789Z",
    ] {
        current = next_nominal(&monthly, anchor, current).unwrap();
        assert_eq!(current, parse(expected));
    }
    for (repeat, expected) in [
        ("daily", "2024-02-01T12:34:56.123456789Z"),
        ("weekly", "2024-02-07T12:34:56.123456789Z"),
        ("every:3s", "2024-01-31T12:34:59.123456789Z"),
        ("0 13 * * *", "2024-01-31T13:00:00Z"),
    ] {
        let repeat = crate::repeat::parse_repeat(repeat).unwrap();
        assert_eq!(
            next_nominal(&repeat, anchor, anchor).unwrap(),
            parse(expected)
        );
    }
    let local = parse("2024-01-31T14:34:56.123456789+02:00");
    let make = |at| OccurrenceKey {
        store_id: Uuid::from_u128(1),
        schedule_id: Uuid::from_u128(2),
        definition_revision: 1,
        scheduled_for: OccurrenceInstant::from_datetime(at),
    };
    assert_eq!(make(anchor).id(), make(local).id());
}

#[tokio::test]
async fn skip_selects_only_original_source_and_overlap_accounts_every_new_instant() {
    let Some(root) =
        isolate("skip_selects_only_original_source_and_overlap_accounts_every_new_instant")
    else {
        return;
    };
    let f = Fixture::open(&root);
    let (id, first) = f.declare(Some("every:1s"), false).await;
    let clock = TestClock::fixed(f.store.store_id(), first + 20_000_000);
    let page = f.page(id, first + 20_000_000, 200).await;
    assert_eq!(page.created, 21);
    let items = f.inventory(id).await;
    assert_eq!(page.selected_occurrence_id, Some(items[0].id));
    assert!(items[0].terminal.is_none());
    assert!(items
        .iter()
        .skip(1)
        .all(|o| o.terminal.as_ref().unwrap()["outcome"] == "missed"));
    let claim = f
        .store
        .claim_candidate(id, items[0].id)
        .await
        .unwrap()
        .unwrap();
    let intent = match f.store.begin_invocation(&claim).await.unwrap() {
        Admission::Launch(v) => v,
        other => panic!("{other:?}"),
    };
    clock.set(first + 220_000_000);
    assert_eq!(f.page(id, first + 220_000_000, 200).await.created, 200);
    let items = f.inventory(id).await;
    assert_eq!(items.len(), 221);
    assert!(items
        .iter()
        .skip(21)
        .all(|o| o.terminal.as_ref().unwrap()["reason_code"] == "overlap"));
    f.store
        .finish(
            &intent,
            ObservedResult::non_invoked(TerminalOutcome::Succeeded, "source_finished"),
        )
        .await
        .unwrap()
        .unwrap();
}
