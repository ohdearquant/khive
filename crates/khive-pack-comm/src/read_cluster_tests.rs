//! Real-backend phase contracts for the shared comm read paths.
//!
//! Hooks observe actual completed store operations. They do not substitute
//! rows, report SQL statement counts, or exist in production builds.

use super::*;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use khive_runtime::{Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::types::{DeleteMode, SqlStatement};

#[derive(Clone)]
pub(super) enum Phase {
    ThreadPage(Vec<Uuid>),
    ThreadRetained { stage: &'static str, rows: usize },
    ThreadOwners(Vec<Uuid>),
    BatchRead(Vec<Uuid>),
    ValidatedWindow(Vec<Uuid>),
    ValidatedAll,
    AtomicCommitted,
}

#[path = "read_cluster_tests/thread_baseline.rs"]
mod thread_baseline;

type HookFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type Hook = Arc<dyn Fn(Phase) -> HookFuture + Send + Sync>;

tokio::task_local! {
    static PHASE_HOOK: Hook;
    static MESSAGE_RENDER_COUNT: Arc<AtomicUsize>;
}

/// Count the real shared renderer, only inside a caller-established test scope.
pub(crate) fn observe_message_render() {
    let _ = MESSAGE_RENDER_COUNT.try_with(|count| count.fetch_add(1, Ordering::Relaxed));
}

pub(super) async fn observe_phase(phase: Phase) {
    if let Ok(hook) = PHASE_HOOK.try_with(Arc::clone) {
        hook(phase).await;
    }
}

fn hook(callback: impl Fn(Phase) -> HookFuture + Send + Sync + 'static) -> Hook {
    Arc::new(callback)
}

fn fixture() -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("lambda:reader".into()),
        brain_profile: None,
        packs: vec!["kg".into(), "comm".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private in-memory SQLite runtime");
    let token = runtime.authorize(Namespace::local()).expect("reader token");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::CommPack::new(runtime.clone()));
    builder.with_actor_id(Some("lambda:reader".into()));
    let registry = builder.build().expect("comm registry");
    runtime.notes(&token).expect("warm the routed notes gate");
    (runtime, token, registry)
}

fn id(index: u32) -> Uuid {
    Uuid::from_u128((u128::from(index) << 96) | 0xabcd_4e00_8fab_cdef_0000_0001)
}

fn message(index: u32) -> Note {
    let mut note =
        Note::new("local", "message", format!("message {index}")).with_properties(json!({
            "direction": "inbound",
            "from_actor": "lambda:sender",
            "to_actor": "lambda:reader",
            "subject": format!("subject {index}"),
            "read": false,
        }));
    note.id = id(index);
    note.created_at = 1_000_000;
    note.updated_at = note.created_at;
    note
}

async fn seed(runtime: &KhiveRuntime, notes: Vec<Note>) {
    // Direct backend fixture insertion represents already-stored/imported rows;
    // public reads still cross the shipping runtime/pack authorization chain.
    let store = runtime.backend().notes().expect("actual SQLite notes");
    for note in notes {
        store
            .upsert_note(note)
            .await
            .expect("seed real stored note");
    }
}

async fn change_properties(runtime: &KhiveRuntime, note_id: Uuid, change: fn(&mut Value)) {
    let store = runtime.backend().notes().expect("actual notes");
    let mut note = store.get_note(note_id).await.unwrap().unwrap();
    change(note.properties.as_mut().expect("fixture properties"));
    store
        .upsert_note(note)
        .await
        .expect("real concurrent property write");
}

async fn corrupt_content(runtime: &KhiveRuntime, note_id: Uuid) {
    // Keep valid JSON and current indexes intact. A BLOB in the full Note
    // content column is a genuine decoder error, not a mocked batch refusal.
    runtime
        .sql()
        .writer()
        .await
        .expect("fixture writer")
        .execute(SqlStatement {
            sql: "UPDATE notes SET content = ?1 WHERE id = ?2".into(),
            params: vec![
                SqlValue::Blob(vec![0xff]),
                SqlValue::Text(note_id.to_string()),
            ],
            label: Some("comm-read-fixture-corrupt-content".into()),
        })
        .await
        .expect("seed historical wrong full-row column type");
    assert!(runtime
        .backend()
        .notes()
        .unwrap()
        .get_note(note_id)
        .await
        .is_err());
}

fn reader_acquisitions(runtime: &KhiveRuntime) -> u64 {
    runtime
        .backend()
        .pool()
        .reader_acquisition_snapshot()
        .acquisitions
}

fn full_ids(value: &Value) -> Vec<String> {
    value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["full_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn bulk_live_windows_keep_order_and_bound_real_reader_acquisitions() {
    for count in [1_u32, 127, 128, 129, 500] {
        let (runtime, _, registry) = fixture();
        seed(&runtime, (1..=count).map(message).collect()).await;
        let mut raw: Vec<String> = (1..=count)
            .rev()
            .map(|index| id(index).to_string())
            .collect();
        raw[0] = id(count).braced().to_string().to_uppercase();
        if count < 500 {
            raw.push(id(count).simple().to_string());
        }
        let requested = raw.len();
        let before = reader_acquisitions(&runtime);
        let response = registry
            .dispatch("comm.mark_read", json!({"ids": raw, "atomic": true}))
            .await
            .expect("real atomic bulk mark");
        let acquired = reader_acquisitions(&runtime) - before;
        assert_eq!(response["requested_count"], requested);
        assert_eq!(response["unique_count"], count);
        assert_eq!(response["marked_count"], count);
        assert_eq!(
            full_ids(&response),
            (1..=count)
                .rev()
                .map(|index| id(index).to_string())
                .collect::<Vec<_>>()
        );
        // Isolated, fully warmed backend; no embeddings or profile/background
        // reads. The unchanged atomic policy guards read each unique note twice
        // including tombstones, before the writer's guarded patch. Only the
        // validation and committed-readback hydration reads follow 128 windows.
        // This measures actual reader acquisitions, not helper calls, SQL/VDBE
        // statements, elapsed time or retained bytes; total guard work stays O(N).
        let unique = count as usize;
        let policy_reads = 2 * unique;
        let validation_windows = requested.div_ceil(128);
        let readback_windows = unique.div_ceil(128);
        let expected_reads = policy_reads + validation_windows + readback_windows;
        assert!(
            acquired <= expected_reads as u64 + 6,
            "{count} unique rows ({requested} occurrences) acquired {acquired} readers; expected {policy_reads} policy reads plus {validation_windows} validation and {readback_windows} readback windows with bounded route overhead"
        );
        assert!(
            acquired >= 2,
            "positive control: real validation and readback both acquired readers"
        );
    }
}

#[tokio::test]
async fn original_bulk_validation_refusals_precede_every_write() {
    let (runtime, _, registry) = fixture();
    let good = message(1);
    let mut wrong_kind = message(2);
    wrong_kind.kind = "observation".into();
    seed(&runtime, vec![good, wrong_kind, message(3)]).await;
    corrupt_content(&runtime, id(3)).await;
    for (first, expected) in [
        (
            id(2),
            RuntimeError::InvalidInput(format!(
                "read: note {} is kind \"observation\", expected \"message\"",
                id(2)
            )),
        ),
        (
            id(9),
            RuntimeError::NotFound(format!("read: message {} not found", id(9))),
        ),
    ] {
        let error = registry
            .dispatch(
                "comm.read",
                json!({"ids": [id(1).to_string(), first.to_string(), id(3).to_string()]}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), expected.to_string());
        assert_eq!(
            std::mem::discriminant(&error),
            std::mem::discriminant(&expected)
        );
        assert_eq!(
            runtime
                .backend()
                .notes()
                .unwrap()
                .get_note(id(1))
                .await
                .unwrap()
                .unwrap()
                .properties
                .unwrap()["read"],
            false
        );
    }
    for raw in [vec![], vec![id(1).to_string(); 501]] {
        let before = reader_acquisitions(&runtime);
        let writes_before = runtime
            .backend()
            .pool()
            .writer_acquisition_snapshot()
            .acquisitions;
        let error = registry
            .dispatch("comm.mark_read", json!({"ids": raw}))
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert_eq!(
            reader_acquisitions(&runtime),
            before,
            "raw count validation precedes any read"
        );
        assert_eq!(
            runtime
                .backend()
                .pool()
                .writer_acquisition_snapshot()
                .acquisitions,
            writes_before,
            "empty and over-cap input never acquires a writer"
        );
    }
}

#[tokio::test]
async fn bulk_policy_checks_keep_namespace_direction_and_recipient_order() {
    let (runtime, _, registry) = fixture();
    let mut foreign = message(1);
    foreign.namespace = "lambda:foreign".into();
    let mut outbound_foreign = message(2);
    outbound_foreign.properties.as_mut().unwrap()["direction"] = json!("outbound");
    outbound_foreign.properties.as_mut().unwrap()["to_actor"] = json!("lambda:other");
    let mut outbound_own = message(3);
    outbound_own.properties.as_mut().unwrap()["direction"] = json!("outbound");
    let mut wrong_recipient = message(4);
    wrong_recipient.properties.as_mut().unwrap()["to_actor"] = json!("lambda:other");
    seed(
        &runtime,
        vec![foreign, outbound_foreign, outbound_own, wrong_recipient],
    )
    .await;
    for (index, expected) in [
        (1, RuntimeError::NotFound(format!("read: message {} not found", id(1)))),
        (2, RuntimeError::InvalidInput("read: that message is not addressed to caller actor \"lambda:reader\"".into())),
        (3, RuntimeError::InvalidInput(format!("read: message {} is outbound; only received (inbound) messages can be marked as read", id(3)))),
        (4, RuntimeError::InvalidInput("read: that message is not addressed to caller actor \"lambda:reader\"".into())),
    ] {
        let error = registry.dispatch("comm.read", json!({"ids":[id(index).to_string()]})).await.unwrap_err();
        assert_eq!(error.to_string().as_bytes(), expected.to_string().as_bytes());
        assert_eq!(std::mem::discriminant(&error), std::mem::discriminant(&expected));
    }
}

#[tokio::test]
async fn duplicate_in_later_window_is_revalidated_before_deduplication() {
    let (runtime, _, registry) = fixture();
    seed(&runtime, (1..=128).map(message).collect()).await;
    let raw: Vec<String> = (1..=128)
        .chain([1])
        .map(|index| id(index).to_string())
        .collect();
    let altered = Arc::new(AtomicBool::new(false));
    let callback = hook({
        let runtime = runtime.clone();
        let altered = Arc::clone(&altered);
        move |phase| {
            let runtime = runtime.clone();
            let altered = Arc::clone(&altered);
            Box::pin(async move {
                if let Phase::ValidatedWindow(ids) = phase {
                    if !altered.swap(true, Ordering::SeqCst) {
                        assert_eq!(ids.len(), 128);
                        assert!(runtime
                            .backend()
                            .notes()
                            .unwrap()
                            .delete_note(id(1), DeleteMode::Soft)
                            .await
                            .unwrap());
                    }
                }
            })
        }
    });
    let error = PHASE_HOOK
        .scope(
            callback,
            registry.dispatch("comm.mark_read", json!({"ids":raw,"atomic":true})),
        )
        .await
        .unwrap_err();
    assert!(
        altered.load(Ordering::SeqCst),
        "actual first window reached and write completed"
    );
    assert!(
        matches!(error, RuntimeError::NotFound(ref text) if text == &format!("read: message {} not found",id(1)))
    );
    assert_eq!(
        runtime
            .backend()
            .notes()
            .unwrap()
            .get_note(id(2))
            .await
            .unwrap()
            .unwrap()
            .properties
            .unwrap()["read"],
        false
    );
}

#[tokio::test]
async fn duplicates_within_one_healthy_window_share_one_observation() {
    let (runtime, _, _) = fixture();
    seed(&runtime, vec![message(1)]).await;
    let observed = Arc::new(AtomicBool::new(false));
    let callback = hook({
        let runtime = runtime.clone();
        let observed = Arc::clone(&observed);
        move |phase| {
            let runtime = runtime.clone();
            let observed = Arc::clone(&observed);
            Box::pin(async move {
                if let Phase::BatchRead(ids) = phase {
                    assert_eq!(ids, vec![id(1), id(1)]);
                    observed.store(true, Ordering::SeqCst);
                    change_properties(&runtime, id(1), |p| p["to_actor"] = json!("lambda:other"))
                        .await;
                }
            })
        }
    });
    let token = runtime.authorize(Namespace::local()).unwrap();
    let (requested, targets) = PHASE_HOOK
        .scope(
            callback,
            validate_bulk_read_targets(
                &runtime,
                &token,
                vec![id(1).to_string(), id(1).simple().to_string()],
                "read",
            ),
        )
        .await
        .expect("shared healthy observation");
    assert!(observed.load(Ordering::SeqCst));
    assert_eq!(requested, 2);
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].1.properties.as_ref().unwrap()["to_actor"],
        "lambda:reader"
    );
    assert_eq!(
        runtime
            .backend()
            .notes()
            .unwrap()
            .get_note(id(1))
            .await
            .unwrap()
            .unwrap()
            .properties
            .unwrap()["to_actor"],
        "lambda:other"
    );
}

#[tokio::test]
async fn mixed_prefixes_resolve_only_at_their_original_phase() {
    let (runtime, _, registry) = fixture();
    seed(&runtime, vec![message(1), message(2), message(3)]).await;
    corrupt_content(&runtime, id(3)).await;
    let prefix = id(2).simple().to_string()[..8].to_string();
    let altered = Arc::new(AtomicBool::new(false));
    let callback = hook({
        let runtime = runtime.clone();
        let altered = Arc::clone(&altered);
        move |phase| {
            let runtime = runtime.clone();
            let altered = Arc::clone(&altered);
            Box::pin(async move {
                if let Phase::ValidatedWindow(ids) = phase {
                    assert_eq!(ids, vec![id(1)]);
                    assert!(!altered.swap(true, Ordering::SeqCst));
                    assert!(runtime
                        .backend()
                        .notes()
                        .unwrap()
                        .delete_note(id(2), DeleteMode::Soft)
                        .await
                        .unwrap());
                }
            })
        }
    });
    let error = PHASE_HOOK
        .scope(
            callback,
            registry.dispatch(
                "comm.read",
                json!({"ids":[id(1).to_string(),prefix,id(3).to_string()]}),
            ),
        )
        .await
        .unwrap_err();
    assert!(altered.load(Ordering::SeqCst));
    assert!(
        matches!(error,RuntimeError::InvalidInput(ref text) if text==&format!("read: no record matches prefix: {prefix:?}"))
    );
    assert_eq!(
        runtime
            .backend()
            .notes()
            .unwrap()
            .get_note(id(1))
            .await
            .unwrap()
            .unwrap()
            .properties
            .unwrap()["read"],
        false
    );
}

#[tokio::test]
async fn atomic_readback_keeps_fresh_siblings_and_unreadable_snapshot_fallback() {
    let (runtime, _, registry) = fixture();
    seed(&runtime, (1..=3).map(message).collect()).await;
    let committed = Arc::new(AtomicBool::new(false));
    let callback = hook({
        let runtime = runtime.clone();
        let committed = Arc::clone(&committed);
        move |phase| {
            let runtime = runtime.clone();
            let committed = Arc::clone(&committed);
            Box::pin(async move {
                if matches!(phase, Phase::AtomicCommitted) {
                    assert!(!committed.swap(true, Ordering::SeqCst));
                    assert_eq!(
                        runtime
                            .backend()
                            .notes()
                            .unwrap()
                            .get_note(id(1))
                            .await
                            .unwrap()
                            .unwrap()
                            .properties
                            .unwrap()["read"],
                        true,
                        "premise: mutation really committed before fault"
                    );
                    corrupt_content(&runtime, id(1)).await;
                    assert!(runtime
                        .backend()
                        .notes()
                        .unwrap()
                        .delete_note(id(2), DeleteMode::Soft)
                        .await
                        .unwrap());
                    change_properties(&runtime, id(3), |p| {
                        p["concurrent"] = json!("fresh-after-commit")
                    })
                    .await;
                }
            })
        }
    });
    let response=PHASE_HOOK.scope(callback,registry.dispatch("comm.mark_read",json!({"ids":[id(1).to_string(),id(2).to_string(),id(3).to_string()],"atomic":true}))).await.expect("committed marks remain successes");
    assert!(committed.load(Ordering::SeqCst));
    assert_eq!(response["marked_count"], 3);
    for result in response["results"].as_array().unwrap() {
        assert_eq!(result["status"], "success");
        assert_eq!(result["read"], true);
        assert_eq!(result["properties"]["read"], true);
    }
    assert_eq!(
        response["results"][2]["properties"]["concurrent"],
        "fresh-after-commit"
    );
    assert!(response["results"][0]["properties"]
        .get("concurrent")
        .is_none());
    assert!(runtime
        .backend()
        .notes()
        .unwrap()
        .get_note(id(2))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn original_atomic_writer_recheck_rolls_back_and_keeps_concurrent_properties() {
    let (runtime, _, registry) = fixture();
    seed(&runtime, vec![message(1), message(2)]).await;
    let callback = hook({
        let runtime = runtime.clone();
        move |phase| {
            let runtime = runtime.clone();
            Box::pin(async move {
                if matches!(phase, Phase::ValidatedAll) {
                    change_properties(&runtime, id(1), |p| p["concurrent"] = json!("retained"))
                        .await;
                    change_properties(&runtime, id(2), |p| p["to_actor"] = json!("lambda:other"))
                        .await;
                }
            })
        }
    });
    PHASE_HOOK
        .scope(
            callback,
            registry.dispatch(
                "comm.mark_read",
                json!({"ids":[id(1).to_string(),id(2).to_string()],"atomic":true}),
            ),
        )
        .await
        .expect_err("actual current-state writer guard refuses");
    let first = runtime
        .backend()
        .notes()
        .unwrap()
        .get_note(id(1))
        .await
        .unwrap()
        .unwrap()
        .properties
        .unwrap();
    assert_eq!(first["read"], false, "first patch must roll back");
    assert_eq!(first["concurrent"], "retained");
}

#[tokio::test]
async fn best_effort_body_and_current_state_guard_keep_existing_dispositions() {
    let (runtime, _, registry) = fixture();
    seed(&runtime, vec![message(1), message(2)]).await;
    let callback = hook({
        let runtime = runtime.clone();
        move |phase| {
            let runtime = runtime.clone();
            Box::pin(async move {
                if matches!(phase, Phase::ValidatedAll) {
                    change_properties(&runtime, id(1), |p| p["concurrent"] = json!("retained"))
                        .await;
                    change_properties(&runtime, id(2), |p| p["direction"] = json!("outbound"))
                        .await;
                }
            })
        }
    });
    let response = PHASE_HOOK
        .scope(
            callback,
            registry.dispatch(
                "comm.read",
                json!({"ids":[id(1).to_string(),id(2).to_string()],"body":true}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(response["status"], "partial");
    assert_eq!(response["results"][0]["content"], "message 1");
    assert_eq!(
        response["results"][0]["properties"]["concurrent"],
        "retained"
    );
    assert_eq!(response["results"][1]["status"], "failed");
    assert_eq!(response["results"][1]["read"], false);
    assert!(response["results"][1].get("content").is_none());
    let ack = registry
        .dispatch("comm.read", json!({"id":id(1).to_string(),"body":false}))
        .await
        .unwrap();
    assert!(ack.get("content").is_none());
    assert_eq!(ack["read"], true);
}

fn thread_messages(response: &Value) -> Vec<String> {
    response["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["full_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn thread_deleting_a_fetched_row_does_not_skip_the_next_physical_id() {
    let (runtime, _, registry) = fixture();
    let root = id(1);
    seed(
        &runtime,
        (1..=401)
            .map(|index| {
                let mut note = message(index);
                note.properties.as_mut().unwrap()["thread_id"] = json!(root.to_string());
                note
            })
            .collect(),
    )
    .await;
    let pages = Arc::new(AtomicUsize::new(0));
    let deleted = Arc::new(AtomicBool::new(false));
    let callback = hook({
        let runtime = runtime.clone();
        let pages = Arc::clone(&pages);
        let deleted = Arc::clone(&deleted);
        move |phase| {
            let runtime = runtime.clone();
            let pages = Arc::clone(&pages);
            let deleted = Arc::clone(&deleted);
            Box::pin(async move {
                if let Phase::ThreadPage(ids) = phase {
                    if pages.fetch_add(1, Ordering::SeqCst) == 0 {
                        assert_eq!(ids.len(), 200);
                        assert!(ids.contains(&id(2)));
                        assert!(!ids.contains(&id(201)));
                        assert!(runtime
                            .backend()
                            .notes()
                            .unwrap()
                            .delete_note(id(2), DeleteMode::Soft)
                            .await
                            .unwrap());
                        deleted.store(true, Ordering::SeqCst);
                    }
                }
            })
        }
    });
    let response = PHASE_HOOK
        .scope(
            callback,
            registry.dispatch("comm.thread", json!({"id":root.to_string(),"limit":500})),
        )
        .await
        .unwrap();
    assert!(deleted.load(Ordering::SeqCst));
    assert_eq!(pages.load(Ordering::SeqCst), 3);
    assert_eq!(
        response["count"], 401,
        "already fetched evidence retained; unread later physical rows not skipped"
    );
    assert!(thread_messages(&response).contains(&id(201).to_string()));
    assert!(
        runtime
            .backend()
            .notes()
            .unwrap()
            .get_note(id(2))
            .await
            .unwrap()
            .is_none(),
        "premise: real deletion completed"
    );
}

#[tokio::test]
async fn thread_late_twin_read_state_is_folded_before_logical_limit() {
    let (runtime, _, registry) = fixture();
    let root = id(1);
    let mut root_note = message(1);
    root_note.created_at = 2_000_000;
    root_note.properties.as_mut().unwrap()["thread_id"] = json!(root.to_string());
    root_note.properties.as_mut().unwrap()["direction"] = json!("outbound");
    root_note.properties.as_mut().unwrap()["from_actor"] = json!("lambda:reader");
    let mut notes = vec![root_note];
    for index in 2..=400 {
        let mut note = message(index);
        note.properties.as_mut().unwrap()["thread_id"] = json!(root.to_string());
        notes.push(note);
    }
    let mut twin = message(401);
    twin.created_at = 1;
    twin.properties.as_mut().unwrap()["thread_id"] = json!(root.to_string());
    twin.properties.as_mut().unwrap()["outbound_ref"] = json!(root.to_string());
    twin.properties.as_mut().unwrap()["read"] = json!(true);
    notes.push(twin);
    seed(&runtime, notes).await;
    let response = registry
        .dispatch(
            "comm.thread",
            json!({"id":root.to_string(),"limit":1,"order":"desc","fields":["full_id","read"]}),
        )
        .await
        .unwrap();
    assert_eq!(response["count"], 1);
    assert_eq!(response["messages"][0]["full_id"], root.to_string());
    assert_eq!(response["messages"][0]["read"], true);
    assert!(response["messages"][0].get("content").is_none());
}

#[tokio::test]
async fn thread_physical_ties_legacy_roots_and_mailbox_filter_keep_bounded_reader_walk() {
    let (runtime, _, registry) = fixture();
    let root = id(1);
    let mut notes = Vec::new();
    let spellings = [
        root.to_string(),
        root.simple().to_string(),
        root.braced().to_string().to_uppercase(),
        root.urn().to_string(),
    ];
    for index in 1..=401 {
        let mut note = message(index);
        if index != 1 {
            note.properties.as_mut().unwrap()["thread_id"] =
                json!(spellings[(index as usize) % spellings.len()]);
        }
        if index % 2 == 0 {
            note.properties.as_mut().unwrap()["to_actor"] = json!("lambda:hidden");
        }
        notes.push(note);
    }
    seed(&runtime, notes).await;
    let before = reader_acquisitions(&runtime);
    let response = registry
        .dispatch(
            "comm.thread",
            json!({"id":root.to_string(),"limit":500,"order":"asc"}),
        )
        .await
        .unwrap();
    let acquired = reader_acquisitions(&runtime) - before;
    assert_eq!(response["thread_id"], root.to_string());
    assert_eq!(
        thread_messages(&response),
        (1..=401)
            .filter(|index| index % 2 != 0)
            .map(|index| id(index).to_string())
            .collect::<Vec<_>>()
    );
    assert!(acquired<=6,"actual physical reader acquisitions must follow 200-row windows plus the validated root: {acquired}");
    assert!(
        acquired >= 4,
        "positive control: actual root and full physical page walk"
    );
    let descending = registry
        .dispatch(
            "comm.thread",
            json!({"id":root.to_string(),"limit":3,"order":"desc"}),
        )
        .await
        .unwrap();
    assert_eq!(
        thread_messages(&descending),
        vec![
            id(401).to_string(),
            id(399).to_string(),
            id(397).to_string()
        ]
    );
    let after = registry
        .dispatch(
            "comm.thread",
            json!({"id":root.to_string(),"limit":3,"after":id(397).to_string(),"order":"desc"}),
        )
        .await
        .unwrap();
    assert_eq!(
        thread_messages(&after),
        vec![
            id(395).to_string(),
            id(393).to_string(),
            id(391).to_string()
        ]
    );
}

#[tokio::test]
async fn malformed_first_row_keeps_the_typed_scalar_get_note_error() {
    let (runtime, token, registry) = fixture();
    let mut wrong_kind = message(2);
    wrong_kind.kind = "observation".into();
    seed(&runtime, vec![message(1), wrong_kind]).await;
    corrupt_content(&runtime, id(1)).await;
    let scalar_error = runtime
        .notes(&token)
        .unwrap()
        .get_note(id(1))
        .await
        .unwrap_err();
    let expected = RuntimeError::Storage(scalar_error);
    let error = registry
        .dispatch(
            "comm.mark_read",
            json!({"ids":[id(1).to_string(),id(2).to_string()], "atomic":true}),
        )
        .await
        .unwrap_err();
    let RuntimeError::Storage(khive_storage::StorageError::Driver {
        capability,
        operation,
        source,
    }) = error.refusal_source()
    else {
        panic!("expected the original storage driver failure, got {error:?}");
    };
    assert_eq!(*capability, khive_storage::StorageCapability::Notes);
    assert_eq!(operation.as_ref(), "get_note");
    let driver = source.downcast_ref::<rusqlite::Error>().unwrap();
    assert!(matches!(
        driver,
        rusqlite::Error::InvalidColumnType(5, column, rusqlite::types::Type::Blob)
            if column == "content"
    ));
    assert_eq!(driver.sqlite_error_code(), None);
    assert_eq!(
        error.to_string().as_bytes(),
        expected.to_string().as_bytes()
    );
    assert!(!runtime
        .notes(&token)
        .unwrap()
        .get_note(id(2))
        .await
        .unwrap()
        .unwrap()
        .properties
        .unwrap()["read"]
        .as_bool()
        .unwrap());
}

#[tokio::test]
async fn read_admission_failures_keep_typed_wire_errors_and_never_mark_messages() {
    use std::time::Duration;

    for (verb, params) in [
        ("comm.read", json!({"id": id(1).to_string(), "body": false})),
        (
            "comm.read",
            json!({"ids": [id(1).to_string(), id(2).to_string()], "body": false}),
        ),
        (
            "comm.mark_read",
            json!({"ids": [id(1).to_string(), id(2).to_string()], "atomic": true}),
        ),
    ] {
        let (runtime, token, registry) = fixture();
        seed(&runtime, vec![message(1), message(2)]).await;
        let store = runtime.notes(&token).unwrap();
        let before = [
            store.get_note(id(1)).await.unwrap().unwrap(),
            store.get_note(id(2)).await.unwrap().unwrap(),
        ];
        let pool = runtime.backend().pool();
        assert_eq!(pool.max_readers(), 0, "private memory backend");
        let checkout_timeout = pool.config().checkout_timeout;
        let timeout_ms = u64::try_from(checkout_timeout.as_millis()).unwrap();
        let held_reader = pool
            .reader()
            .expect("hold the only reader admission permit");
        let reads_before = pool.reader_acquisition_snapshot();
        assert_eq!(reads_before.reader_admission_capacity, 1);
        assert_eq!(reads_before.available_reader_admission_slots, 0);
        let writes_before = pool.writer_acquisition_snapshot().acquisitions;

        // Store admission waits asynchronously before any SQL worker starts.
        // The guard stays held until dispatch returns; no scheduler race releases it.
        tokio::time::pause();
        let error = tokio::time::timeout(
            checkout_timeout.saturating_mul(3) + Duration::from_secs(1),
            registry.dispatch(verb, params.clone()),
        )
        .await
        .expect("bounded scalar or batch-then-scalar admission wait")
        .expect_err("the real reader pool is exhausted");
        let RuntimeError::Storage(khive_storage::StorageError::AdmissionTimeout {
            operation,
            timeout_ms: actual_timeout,
            pool_identity,
        }) = error.refusal_source()
        else {
            panic!("{verb} erased the typed admission failure: {error:?}");
        };
        assert_eq!(operation.as_ref(), "get_note");
        assert_eq!(*actual_timeout, timeout_ms);
        assert_eq!(pool_identity.as_deref(), Some(":memory:"));
        let retry = error.retryable_failure_context().unwrap();
        assert_eq!(retry.stage, "storage_admission_timeout");
        assert_eq!(retry.timeout, checkout_timeout);
        assert_eq!(retry.operation.as_deref(), Some("get_note"));
        assert_eq!(retry.pool_identity.as_deref(), Some(":memory:"));

        let wire =
            khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::Unknown);
        assert_eq!(wire["kind"], "unavailable");
        assert_eq!(wire["code"], "storage_admission_timeout");
        assert_eq!(wire["stage"], "storage_admission_timeout");
        assert_eq!(wire["retryable"], true);
        assert_eq!(wire["operation"], "get_note");
        assert_eq!(wire["timeout_ms"], timeout_ms);
        assert_eq!(wire["pool_identity"], ":memory:");
        assert_eq!(wire["domain_disposition"], "unknown");
        for field in [
            "sqlite_primary_code",
            "sqlite_extended_code",
            "sqlite_write_stage",
        ] {
            assert!(wire.get(field).is_none(), "no SQLite work began: {wire}");
        }
        let reads_after = pool.reader_acquisition_snapshot();
        assert!(reads_after.checkout_timeouts > reads_before.checkout_timeouts);
        assert_eq!(reads_after.acquisitions, reads_before.acquisitions);
        assert_eq!(
            pool.writer_acquisition_snapshot().acquisitions,
            writes_before
        );
        drop(held_reader);
        tokio::time::resume();

        for note in &before {
            assert_eq!(store.get_note(note.id).await.unwrap().unwrap(), *note);
            assert_eq!(note.properties.as_ref().unwrap()["read"], false);
        }
        let recovered = registry.dispatch(verb, params).await.unwrap();
        assert_eq!(recovered["status"], "success");
        assert_eq!(
            store
                .get_note(id(1))
                .await
                .unwrap()
                .unwrap()
                .properties
                .unwrap()["read"],
            true
        );
        assert_eq!(
            store
                .get_note(id(2))
                .await
                .unwrap()
                .unwrap()
                .properties
                .unwrap()["read"],
            recovered.get("results").is_some(),
            "only the requested scalar target or both requested bulk targets are marked"
        );
    }
}

#[tokio::test]
async fn attachment_bearing_thread_and_inbox_pages_keep_bounded_reader_acquisitions() {
    use khive_storage::{Attachment, AttachmentSubstrate, ContentRef, NewAttachment};

    for count in [3_u32, 201] {
        let (runtime, _, registry) = fixture();
        let root = id(1);
        seed(
            &runtime,
            (1..=count)
                .map(|index| {
                    let mut note = message(index);
                    note.properties.as_mut().unwrap()["thread_id"] = json!(root.to_string());
                    note
                })
                .collect(),
        )
        .await;
        let attachments = runtime.attachments().expect("canonical attachment store");
        for index in 1..=count {
            for position in [1_u32, 0] {
                attachments
                    .upsert_attachment(Attachment::from_new(
                        id(index),
                        AttachmentSubstrate::Note,
                        NewAttachment {
                            role: format!("message-attachment:{position}"),
                            content_ref: ContentRef::from_hex(format!(
                                "{:064x}",
                                u64::from(index) * 2 + u64::from(position)
                            ))
                            .unwrap(),
                            media_type: Some(format!("application/fixture-{position}")),
                            size_bytes: Some(u64::from(index) + u64::from(position)),
                        },
                        1_000_000,
                    ))
                    .await
                    .expect("seed real attachment row");
            }
        }
        for verb in ["comm.thread", "comm.inbox"] {
            let args = if verb == "comm.thread" {
                json!({"id":root.to_string(),"limit":500,"order":"asc"})
            } else {
                json!({"limit":200})
            };
            let before = reader_acquisitions(&runtime);
            let response = registry.dispatch(verb, args).await.unwrap();
            let acquired = reader_acquisitions(&runtime) - before;
            let rows = response["messages"].as_array().unwrap();
            assert_eq!(
                rows.len(),
                if verb == "comm.thread" {
                    count as usize
                } else {
                    count.min(200) as usize
                }
            );
            for row in rows {
                let owner: Uuid = row["full_id"].as_str().unwrap().parse().unwrap();
                let index = (owner.as_u128() >> 96) as u64;
                assert_eq!(
                    row["attachments"],
                    json!([
                        {"content_ref":format!("{:064x}",index*2),"size":index,"media_type":"application/fixture-0"},
                        {"content_ref":format!("{:064x}",index*2+1),"size":index+1,"media_type":"application/fixture-1"},
                    ]),
                    "{verb} keeps owner identity and numeric attachment order"
                );
                assert!(row.get("attachments_error").is_none());
            }
            assert!(
                acquired <= 6,
                "{verb} must read attachment-bearing pages in bounded windows, count={count}, acquisitions={acquired}"
            );
            assert!(
                acquired >= 2,
                "real note and attachment readers were acquired"
            );
        }
        let mut ids: Vec<_> = (1..=count)
            .rev()
            .map(|index| id(index).to_string())
            .collect();
        ids.push(id(count).simple().to_string());
        let before = reader_acquisitions(&runtime);
        let acknowledgement = registry
            .dispatch("comm.read", json!({"ids":ids,"body":false}))
            .await
            .expect("actual acknowledgement baseline");
        let baseline_reads = reader_acquisitions(&runtime) - before;
        assert_eq!(acknowledgement["requested_count"], count + 1);
        assert_eq!(acknowledgement["unique_count"], count);
        assert_eq!(acknowledgement["marked_count"], count);
        for row in acknowledgement["results"].as_array().unwrap() {
            assert_eq!(row["status"], "success");
            assert_eq!(row["read"], true);
            assert!(row.get("attachments").is_none());
            assert!(row.get("content").is_none());
        }
        let before = reader_acquisitions(&runtime);
        let response = registry
            .dispatch("comm.read", json!({"ids":ids,"body":true}))
            .await
            .expect("actual body-bearing bulk read");
        let body_reads = reader_acquisitions(&runtime) - before;
        assert_eq!(response["requested_count"], count + 1);
        assert_eq!(response["unique_count"], count);
        assert_eq!(response["marked_count"], count);
        assert_eq!(
            full_ids(&response),
            (1..=count)
                .rev()
                .map(|index| id(index).to_string())
                .collect::<Vec<_>>()
        );
        for row in response["results"].as_array().unwrap() {
            let owner: Uuid = row["full_id"].as_str().unwrap().parse().unwrap();
            let index = (owner.as_u128() >> 96) as u64;
            assert_eq!(row["status"], "success");
            assert_eq!(row["read"], true);
            assert_eq!(row["content"], format!("message {index}"));
            assert_eq!(
                row["attachments"],
                json!([
                    {"content_ref":format!("{:064x}",index*2),"size":index,"media_type":"application/fixture-0"},
                    {"content_ref":format!("{:064x}",index*2+1),"size":index+1,"media_type":"application/fixture-1"},
                ])
            );
            assert!(row.get("attachments_error").is_none());
        }
        assert!(
            baseline_reads >= u64::from(count) * 2 + u64::from(count + 1).div_ceil(128),
            "baseline includes the existing per-target policy and successful mark readback readers"
        );
        assert_eq!(
            body_reads.checked_sub(baseline_reads),
            Some(1),
            "body-bearing bulk read must add one physical attachment reader, count={count}, baseline={baseline_reads}, body={body_reads}"
        );
    }
}

#[tokio::test]
async fn attachment_page_batch_keeps_readable_siblings_and_owner_diagnostics() {
    use khive_storage::{Attachment, AttachmentSubstrate, ContentRef, NewAttachment};

    let (runtime, _, registry) = fixture();
    let root = id(1);
    seed(
        &runtime,
        (1..=3)
            .map(|index| {
                let mut note = message(index);
                note.properties.as_mut().unwrap()["thread_id"] = json!(root.to_string());
                note
            })
            .collect(),
    )
    .await;
    let content_ref = ContentRef::from_hex("11".repeat(32)).unwrap();
    let store = runtime.attachments().unwrap();
    for (owner, position) in [(id(1), 0), (id(2), 0), (id(2), 1)] {
        store
            .upsert_attachment(Attachment::from_new(
                owner,
                AttachmentSubstrate::Note,
                NewAttachment {
                    role: format!("message-attachment:{position}"),
                    content_ref: content_ref.clone(),
                    media_type: None,
                    size_bytes: Some(7),
                },
                1_000_000,
            ))
            .await
            .unwrap();
    }
    let mut writer = runtime.sql().writer().await.unwrap();
    writer
        .execute(SqlStatement {
            sql: "PRAGMA ignore_check_constraints = ON".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    writer
        .execute(SqlStatement {
            sql: "UPDATE attachments SET content_ref = 'invalid-ref' WHERE record_uuid = ?1 AND role = 'message-attachment:1'".into(),
            params: vec![SqlValue::Text(id(2).to_string())],
            label: Some("comm-page-fixture-unreadable-attachment".into()),
        })
        .await
        .unwrap();
    writer
        .execute(SqlStatement {
            sql: "PRAGMA ignore_check_constraints = OFF".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    drop(writer);
    for role in ["", "\n"] {
        runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement {
                sql: "INSERT INTO attachment_quarantine (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at, reason) VALUES (?1, 'note', ?2, ?3, NULL, 7, 1000000, 'invalid_role')".into(),
                params: vec![
                    SqlValue::Text(id(3).to_string()),
                    SqlValue::Text(role.into()),
                    SqlValue::Text(content_ref.to_string()),
                ],
                label: Some("comm-page-fixture-quarantined-attachment".into()),
            })
            .await
            .unwrap();
    }
    for verb in ["comm.thread", "comm.inbox"] {
        let args = if verb == "comm.thread" {
            json!({"id":root.to_string(),"limit":500})
        } else {
            json!({"limit":200})
        };
        let before = reader_acquisitions(&runtime);
        let response = registry.dispatch(verb, args).await;
        assert!(
            response.is_ok(),
            "{verb} must retain readable message views: {response:?}"
        );
        let response = response.unwrap();
        let acquired = reader_acquisitions(&runtime) - before;
        let rows = response["messages"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        for index in 1..=3 {
            let row = rows
                .iter()
                .find(|row| row["full_id"] == id(index).to_string())
                .unwrap();
            let expected = if index == 3 {
                json!([])
            } else {
                json!([{"content_ref":content_ref,"size":7,"media_type":null}])
            };
            assert_eq!(row["attachments"], expected, "{verb} owner {index}");
            if index == 1 {
                assert!(row.get("attachments_error").is_none());
            } else {
                assert_eq!(
                    row["attachments_error"],
                    json!({"count":index-1,"reason":"unreadable_attachment"}),
                    "{verb} reports only this owner's unreadable and quarantined rows"
                );
            }
        }
        assert!(
            acquired <= 6,
            "{verb} diagnostics page acquisitions: {acquired}"
        );
    }
}

#[tokio::test]
async fn nonmarking_read_preserves_stored_note_and_unread_inbox() {
    let (runtime, token, registry) = fixture();
    seed(&runtime, vec![message(1), message(2)]).await;
    let store = runtime.notes(&token).unwrap();
    let before = store.get_note(id(1)).await.unwrap().unwrap();
    let inbox_before = registry
        .dispatch("comm.inbox", json!({"status":"unread"}))
        .await
        .unwrap();
    assert_eq!(inbox_before["messages"].as_array().unwrap().len(), 2);
    for raw in [id(1).to_string(), short_id(id(1))] {
        let result = registry
            .dispatch("comm.read", json!({"id":raw,"mark":false}))
            .await
            .unwrap();
        assert_eq!(
            store.get_note(id(1)).await.unwrap().unwrap(),
            before,
            "nonmarking read must preserve the complete stored note and unread inbox membership"
        );
        assert_eq!(result["id"], short_id(id(1)));
        assert_eq!(result["full_id"], id(1).to_string());
        assert_eq!(result["status"], "success");
        assert_eq!(result["read"], false);
        assert_eq!(result["properties"], before.properties.clone().unwrap());
        assert_eq!(result["subject"], "subject 1");
        assert_eq!(result["content"], "message 1");
        assert_eq!(result["from"], "lambda:sender");
        assert_eq!(result["to"], "lambda:reader");
        assert_eq!(result["direction"], "inbound");
        assert_eq!(result["attachments"], json!([]));
        assert!(result.get("results").is_none());
        let inbox_after = registry
            .dispatch("comm.inbox", json!({"status":"unread"}))
            .await
            .unwrap();
        assert_eq!(
            inbox_after["messages"], inbox_before["messages"],
            "nonmarking read must preserve unread inbox rows and order"
        );
        assert_eq!(inbox_after["unread_count"], 2);
    }
}

#[tokio::test]
async fn nonmarking_read_reports_actual_read_state_and_body_opt_out() {
    for (stored_read, expected_read) in [
        (Some(json!(true)), true),
        (Some(json!(false)), false),
        (None, false),
        (Some(Value::Null), false),
        (Some(json!("true")), false),
    ] {
        let (runtime, token, registry) = fixture();
        let mut note = message(1);
        let props = note.properties.as_mut().unwrap().as_object_mut().unwrap();
        if let Some(read) = stored_read {
            props.insert("read".into(), read);
        } else {
            props.remove("read");
        }
        props.insert("untouched".into(), json!({"nested": [1, null, "keep"]}));
        seed(&runtime, vec![note]).await;
        let store = runtime.notes(&token).unwrap();
        let before = store.get_note(id(1)).await.unwrap().unwrap();
        for body in [true, false] {
            let result = registry
                .dispatch(
                    "comm.read",
                    json!({"id":id(1).to_string(),"mark":false,"body":body}),
                )
                .await
                .unwrap();
            assert_eq!(
                result["read"], expected_read,
                "nonmarking fetch must report stored read semantics without rewriting properties"
            );
            assert_eq!(result["properties"], before.properties.clone().unwrap());
            assert_eq!(result["status"], "success");
            assert_eq!(result["id"], short_id(id(1)));
            assert_eq!(result["full_id"], id(1).to_string());
            assert!(result.get("mark_error").is_none());
            assert!(result.get("marked").is_none());
            if body {
                assert_eq!(result["content"], "message 1");
                assert_eq!(result["subject"], "subject 1");
            } else {
                assert_eq!(
                    result.as_object().unwrap().len(),
                    5,
                    "body=false nonmarking fetch must contain only fetched-state metadata"
                );
                for field in [
                    "subject",
                    "content",
                    "from",
                    "to",
                    "direction",
                    "created_at",
                    "attachments",
                    "attachments_error",
                ] {
                    assert!(result.get(field).is_none(), "body=false exposed {field}");
                }
            }
            assert_eq!(store.get_note(id(1)).await.unwrap().unwrap(), before);
            let inbox = registry
                .dispatch("comm.inbox", json!({"status":"unread"}))
                .await
                .unwrap();
            assert_eq!(
                inbox["messages"].as_array().unwrap().len(),
                usize::from(!expected_read)
            );
        }
    }
}

#[tokio::test]
async fn read_mark_default_true_and_existing_bulk_contract_stay_mutating() {
    for explicit in [false, true] {
        for body in [false, true] {
            let (runtime, token, registry) = fixture();
            seed(&runtime, vec![message(1), message(2), message(3)]).await;
            for bulk in [false, true] {
                let mut params = if bulk {
                    json!({"ids":[id(3).to_string(),id(2).to_string(),id(3).to_string()],"body":body})
                } else {
                    json!({"id":id(1).to_string(),"body":body})
                };
                if explicit {
                    params["mark"] = json!(true);
                }
                let result = registry.dispatch("comm.read", params).await.unwrap();
                assert_eq!(result["status"], "success");
                let rows: Vec<&Value> = if bulk {
                    assert_eq!(result["requested_count"], 3);
                    assert_eq!(result["unique_count"], 2);
                    assert_eq!(result["marked_count"], 2);
                    assert_eq!(result["failed_count"], 0);
                    assert_eq!(
                        full_ids(&result),
                        vec![id(3).to_string(), id(2).to_string()]
                    );
                    result["results"].as_array().unwrap().iter().collect()
                } else {
                    vec![&result]
                };
                for row in rows {
                    let row_id = Uuid::parse_str(row["full_id"].as_str().unwrap()).unwrap();
                    let stored = runtime
                        .notes(&token)
                        .unwrap()
                        .get_note(row_id)
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        stored.properties.as_ref().unwrap()["read"],
                        true,
                        "omitted or true mark must retain the released mutation behavior"
                    );
                    assert_eq!(row["read"], true);
                    if body {
                        assert_eq!(row["content"], stored.content);
                    } else {
                        assert!(row.get("content").is_none());
                    }
                }
            }
            let inbox = registry
                .dispatch("comm.inbox", json!({"status":"unread"}))
                .await
                .unwrap();
            assert!(inbox["messages"].as_array().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn nonmarking_read_rejects_bulk_and_invalid_mark_before_target_retrieval() {
    let (runtime, token, registry) = fixture();
    seed(&runtime, vec![message(1)]).await;
    let before = runtime
        .notes(&token)
        .unwrap()
        .get_note(id(1))
        .await
        .unwrap()
        .unwrap();
    for ids in [
        json!([]),
        json!([id(1).to_string()]),
        json!([id(1).to_string(), id(1).to_string()]),
    ] {
        let reads = reader_acquisitions(&runtime);
        let error = registry
            .dispatch("comm.read", json!({"ids":ids,"mark":false}))
            .await
            .expect_err("nonmarking bulk must refuse");
        assert!(matches!(error.refusal_source(), RuntimeError::InvalidInput(message)
            if message == "read: mark=false requires a single `id`; `ids` is not supported"),
            "mark=false with ids must refuse before target retrieval even when ids is empty: {error:?}");
        assert_eq!(error.audit_error_class().as_str(), "invalid_params");
        assert_eq!(reader_acquisitions(&runtime), reads);
    }
    for params in [
        json!({"id":id(1).to_string(),"mark":null}),
        json!({"id":id(1).to_string(),"mark":"false"}),
        json!({"id":id(1).to_string(),"mark":0}),
        json!({"id":id(1).to_string(),"ids":[],"mark":false}),
        json!({"mark":false}),
    ] {
        let reads = reader_acquisitions(&runtime);
        let error = registry.dispatch("comm.read", params).await.unwrap_err();
        assert!(matches!(
            error.refusal_source(),
            RuntimeError::InvalidInput(_)
        ));
        assert_eq!(error.audit_error_class().as_str(), "invalid_params");
        assert_eq!(reader_acquisitions(&runtime), reads);
    }
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(id(1))
            .await
            .unwrap()
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn nonmarking_read_reuses_typed_target_admission() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("lambda:maintainer".into()),
        brain_profile: None,
        packs: vec!["kg".into(), "comm".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::CommPack::new(runtime.clone()));
    builder.with_actor_id(Some("lambda:reader".into()));
    let registry = builder.build().unwrap();
    let mut notes: Vec<Note> = (1..=7).map(message).collect();
    notes[0].kind = "observation".into();
    notes[1].namespace = "lambda:foreign".into();
    notes[2].properties.as_mut().unwrap()["direction"] = json!("outbound");
    notes[3].properties.as_mut().unwrap()["direction"] = json!("outbound");
    notes[3].properties.as_mut().unwrap()["to_actor"] = json!("lambda:other");
    notes[4].properties.as_mut().unwrap()["to_actor"] = json!("lambda:other");
    notes[5]
        .properties
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("to_actor");
    notes[6].properties.as_mut().unwrap()["to_actor"] = json!("lambda:other");
    notes[6].properties.as_mut().unwrap()["quarantined"] = json!(true);
    seed(&runtime, notes.clone()).await;
    for index in 1..=9 {
        if index == 8 {
            continue;
        }
        for mark in [false, true] {
            let error = registry
                .dispatch("comm.read", json!({"id":id(index).to_string(),"mark":mark}))
                .await
                .expect_err("unauthorized or invalid target must not expose a message");
            let source = error.refusal_source();
            if matches!(index, 2 | 9) {
                assert!(matches!(source, RuntimeError::NotFound(_)),
                    "nonmarking read must use the same target admission and preserve typed refusal: {error:?}");
            } else {
                assert!(matches!(source, RuntimeError::InvalidInput(_)),
                    "nonmarking read must use the same target admission and preserve typed refusal: {error:?}");
                let expected = match index {
                    1 => format!("read: note {} is kind \"observation\", expected \"message\"", id(1)),
                    3 => format!("read: message {} is outbound; only received (inbound) messages can be marked as read", id(3)),
                    _ => "read: that message is not addressed to caller actor \"lambda:reader\"".into(),
                };
                assert!(
                    matches!(source, RuntimeError::InvalidInput(message) if message == &expected)
                );
            }
        }
    }
    for note in notes {
        assert_eq!(
            runtime
                .backend()
                .notes()
                .unwrap()
                .get_note(note.id)
                .await
                .unwrap()
                .unwrap(),
            note
        );
    }
}

#[tokio::test]
async fn nonmarking_read_preserves_typed_reader_timeout_and_recovers_without_marking() {
    use std::time::Duration;
    let (runtime, token, registry) = fixture();
    seed(&runtime, vec![message(1)]).await;
    let store = runtime.notes(&token).unwrap();
    let before = store.get_note(id(1)).await.unwrap().unwrap();
    let pool = runtime.backend().pool();
    let checkout_timeout = pool.config().checkout_timeout;
    let held_reader = pool.reader().expect("hold real reader permit");
    assert_eq!(
        pool.reader_acquisition_snapshot()
            .available_reader_admission_slots,
        0
    );
    tokio::time::pause();
    let params = json!({"id":id(1).to_string(),"mark":false,"body":false});
    let error = tokio::time::timeout(
        checkout_timeout.saturating_mul(3) + Duration::from_secs(1),
        registry.dispatch("comm.read", params.clone()),
    )
    .await
    .expect("bounded admission wait")
    .expect_err("real reader exhausted");
    let RuntimeError::Storage(khive_storage::StorageError::AdmissionTimeout {
        operation,
        timeout_ms,
        pool_identity,
    }) = error.refusal_source()
    else {
        panic!("nonmarking read erased the typed reader admission failure: {error:?}");
    };
    assert_eq!(operation.as_ref(), "get_note");
    assert_eq!(
        *timeout_ms,
        u64::try_from(checkout_timeout.as_millis()).unwrap()
    );
    assert_eq!(pool_identity.as_deref(), Some(":memory:"));
    let retry = error.retryable_failure_context().unwrap();
    assert_eq!(retry.stage, "storage_admission_timeout");
    assert_eq!(retry.operation.as_deref(), Some("get_note"));
    let wire = khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::Unknown);
    assert_eq!(wire["kind"], "unavailable");
    assert_eq!(wire["code"], "storage_admission_timeout");
    assert_eq!(wire["retryable"], true);
    assert_eq!(wire["operation"], "get_note");
    drop(held_reader);
    tokio::time::resume();
    let result = registry.dispatch("comm.read", params).await.unwrap();
    assert_eq!(result["status"], "success");
    assert_eq!(result["read"], false);
    assert_eq!(store.get_note(id(1)).await.unwrap().unwrap(), before);
}

#[tokio::test]
async fn nonmarking_read_rejects_nonempty_bulk_before_target_retrieval() {
    let (runtime, token, registry) = fixture();
    seed(&runtime, vec![message(1)]).await;
    let store = runtime.notes(&token).unwrap();
    let before = store.get_note(id(1)).await.unwrap().unwrap();
    let reads = reader_acquisitions(&runtime);
    let error = registry
        .dispatch("comm.read", json!({"ids":[id(1).to_string()],"mark":false}))
        .await
        .expect_err("nonempty nonmarking bulk must refuse rather than mark its target");
    assert!(
        matches!(error.refusal_source(), RuntimeError::InvalidInput(message)
        if message == "read: mark=false requires a single `id`; `ids` is not supported")
    );
    assert_eq!(reader_acquisitions(&runtime), reads);
    assert_eq!(store.get_note(id(1)).await.unwrap().unwrap(), before);
}

#[tokio::test]
async fn nonmarking_read_rejects_null_ids_without_changing_default_marking() {
    let (runtime, token, registry) = fixture();
    seed(&runtime, vec![message(1), message(2), message(3)]).await;
    let store = runtime.notes(&token).unwrap();
    let before = store.get_note(id(1)).await.unwrap().unwrap();
    let inbox_before = registry
        .dispatch("comm.inbox", json!({"status":"unread"}))
        .await
        .unwrap();
    assert_eq!(inbox_before["messages"].as_array().unwrap().len(), 3);
    for params in [
        json!({"id":id(1).to_string(),"ids":null,"mark":false}),
        json!({"ids":null,"mark":false}),
    ] {
        let reads = reader_acquisitions(&runtime);
        let error = registry
            .dispatch("comm.read", params)
            .await
            .expect_err("supplied null ids must refuse a nonmarking fetch");
        assert!(
            matches!(error.refusal_source(), RuntimeError::InvalidInput(message)
            if message == "read: mark=false requires a single `id`; `ids` is not supported"),
            "supplied null ids must use the nonmarking bulk refusal even without id: {error:?}"
        );
        assert_eq!(error.audit_error_class().as_str(), "invalid_params");
        assert_eq!(
            reader_acquisitions(&runtime),
            reads,
            "null ids refusal must precede target retrieval"
        );
        assert_eq!(store.get_note(id(1)).await.unwrap().unwrap(), before);
        let inbox_after = registry
            .dispatch("comm.inbox", json!({"status":"unread"}))
            .await
            .unwrap();
        assert_eq!(inbox_after["messages"], inbox_before["messages"]);
        assert_eq!(inbox_after["unread_count"], 3);
    }
    for (index, explicit) in [(2, false), (3, true)] {
        let mut params = json!({"id":id(index).to_string(),"ids":null});
        if explicit {
            params["mark"] = json!(true);
        }
        let response = registry.dispatch("comm.read", params).await.unwrap();
        assert_eq!(
            response["read"], true,
            "default and true mark must keep null ids compatibility"
        );
        assert_eq!(
            store
                .get_note(id(index))
                .await
                .unwrap()
                .unwrap()
                .properties
                .unwrap()["read"],
            true
        );
    }
    assert_eq!(store.get_note(id(1)).await.unwrap().unwrap(), before);
    let inbox_after = registry
        .dispatch("comm.inbox", json!({"status":"unread"}))
        .await
        .unwrap();
    assert_eq!(inbox_after["messages"].as_array().unwrap().len(), 1);
    assert_eq!(inbox_after["messages"][0]["full_id"], id(1).to_string());
}
