//! ADR-174 Amendment 9 acceptance: the single-head fence pins identity, and
//! both routes resolve it through one predicate.
//!
//! Arm 1 pairs the routes on one fixture and compares the refusals against each
//! other, for the reason the deadline amendment did: two predicates that agree
//! today pass a pair of separate assertions and fail this one.

use super::*;

use crate::note_write::{NoteFences, NoteWriteOptions};
use crate::pack::VerbRegistryBuilder;
use crate::{StreamAppendSpec, StreamBatchMember, StreamBatchRefusal, StreamObservation};
use serde_json::Value;
use uuid::Uuid;

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

/// Hard-delete the live holder and create a new note under the same key, which
/// is the state a fence cannot see by version alone: the replacement starts at
/// version 1, exactly where the caller's observation was.
async fn replace(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    key: &str,
    content: Value,
) -> (Uuid, Uuid) {
    let original = runtime
        .notes(token)
        .unwrap()
        .get_live_notes_by_key(token.namespace().as_str(), key, Some("head"))
        .await
        .unwrap()
        .remove(0);
    runtime.delete_note(token, original.id, true).await.unwrap();
    let replacement = head(runtime, token, key, content).await;
    assert_eq!(
        replacement.version, original.version,
        "the replacement must sit where the observation did, or this fixture proves nothing"
    );
    assert_ne!(replacement.id, original.id);
    (original.id, replacement.id)
}

fn append(stream: &str) -> StreamBatchMember {
    StreamBatchMember::Append(StreamAppendSpec {
        stream: stream.into(),
        record: json!({"event": "fence-identity"}),
        expected_seq: None,
        embed: Some(false),
        embedding_model: None,
        note_kind: "observation".into(),
        tags: None,
        fence: None,
    })
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

/// Arm 1. One fixture, both routes, and the refusals compared against each
/// other rather than each against its own expectation.
#[tokio::test]
async fn fence_identity_arm1_both_routes_refuse_one_replaced_key_with_the_same_evidence() {
    let (runtime, token, _) = fixture();
    let registry = VerbRegistryBuilder::new().build().unwrap();
    head(&runtime, &token, "lease/replaced", json!({})).await;
    let target = head(&runtime, &token, "target/replaced", json!({})).await;
    let (observed_id, current_id) = replace(
        &runtime,
        &token,
        "lease/replaced",
        json!({"owner": "other"}),
    )
    .await;

    let batch = batch_refusal(
        observed_batch(
            &runtime,
            &token,
            &registry,
            "arm1",
            vec![StreamObservation {
                key: "lease/replaced".into(),
                kind: "head".into(),
                version: Some(1),
                id: Some(observed_id),
                live_until: None,
            }],
        )
        .await,
    );
    let fenced = refusal(
        fenced_update(
            &runtime,
            &token,
            &target,
            json!([{"kind":"head","key":"lease/replaced","expected_version":1,"id":observed_id}]),
        )
        .await,
    );

    assert_eq!(field_names(&batch), field_names(&fenced));
    for field in [
        "reason",
        "key",
        "kind",
        "version",
        "id",
        "current_id",
        "index",
    ] {
        assert_eq!(
            batch[field], fenced[field],
            "{field} differs between the two routes: {batch} vs {fenced}"
        );
    }
    assert_eq!(batch["reason"], "identity_conflict");
    assert_eq!(batch["id"], observed_id.to_string());
    assert_eq!(batch["current_id"], current_id.to_string());
    assert_eq!(batch["version"], "1");
    assert_eq!(version_of(&runtime, &token, &target).await, 1);
}

/// Arm 2. Identity is read before the deadline, so a caller whose note was
/// replaced is not told about a deadline belonging to a document it never
/// observed.
#[tokio::test]
async fn fence_identity_arm2_identity_refuses_before_the_deadline() {
    let (runtime, token, _) = fixture();
    let expired = (chrono::Utc::now() - chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    head(
        &runtime,
        &token,
        "lease/both",
        json!({"expires_at": expired}),
    )
    .await;
    let target = head(&runtime, &token, "target/both", json!({})).await;
    let (observed_id, _) = replace(
        &runtime,
        &token,
        "lease/both",
        json!({"expires_at": expired}),
    )
    .await;

    let fenced = refusal(
        fenced_update(
            &runtime,
            &token,
            &target,
            json!([{"kind":"head","key":"lease/both","expected_version":1,
                    "id":observed_id,"live_until":"expires_at"}]),
        )
        .await,
    );
    assert_eq!(fenced["reason"], "identity_conflict");
    assert!(fenced.get("value").is_none(), "{fenced}");
    assert!(fenced.get("now").is_none(), "{fenced}");
}

/// Arm 3. A note that was replaced and also sits at a different version reports
/// the identity refusal on both routes, which is the arm that would catch them
/// disagreeing about which failure a caller is shown.
#[tokio::test]
async fn fence_identity_arm3_identity_refuses_before_the_version_on_both_routes() {
    let (runtime, token, _) = fixture();
    let registry = VerbRegistryBuilder::new().build().unwrap();
    head(&runtime, &token, "lease/moved", json!({})).await;
    let target = head(&runtime, &token, "target/moved", json!({})).await;
    let (observed_id, _) = replace(&runtime, &token, "lease/moved", json!({})).await;
    // Move the replacement past the observed version, so the version half would
    // also refuse if it were consulted first.
    let replacement = runtime
        .notes(&token)
        .unwrap()
        .get_live_notes_by_key(token.namespace().as_str(), "lease/moved", Some("head"))
        .await
        .unwrap()
        .remove(0);
    runtime
        .update_note(&token, replacement.id, patch("{\"bumped\":true}", 1, None))
        .await
        .unwrap();

    let batch = batch_refusal(
        observed_batch(
            &runtime,
            &token,
            &registry,
            "arm3",
            vec![StreamObservation {
                key: "lease/moved".into(),
                kind: "head".into(),
                version: Some(1),
                id: Some(observed_id),
                live_until: None,
            }],
        )
        .await,
    );
    let fenced = refusal(
        fenced_update(
            &runtime,
            &token,
            &target,
            json!([{"kind":"head","key":"lease/moved","expected_version":1,"id":observed_id}]),
        )
        .await,
    );
    assert_eq!(batch["reason"], "identity_conflict");
    assert_eq!(fenced["reason"], "identity_conflict");
    assert_eq!(field_names(&batch), field_names(&fenced));
}

/// Arm 4. A matching identity admits, so the check is not refusing everything.
#[tokio::test]
async fn fence_identity_arm4_a_matching_identity_admits() {
    let (runtime, token, _) = fixture();
    let lease = head(&runtime, &token, "lease/held", json!({})).await;
    let target = head(&runtime, &token, "target/held", json!({})).await;
    let outcome = fenced_update(
        &runtime,
        &token,
        &target,
        json!([{"kind":"head","key":"lease/held","expected_version":1,"id":lease.id}]),
    )
    .await;
    assert!(
        matches!(outcome, AtomicRunOutcome::Committed { .. }),
        "a matching identity must commit: {outcome:?}"
    );
    assert_eq!(version_of(&runtime, &token, &target).await, 2);
}

/// Arm 5. An identity has nothing to compare against an absence assertion, so
/// the pairing is refused while the call is still being shaped, in both fence
/// forms.
#[tokio::test]
async fn fence_identity_arm5_an_absence_assertion_cannot_carry_an_identity() {
    let (runtime, token, _) = fixture();
    let target = head(&runtime, &token, "target/pairing", json!({})).await;
    let id = Uuid::new_v4();
    for fence in [
        json!({"kind":"head","key":"lease/pairing","expected_version":null,"id":id}),
        json!([{"kind":"head","key":"lease/pairing","expected_version":null,"id":id}]),
    ] {
        let message = serde_json::from_value::<NoteFences>(fence.clone())
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("fence id requires a positive expected_version"),
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
    assert_eq!(version_of(&runtime, &token, &target).await, 1);
}

/// Arm 6. An absent holder has no identity to name, so it is a version refusal
/// on both routes, not an identity one.
#[tokio::test]
async fn fence_identity_arm6_an_absent_holder_is_a_version_refusal() {
    let (runtime, token, _) = fixture();
    let registry = VerbRegistryBuilder::new().build().unwrap();
    let lease = head(&runtime, &token, "lease/gone", json!({})).await;
    let target = head(&runtime, &token, "target/gone", json!({})).await;
    runtime.delete_note(&token, lease.id, true).await.unwrap();

    let batch = batch_refusal(
        observed_batch(
            &runtime,
            &token,
            &registry,
            "arm6",
            vec![StreamObservation {
                key: "lease/gone".into(),
                kind: "head".into(),
                version: Some(1),
                id: Some(lease.id),
                live_until: None,
            }],
        )
        .await,
    );
    assert_eq!(batch["reason"], "version_conflict");
    let fenced = refusal(
        fenced_update(
            &runtime,
            &token,
            &target,
            json!([{"kind":"head","key":"lease/gone","expected_version":1,"id":lease.id}]),
        )
        .await,
    );
    assert_eq!(fenced["reason"], "fence_conflict");
    assert!(fenced.get("current_id").is_none(), "{fenced}");
}

/// Arm 7. The fenced write may be a creation, and the object fence form's
/// refusal differs from the list form's by the entry index alone.
#[tokio::test]
async fn fence_identity_arm7_a_creation_is_fenced_too_and_the_object_form_has_no_index() {
    let (runtime, token, _) = fixture();
    head(&runtime, &token, "lease/create", json!({})).await;
    let (observed_id, current_id) =
        replace(&runtime, &token, "lease/create", json!({"owner": "other"})).await;

    let create = |fence: Value| {
        let runtime = &runtime;
        let token = &token;
        async move {
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
                        key: Some("created/refused".into()),
                        embed: Some(false),
                        fence: Some(serde_json::from_value(fence).unwrap()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap_err()
        }
    };

    let object = details(
        create(json!({"kind":"head","key":"lease/create","expected_version":1,"id":observed_id}))
            .await,
    );
    assert_eq!(object["reason"], "identity_conflict");
    assert_eq!(object["current_id"], current_id.to_string());
    assert!(object.get("index").is_none(), "{object}");

    let listed = details(
        create(json!([{"kind":"head","key":"lease/create","expected_version":1,"id":observed_id}]))
            .await,
    );
    assert_eq!(listed["index"], "0");
    let mut without_index = listed.clone();
    without_index.as_object_mut().unwrap().remove("index");
    assert_eq!(without_index, object);

    assert!(runtime
        .notes(&token)
        .unwrap()
        .get_live_notes_by_key(token.namespace().as_str(), "created/refused", Some("head"))
        .await
        .unwrap()
        .is_empty());
}
