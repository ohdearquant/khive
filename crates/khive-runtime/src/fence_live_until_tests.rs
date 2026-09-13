//! ADR-174 Amendment 8 acceptance: the single-head fence carries `live_until`,
//! and both routes resolve it through one predicate.
//!
//! Arm 1 is the load-bearing one. One head, one expired deadline, two routes,
//! and the two refusals are compared against each other field by field rather
//! than each against its own expectation: a duplicated predicate passes a pair
//! of separate assertions and fails this one.

use super::*;

use crate::note_write::{NoteFences, NoteWriteGuard};
use crate::pack::VerbRegistryBuilder;
use crate::{StreamAppendSpec, StreamBatchMember, StreamBatchRefusal, StreamObservation};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use khive_storage::{SqlReader, SqlWriter, StorageResult};
use serde_json::Value;

fn deadline(offset: chrono::Duration) -> String {
    (chrono::Utc::now() + offset).to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn expired() -> String {
    deadline(-chrono::Duration::hours(1))
}

fn live() -> String {
    deadline(chrono::Duration::hours(1))
}

async fn head(runtime: &KhiveRuntime, token: &NamespaceToken, key: &str, content: Value) -> Note {
    runtime
        .create_note_with_options(
            token,
            "head",
            None,
            &content.to_string(),
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some(key.into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .0
}

fn append(stream: &str) -> StreamBatchMember {
    StreamBatchMember::Append(StreamAppendSpec {
        stream: stream.into(),
        record: json!({"event": "fence-live-until"}),
        expected_seq: None,
        embed: Some(false),
        embedding_model: None,
        note_kind: "observation".into(),
        tags: None,
        fence: None,
    })
}

fn observation(key: &str, version: Option<i64>, live_until: Option<&str>) -> StreamObservation {
    StreamObservation {
        key: key.into(),
        kind: "head".into(),
        version,
        id: None,
        live_until: live_until.map(Into::into),
    }
}

async fn observed_batch(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &crate::VerbRegistry,
    stream: &str,
    observed: Vec<StreamObservation>,
) -> RuntimeResult<Result<Vec<Value>, StreamBatchRefusal>> {
    runtime
        .stream_batch_atomic(token, vec![append(stream)], None, observed, registry)
        .await
}

async fn fenced_update(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    target: &Note,
    fence: Value,
) -> AtomicRunOutcome {
    let plan = crate::atomic_prepare::prepare_update(
        runtime,
        token,
        &json!({"id": target.id, "content": "{\"written\":true}", "fence": fence}),
        None,
    )
    .await
    .unwrap();
    run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .unwrap()
}

fn refusal(outcome: AtomicRunOutcome) -> Value {
    let AtomicRunOutcome::RolledBack {
        failure: crate::atomic_runner::AtomicOpFailure::NoteConflict(conflict),
        ..
    } = outcome
    else {
        panic!("expected a fence refusal, got {outcome:?}");
    };
    serde_json::to_value(conflict.into_error().details().unwrap()).unwrap()
}

fn batch_refusal(result: RuntimeResult<Result<Vec<Value>, StreamBatchRefusal>>) -> Value {
    let Err(RuntimeError::Khive(error)) = result else {
        panic!("expected a structured observation refusal");
    };
    serde_json::to_value(error.details().expect("conflict details")).unwrap()
}

fn field_names(value: &Value) -> Vec<String> {
    let mut names: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    names.sort();
    names
}

async fn version_of(runtime: &KhiveRuntime, token: &NamespaceToken, note: &Note) -> i64 {
    runtime
        .notes(token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .unwrap()
        .version
}

/// Arm 1. The guarantee must not differ by route, so the fixture is one head
/// and the assertion is an equality between the two refusals.
#[tokio::test]
async fn fence_live_until_arm1_both_routes_refuse_one_expired_head_with_the_same_evidence() {
    let (runtime, token, _) = fixture();
    let registry = VerbRegistryBuilder::new().build().unwrap();
    let expires_at = expired();
    head(
        &runtime,
        &token,
        "lease/shared",
        json!({"expires_at": expires_at}),
    )
    .await;
    let target = head(&runtime, &token, "target/shared", json!({})).await;

    let observed = batch_refusal(
        observed_batch(
            &runtime,
            &token,
            &registry,
            "arm1",
            vec![observation("lease/shared", Some(1), Some("expires_at"))],
        )
        .await,
    );
    let fenced = refusal(
        fenced_update(
            &runtime,
            &token,
            &target,
            json!([{"kind":"head","key":"lease/shared","expected_version":1,"live_until":"expires_at"}]),
        )
        .await,
    );

    assert_eq!(field_names(&observed), field_names(&fenced));
    for field in [
        "reason", "key", "kind", "version", "field", "index", "value",
    ] {
        assert_eq!(
            observed[field], fenced[field],
            "{field} differs between the two routes: {observed} vs {fenced}"
        );
    }
    assert_eq!(observed["reason"], "expired");
    assert_eq!(observed["key"], "lease/shared");
    assert_eq!(observed["field"], "expires_at");
    assert_eq!(observed["version"], "1");
    assert_eq!(observed["value"], json!(format!("\"{expires_at}\"")));
    let read = chrono::DateTime::parse_from_rfc3339(&expires_at).unwrap();
    for route in [&observed, &fenced] {
        let now = chrono::DateTime::parse_from_rfc3339(route["now"].as_str().unwrap()).unwrap();
        assert!(
            now > read,
            "the clock must postdate the deadline it refused"
        );
    }
    assert_eq!(version_of(&runtime, &token, &target).await, 1);
}

/// Arm 2. The same fixture with a live deadline admits, on both routes, so the
/// predicate is not refusing everything.
#[tokio::test]
async fn fence_live_until_arm2_a_future_deadline_admits_on_both_routes() {
    let (runtime, token, _) = fixture();
    let registry = VerbRegistryBuilder::new().build().unwrap();
    head(
        &runtime,
        &token,
        "lease/live",
        json!({"expires_at": live()}),
    )
    .await;
    let target = head(&runtime, &token, "target/live", json!({})).await;

    let values = observed_batch(
        &runtime,
        &token,
        &registry,
        "arm2",
        vec![observation("lease/live", Some(1), Some("expires_at"))],
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(values.len(), 1);

    let outcome = fenced_update(
        &runtime,
        &token,
        &target,
        json!([{"kind":"head","key":"lease/live","expected_version":1,"live_until":"expires_at"}]),
    )
    .await;
    assert!(
        matches!(outcome, AtomicRunOutcome::Committed { .. }),
        "a live deadline must commit: {outcome:?}"
    );
    assert_eq!(version_of(&runtime, &token, &target).await, 2);
}

/// Arm 3. A path that resolves to nothing and a path that resolves to a
/// non-timestamp both refuse the same way, naming the JSON type and never the
/// value: the path is caller-chosen, so echoing it would read an arbitrary
/// field of the document back out.
#[tokio::test]
async fn fence_live_until_arm3_an_unreadable_path_reports_its_type_and_not_its_value() {
    let (runtime, token, _) = fixture();
    let registry = VerbRegistryBuilder::new().build().unwrap();
    head(
        &runtime,
        &token,
        "lease/unreadable",
        json!({"expires_at": 12345, "secret": "not-a-timestamp"}),
    )
    .await;
    let target = head(&runtime, &token, "target/unreadable", json!({})).await;

    for (path, value_type) in [
        ("expires_at", "number"),
        ("secret", "string"),
        ("missing", "absent"),
        ("expires_at.nested", "absent"),
    ] {
        let observed = batch_refusal(
            observed_batch(
                &runtime,
                &token,
                &registry,
                "arm3",
                vec![observation("lease/unreadable", Some(1), Some(path))],
            )
            .await,
        );
        let fenced = refusal(
            fenced_update(
                &runtime,
                &token,
                &target,
                json!([{"kind":"head","key":"lease/unreadable","expected_version":1,"live_until":path}]),
            )
            .await,
        );
        assert_eq!(field_names(&observed), field_names(&fenced), "{path}");
        for route in [&observed, &fenced] {
            assert_eq!(route["reason"], "live_until_unreadable", "{path}");
            assert_eq!(route["value_type"], value_type, "{path}");
            assert_eq!(route["field"], path);
            assert!(route.get("value").is_none(), "{path}: {route}");
            let rendered = route.to_string();
            assert!(!rendered.contains("12345"), "{path}: {rendered}");
            assert!(!rendered.contains("not-a-timestamp"), "{path}: {rendered}");
        }
    }
    assert_eq!(version_of(&runtime, &token, &target).await, 1);
}

/// Arm 4. An expired refusal carries the deadline it read and the clock it
/// compared, through a dotted path so the resolver is exercised past the first
/// level.
#[tokio::test]
async fn fence_live_until_arm4_an_expired_refusal_carries_the_deadline_and_the_clock() {
    let (runtime, token, _) = fixture();
    let expires_at = expired();
    head(
        &runtime,
        &token,
        "lease/nested",
        json!({"lease": {"expires_at": expires_at}}),
    )
    .await;
    let target = head(&runtime, &token, "target/nested", json!({})).await;
    let before = chrono::Utc::now();
    let fenced = refusal(
        fenced_update(
            &runtime,
            &token,
            &target,
            json!([{"kind":"head","key":"lease/nested","expected_version":1,"live_until":"lease.expires_at"}]),
        )
        .await,
    );
    assert_eq!(fenced["reason"], "expired");
    assert_eq!(fenced["field"], "lease.expires_at");
    assert_eq!(fenced["value"], json!(format!("\"{expires_at}\"")));
    let now = chrono::DateTime::parse_from_rfc3339(fenced["now"].as_str().unwrap()).unwrap();
    assert!(now >= before, "the clock is read inside this write");
    assert!(now <= chrono::Utc::now());
}

/// Arm 5. `live_until` beside an absence assertion has no document to read a
/// deadline out of, so the pairing is refused rather than interpreted, and it
/// is refused while the call is still being shaped: no plan is produced and no
/// writer is requested.
#[tokio::test]
async fn fence_live_until_arm5_an_absence_assertion_cannot_carry_a_deadline() {
    let (runtime, token, _) = fixture();
    let target = head(&runtime, &token, "target/pairing", json!({})).await;
    for fence in [
        json!({"kind":"head","key":"lease/pairing","expected_version":null,"live_until":"expires_at"}),
        json!([{"kind":"head","key":"lease/pairing","expected_version":null,"live_until":"expires_at"}]),
    ] {
        let message = serde_json::from_value::<NoteFences>(fence.clone())
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("fence live_until requires a positive expected_version"),
            "{message}"
        );
        let error = crate::atomic_prepare::prepare_update(
            &runtime,
            &token,
            &json!({"id": target.id, "content": "{}", "fence": fence}),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, RuntimeError::InvalidInput(_)),
            "{error:?}, and no plan may exist to be run"
        );
    }
    let empty = serde_json::from_value::<NoteFences>(
        json!({"kind":"head","key":"lease/pairing","expected_version":1,"live_until":""}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        empty.contains("fence live_until requires a document path"),
        "{empty}"
    );
    assert_eq!(version_of(&runtime, &token, &target).await, 1);
}

/// Arm 6. A version that does not match refuses as a version conflict even when
/// the deadline has also passed: the caller learns the older failure, and the
/// deadline read never happens.
#[tokio::test]
async fn fence_live_until_arm6_a_version_conflict_refuses_before_the_deadline() {
    let (runtime, token, _) = fixture();
    let registry = VerbRegistryBuilder::new().build().unwrap();
    head(
        &runtime,
        &token,
        "lease/stale",
        json!({"expires_at": expired()}),
    )
    .await;
    let target = head(&runtime, &token, "target/stale", json!({})).await;

    let observed = batch_refusal(
        observed_batch(
            &runtime,
            &token,
            &registry,
            "arm6",
            vec![observation("lease/stale", Some(2), Some("expires_at"))],
        )
        .await,
    );
    assert_eq!(observed["reason"], "version_conflict");

    let fenced = refusal(
        fenced_update(
            &runtime,
            &token,
            &target,
            json!([{"kind":"head","key":"lease/stale","expected_version":2,"live_until":"expires_at"}]),
        )
        .await,
    );
    assert_eq!(fenced["reason"], "fence_conflict");
    assert_eq!(fenced["current_version"], "1");
    assert!(fenced.get("value").is_none(), "{fenced}");
    assert_eq!(version_of(&runtime, &token, &target).await, 1);
}

/// The writer seam for arm 7. Real storage cannot show how many times the clock
/// was read, so the guard is driven directly against a writer that records
/// every labelled statement.
struct LabelTrace {
    labels: Vec<String>,
    deadline: String,
    version: i64,
}

#[async_trait]
impl SqlReader for LabelTrace {
    async fn query_row(&mut self, _: SqlStatement) -> StorageResult<Option<SqlRow>> {
        unreachable!("the fence guard reads scalars")
    }
    async fn query_all(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        unreachable!("the fence guard reads scalars")
    }
    async fn query_scalar(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlValue>> {
        let label = statement.label.clone().unwrap_or_default();
        self.labels.push(label.clone());
        Ok(Some(match label.as_str() {
            "note-write-guard" => SqlValue::Integer(self.version),
            "note-write-guard-clock" => SqlValue::Integer(chrono::Utc::now().timestamp_micros()),
            "note-write-guard-live-until" => {
                SqlValue::Text(json!({"expires_at": self.deadline}).to_string())
            }
            other => unreachable!("unexpected labelled read: {other}"),
        }))
    }
    async fn explain(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        unreachable!("the fence guard does not explain")
    }
}

#[async_trait]
impl SqlWriter for LabelTrace {
    async fn execute(&mut self, _: SqlStatement) -> StorageResult<u64> {
        unreachable!("the fence guard does not write")
    }
    async fn execute_batch(&mut self, _: Vec<SqlStatement>) -> StorageResult<u64> {
        unreachable!("the fence guard does not write")
    }
    async fn execute_script(&mut self, _: String) -> StorageResult<()> {
        unreachable!("the fence guard does not write")
    }
}

fn entry(key: &str, live_until: Option<&str>) -> NoteFence {
    NoteFence {
        key: key.into(),
        kind: "head".into(),
        expected_version: Some(1),
        live_until: live_until.map(Into::into),
    }
}

/// Arm 7. Two entries carrying deadlines in one write read the clock once, so
/// they are judged against one instant. The second row is the control: without
/// a deadline the clock is not read at all, which is what makes the count of
/// one a measurement rather than a constant.
#[tokio::test]
async fn fence_live_until_arm7_two_deadline_entries_read_one_clock() {
    for (fences, clocks, deadlines) in [
        (
            vec![
                entry("lease/a", Some("expires_at")),
                entry("lease/b", Some("expires_at")),
            ],
            1,
            2,
        ),
        (vec![entry("lease/a", None), entry("lease/b", None)], 0, 0),
        (
            vec![entry("lease/a", None), entry("lease/b", Some("expires_at"))],
            1,
            1,
        ),
    ] {
        let guard = NoteWriteGuard {
            namespace: "local".into(),
            target_id: uuid::Uuid::new_v4(),
            expected_version: Some(1),
            fence: Some(NoteFences::Many(fences)),
            create_key: None,
        };
        let mut trace = LabelTrace {
            labels: vec![],
            deadline: live(),
            version: 1,
        };
        assert!(
            guard.check_fence(&mut trace).await.unwrap().is_none(),
            "live deadlines and matching versions admit"
        );
        let count = |label: &str| trace.labels.iter().filter(|seen| *seen == label).count();
        assert_eq!(
            count("note-write-guard-clock"),
            clocks,
            "{:?}",
            trace.labels
        );
        assert_eq!(
            count("note-write-guard-live-until"),
            deadlines,
            "{:?}",
            trace.labels
        );
        if clocks == 1 {
            let first = trace
                .labels
                .iter()
                .position(|label| label == "note-write-guard-clock")
                .unwrap();
            let last = trace
                .labels
                .iter()
                .rposition(|label| label == "note-write-guard-live-until")
                .unwrap();
            assert!(first < last, "the clock precedes every deadline it serves");
        }
    }
}

async fn fenced_create(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    key: &str,
    fence: NoteFences,
) -> RuntimeResult<Note> {
    runtime
        .create_note_with_options(
            token,
            "head",
            None,
            "{}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some(key.into()),
                embed: Some(false),
                fence: Some(fence),
                ..Default::default()
            },
        )
        .await
        .map(|(note, _)| note)
}

/// Arm 8. The fenced write may be a creation, not only an update, and the
/// fence may be a single object rather than a list. Both are part of the
/// surface this amendment changes and neither is covered by the arms above:
/// every one of those fences an update through a list.
#[tokio::test]
async fn fence_live_until_arm8_a_creation_is_fenced_too_and_the_object_form_has_no_index() {
    let (runtime, token, _) = fixture();
    let expires_at = expired();
    head(
        &runtime,
        &token,
        "lease/create",
        json!({"expires_at": expires_at}),
    )
    .await;

    let object = NoteFences::One(NoteFence {
        key: "lease/create".into(),
        kind: "head".into(),
        expected_version: Some(1),
        live_until: Some("expires_at".into()),
    });
    let error = fenced_create(&runtime, &token, "created/refused", object.clone())
        .await
        .unwrap_err();
    let refused = details(error);
    assert_eq!(refused["reason"], "expired");
    assert_eq!(refused["key"], "lease/create");
    assert_eq!(refused["field"], "expires_at");
    assert_eq!(refused["value"], json!(format!("\"{expires_at}\"")));
    assert!(
        refused.get("index").is_none(),
        "the object form has no entry to index: {refused}"
    );

    // The list form of the same refusal differs by that one field and nothing
    // else, which is what makes the object form's absence an omission rather
    // than a different shape.
    let listed = details(
        fenced_create(
            &runtime,
            &token,
            "created/refused",
            NoteFences::Many(object.entries().to_vec()),
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(listed["index"], "0");
    let mut without_index = listed.clone();
    without_index.as_object_mut().unwrap().remove("index");
    without_index["now"] = refused["now"].clone();
    assert_eq!(without_index, refused);

    // Nothing was created on either refusal, and a live deadline creates.
    assert!(runtime
        .notes(&token)
        .unwrap()
        .get_live_notes_by_key(token.namespace().as_str(), "created/refused", Some("head"))
        .await
        .unwrap()
        .is_empty());
    head(
        &runtime,
        &token,
        "lease/create-live",
        json!({"expires_at": live()}),
    )
    .await;
    let created = fenced_create(
        &runtime,
        &token,
        "created/admitted",
        NoteFences::One(NoteFence {
            key: "lease/create-live".into(),
            kind: "head".into(),
            expected_version: Some(1),
            live_until: Some("expires_at".into()),
        }),
    )
    .await
    .unwrap();
    assert_eq!(created.version, 1);
}
