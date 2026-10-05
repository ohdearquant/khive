mod common;

use common::{assign, rt, Fixture};
use khive_pack_gtd::handlers::{duplicate, DependencyOptions, TransitionDecision};
use khive_runtime::{
    run_atomic_unit, AffectedRowGuard, AtomicOpPlan, AtomicRunOutcome, GtdTransitionPlan,
    KhiveRuntime, Namespace, PlanStatement, PostCommitEffect,
};
use khive_storage::{Note, SqlStatement, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

fn pack(runtime: KhiveRuntime) -> Fixture {
    let fixture = common::pack(runtime.clone());
    fixture
        .registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .unwrap();
    fixture
}

async fn task(fixture: &Fixture, title: &str, namespace: &str) -> Uuid {
    let result = assign(fixture, json!({"title": title, "namespace": namespace})).await;
    result["full_id"].as_str().unwrap().parse().unwrap()
}

async fn note(runtime: &KhiveRuntime, id: Uuid) -> Note {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap()
}

async fn sql(runtime: &KhiveRuntime, statement: &str, params: Vec<SqlValue>) {
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: statement.into(),
            params,
            label: None,
        })
        .await
        .unwrap();
}

async fn snapshot(runtime: &KhiveRuntime) -> Vec<u8> {
    let mut rows = Vec::new();
    let mut reader = runtime.sql().reader().await.unwrap();
    for table in ["notes", "graph_edges", "events", "gtd_lifecycle_audit"] {
        rows.push(
            reader
                .query_all(SqlStatement {
                    sql: format!("SELECT * FROM {table} ORDER BY rowid"),
                    params: vec![],
                    label: None,
                })
                .await
                .unwrap(),
        );
    }
    serde_json::to_vec(&rows).unwrap()
}

async fn stored(runtime: &KhiveRuntime, ids: &[Uuid]) -> Vec<(Option<Value>, i64, i64)> {
    let mut rows = Vec::new();
    for id in ids {
        let row = note(runtime, *id).await;
        rows.push((row.properties, row.version, row.updated_at));
    }
    rows
}

async fn task_rows(runtime: &KhiveRuntime) -> usize {
    runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: "SELECT id FROM notes WHERE kind = 'task'".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap()
        .len()
}

async fn cancel(fixture: &Fixture, verb: &str, id: Uuid, partner: Value) -> Value {
    fixture
        .dispatch(
            verb,
            json!({"id": id, "status": "cancelled", "duplicate_of": partner}),
        )
        .await
        .unwrap()
}

async fn plan(runtime: &KhiveRuntime, id: Uuid, partner: Uuid) -> AtomicOpPlan {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let (decision, duplicate_of) = duplicate::prepare_transition(
        runtime,
        &token,
        &id.to_string(),
        "cancelled",
        None,
        DependencyOptions::default(),
        Some(&partner.to_string()),
    )
    .await
    .unwrap();
    let (statement, noop) = match decision {
        TransitionDecision::NoOp { note, current, .. } => (
            duplicate::noop_assertion_statement(&note, &current, duplicate_of).unwrap(),
            true,
        ),
        TransitionDecision::Write {
            note,
            current,
            target,
            props,
            updated_at,
            ..
        } => (
            duplicate::transition_statement(
                &note,
                &current,
                &target,
                &props,
                updated_at,
                duplicate_of,
            )
            .unwrap(),
            false,
        ),
    };
    AtomicOpPlan::GtdTransition(GtdTransitionPlan::new(
        id,
        vec![PlanStatement {
            statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
        noop,
        PostCommitEffect::None,
    ))
}

fn preceding_write(id: Uuid, statement: &str) -> AtomicOpPlan {
    AtomicOpPlan::GtdTransition(GtdTransitionPlan::new(
        id,
        vec![PlanStatement {
            statement: SqlStatement {
                sql: statement.into(),
                params: vec![SqlValue::Text(id.to_string())],
                label: None,
            },
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
        false,
        PostCommitEffect::None,
    ))
}

#[tokio::test]
async fn cancellation_records_canonical_partner_and_reverse_read_for_both_verbs() {
    for verb in ["gtd.transition", "gtd.complete"] {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        let kept = task(&fixture, "kept", "elsewhere").await;
        let duplicate = task(&fixture, "duplicate", "local").await;
        let prefix = kept.simple().to_string()[..12].to_string();
        let result = cancel(&fixture, verb, duplicate, json!(prefix)).await;
        assert_eq!(result["to"], "cancelled");
        let stored = note(&runtime, duplicate).await;
        assert_eq!(
            stored.properties.as_ref().unwrap()["duplicate_of"],
            kept.to_string()
        );
        assert_eq!(stored.properties.as_ref().unwrap()["status"], "cancelled");
        let get = fixture
            .dispatch("get", json!({"id": duplicate}))
            .await
            .unwrap();
        assert_eq!(get["properties"]["duplicate_of"], kept.to_string());
        let tasks = fixture
            .dispatch("gtd.tasks", json!({"duplicate_of": kept}))
            .await
            .unwrap();
        assert_eq!(tasks.as_array().unwrap().len(), 1);
        assert_eq!(tasks[0]["full_id"], duplicate.to_string());
        assert_eq!(tasks[0]["properties"]["duplicate_of"], kept.to_string());
        let edges = runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_all(SqlStatement {
                sql: "SELECT id FROM graph_edges".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert!(
            edges.is_empty(),
            "duplicate judgments create no graph relation"
        );
    }
}

#[tokio::test]
async fn kept_task_may_have_any_lifecycle_status_and_cycles_are_not_checked() {
    for status in [
        "inbox",
        "next",
        "active",
        "waiting",
        "someday",
        "done",
        "cancelled",
    ] {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        let kept = task(&fixture, "kept", "local").await;
        sql(
            &runtime,
            "UPDATE notes SET properties=json_set(properties,'$.status',?1) WHERE id=?2",
            vec![
                SqlValue::Text(status.into()),
                SqlValue::Text(kept.to_string()),
            ],
        )
        .await;
        let duplicate = task(&fixture, "duplicate", "local").await;
        cancel(&fixture, "gtd.transition", duplicate, json!(kept)).await;
        assert_eq!(
            note(&runtime, duplicate).await.properties.unwrap()["duplicate_of"],
            kept.to_string()
        );
    }
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let a = task(&fixture, "A", "local").await;
    let b = task(&fixture, "B", "local").await;
    cancel(&fixture, "gtd.transition", a, json!(b)).await;
    cancel(&fixture, "gtd.complete", b, json!(a)).await;
    assert_eq!(
        note(&runtime, b).await.properties.unwrap()["duplicate_of"],
        a.to_string()
    );
}

#[tokio::test]
async fn invalid_partners_and_wrong_destinations_write_nothing() {
    for verb in ["gtd.transition", "gtd.complete"] {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        let source = task(&fixture, "source", "local").await;
        let deleted = task(&fixture, "deleted", "local").await;
        sql(
            &runtime,
            "UPDATE notes SET deleted_at=7 WHERE id=?1",
            vec![SqlValue::Text(deleted.to_string())],
        )
        .await;
        let other_note = fixture
            .dispatch("create", json!({"kind":"observation","content":"non-task"}))
            .await
            .unwrap();
        let entity = fixture
            .dispatch(
                "create",
                json!({"kind":"concept","name":"non-task entity","skip_dedup_check":true}),
            )
            .await
            .unwrap();
        for partner in [
            json!(source),
            json!(source.simple().to_string()[..12]),
            other_note["id"].clone(),
            entity["id"].clone(),
            json!(deleted),
            json!(Uuid::new_v4()),
            json!("bad-id"),
        ] {
            let before = snapshot(&runtime).await;
            fixture
                .dispatch(
                    verb,
                    json!({"id":source,"status":"cancelled","duplicate_of":partner}),
                )
                .await
                .expect_err("invalid partner must refuse");
            assert_eq!(
                snapshot(&runtime).await,
                before,
                "{verb} changed storage on refusal"
            );
        }
        let kept = task(&fixture, "valid kept", "local").await;
        for status in ["inbox", "next", "active", "waiting", "someday", "done"] {
            let before = snapshot(&runtime).await;
            fixture
                .dispatch(
                    verb,
                    json!({"id":source,"status":status,"duplicate_of":kept}),
                )
                .await
                .expect_err("duplicate_of requires cancellation");
            assert_eq!(snapshot(&runtime).await, before);
        }
        let before = snapshot(&runtime).await;
        fixture
            .dispatch("gtd.complete", json!({"id":source,"duplicate_of":kept}))
            .await
            .expect_err("default done refuses duplicate judgment");
        assert_eq!(snapshot(&runtime).await, before);
    }
}

#[tokio::test]
async fn ambiguous_partner_prefix_is_refused_without_writes() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let source = task(&fixture, "source", "local").await;
    for id in [
        "aaaaaaaa-0000-4000-8000-000000000001",
        "aaaaaaaa-0000-4000-8000-000000000002",
    ] {
        sql(&runtime, "INSERT INTO notes(id,namespace,kind,name,content,properties,created_at,updated_at) VALUES(?1,'other','task','ambiguous','body','{\"status\":\"inbox\"}',1,1)", vec![SqlValue::Text(id.into())]).await;
    }
    let before = snapshot(&runtime).await;
    fixture
        .dispatch(
            "gtd.transition",
            json!({"id":source,"status":"cancelled","duplicate_of":"aaaaaaaa"}),
        )
        .await
        .expect_err("ambiguous prefix");
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn recancel_only_asserts_the_exact_recorded_judgment() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let kept = task(&fixture, "kept", "local").await;
    let other = task(&fixture, "other", "local").await;
    let source = task(&fixture, "source", "local").await;
    cancel(&fixture, "gtd.transition", source, json!(kept)).await;
    let before = snapshot(&runtime).await;
    let result = cancel(
        &fixture,
        "gtd.transition",
        source,
        json!(kept.simple().to_string()[..12]),
    )
    .await;
    assert_eq!(result["transitioned"], false);
    assert_eq!(result["note_recorded"], false);
    assert_eq!(
        snapshot(&runtime).await,
        before,
        "exact judgment must be a mutation-free assertion"
    );
    for verb in ["gtd.transition", "gtd.complete"] {
        fixture
            .dispatch(
                verb,
                json!({"id":source,"status":"cancelled","duplicate_of":other}),
            )
            .await
            .expect_err("changed judgment");
        assert_eq!(snapshot(&runtime).await, before);
    }
    fixture
        .dispatch(
            "gtd.complete",
            json!({"id":source,"status":"cancelled","duplicate_of":kept}),
        )
        .await
        .expect_err("complete keeps terminal refusal");
    assert_eq!(snapshot(&runtime).await, before);
    let bare = task(&fixture, "bare cancellation", "local").await;
    fixture
        .dispatch("gtd.transition", json!({"id":bare,"status":"cancelled"}))
        .await
        .unwrap();
    let before = snapshot(&runtime).await;
    fixture
        .dispatch(
            "gtd.transition",
            json!({"id":bare,"status":"cancelled","duplicate_of":kept}),
        )
        .await
        .expect_err("new judgment cannot be added on no-op");
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn generic_update_cannot_set_change_or_clear_a_duplicate_judgment() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let kept = task(&fixture, "kept", "local").await;
    let other = task(&fixture, "other", "local").await;
    let recorded = task(&fixture, "recorded", "local").await;
    cancel(&fixture, "gtd.transition", recorded, json!(kept)).await;
    let bare = task(&fixture, "bare cancellation", "local").await;
    fixture
        .dispatch("gtd.transition", json!({"id":bare,"status":"cancelled"}))
        .await
        .unwrap();
    let open = task(&fixture, "open", "local").await;
    let ids = [recorded, bare, open];
    let before = stored(&runtime, &ids).await;
    for (target, value) in [
        (bare, json!(kept)),
        (recorded, json!(other)),
        (recorded, Value::Null),
        (recorded, json!(recorded)),
        (open, json!(kept)),
        (open, json!(Uuid::new_v4())),
    ] {
        let error = fixture
            .dispatch(
                "update",
                json!({"id":target,"properties":{"duplicate_of":value}}),
            )
            .await
            .expect_err("generic update must refuse a duplicate judgment");
        assert!(
            error
                .to_string()
                .contains("properties.duplicate_of is lifecycle-owned"),
            "{error}"
        );
        assert_eq!(stored(&runtime, &ids).await, before);
    }
    assert_eq!(
        note(&runtime, recorded).await.properties.unwrap()["duplicate_of"],
        kept.to_string()
    );
    let tasks = fixture
        .dispatch("gtd.tasks", json!({"duplicate_of":kept}))
        .await
        .unwrap();
    assert_eq!(tasks.as_array().unwrap().len(), 1);
    assert_eq!(tasks[0]["full_id"], recorded.to_string());
    let patch = json!({"id":open,"properties":{"priority":"p1"}});
    fixture
        .dispatch("update", patch)
        .await
        .expect("other properties stay patchable");
}

#[tokio::test]
async fn creating_a_task_with_a_duplicate_judgment_is_refused_and_creates_nothing() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let kept = task(&fixture, "kept", "local").await;
    let before = task_rows(&runtime).await;
    for value in [json!(kept), Value::Null, json!("not-a-task")] {
        let error = fixture
            .dispatch(
                "create",
                json!({
                    "kind": "note",
                    "note_kind": "task",
                    "title": "forged duplicate",
                    "properties": {"duplicate_of": value}
                }),
            )
            .await
            .expect_err("create must refuse a duplicate judgment");
        assert!(
            error
                .to_string()
                .contains("properties.duplicate_of cannot be set when creating a task"),
            "{error}"
        );
        assert_eq!(task_rows(&runtime).await, before);
    }
    fixture
        .dispatch(
            "create",
            json!({"kind":"note","note_kind":"task","title":"ordinary"}),
        )
        .await
        .expect("an ordinary task still creates");
    assert_eq!(task_rows(&runtime).await, before + 1);
}

#[tokio::test]
async fn reverse_filter_preserves_explicit_status_pagination_and_namespace_visibility() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let kept = task(&fixture, "kept", "hidden").await;
    for n in 0..3 {
        let id = task(&fixture, &format!("local duplicate {n}"), "local").await;
        cancel(&fixture, "gtd.transition", id, json!(kept)).await;
        task(&fixture, &format!("unrelated {n}"), "local").await;
    }
    let hidden = task(&fixture, "hidden duplicate", "hidden").await;
    cancel(&fixture, "gtd.transition", hidden, json!(kept)).await;
    let bare = task(&fixture, "cancelled without a duplicate", "local").await;
    fixture
        .dispatch("gtd.transition", json!({"id":bare,"status":"cancelled"}))
        .await
        .unwrap();
    let other_kept = task(&fixture, "other kept", "local").await;
    let other_dup = task(&fixture, "duplicate of another", "local").await;
    cancel(&fixture, "gtd.transition", other_dup, json!(other_kept)).await;
    let all = fixture
        .dispatch("gtd.tasks", json!({"duplicate_of":kept}))
        .await
        .unwrap();
    assert_eq!(all.as_array().unwrap().len(), 3);
    let listing = all.to_string();
    assert!(listing.contains(&kept.to_string()));
    for excluded in [bare, other_dup] {
        assert!(!listing.contains(&excluded.to_string()));
    }
    for offset in 0..3 {
        let page = fixture
            .dispatch(
                "gtd.tasks",
                json!({"duplicate_of":kept,"limit":1,"offset":offset}),
            )
            .await
            .unwrap();
        assert_eq!(page[0]["full_id"], all[offset]["full_id"]);
    }
    let explicit = fixture
        .dispatch("gtd.tasks", json!({"duplicate_of":kept,"status":"done"}))
        .await
        .unwrap();
    assert_eq!(explicit, json!([]));
    let hidden_result = fixture
        .dispatch(
            "gtd.tasks",
            json!({"duplicate_of":kept,"namespace":"hidden"}),
        )
        .await
        .unwrap();
    assert_eq!(hidden_result.as_array().unwrap().len(), 1);
    assert_eq!(hidden_result[0]["full_id"], hidden.to_string());
    sql(
        &runtime,
        "UPDATE notes SET deleted_at=9 WHERE id=?1",
        vec![SqlValue::Text(kept.to_string())],
    )
    .await;
    let retained = fixture
        .dispatch("gtd.tasks", json!({"duplicate_of":kept}))
        .await
        .unwrap();
    assert_eq!(
        retained, all,
        "stored judgments remain queryable by full UUID after partner deletion"
    );
}

#[tokio::test]
async fn duplicate_guard_rolls_back_prior_partner_mutation_in_atomic_unit() {
    for statement in [
        "UPDATE notes SET deleted_at=7 WHERE id=?1",
        "UPDATE notes SET kind='observation' WHERE id=?1",
        "DELETE FROM notes WHERE id=?1",
    ] {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        let kept = task(&fixture, "kept", "local").await;
        let source = task(&fixture, "source", "local").await;
        let prepared = plan(&runtime, source, kept).await;
        let before = snapshot(&runtime).await;
        let outcome = run_atomic_unit(
            runtime.sql().as_ref(),
            vec![preceding_write(kept, statement), prepared],
        )
        .await
        .unwrap();
        assert!(
            matches!(
                outcome,
                AtomicRunOutcome::RolledBack {
                    failed_op_index: 1,
                    ..
                }
            ),
            "partner mutation must invalidate prepared cancellation: {outcome:?}"
        );
        assert_eq!(
            snapshot(&runtime).await,
            before,
            "both the partner mutation and cancellation must roll back"
        );
    }
}

#[tokio::test]
async fn duplicate_guard_revalidates_source_version_and_exact_noop_in_atomic_unit() {
    for noop in [false, true] {
        for statement in ["UPDATE notes SET version=version+1 WHERE id=?1", "UPDATE notes SET properties=json_set(properties,'$.duplicate_of','changed') WHERE id=?1"] {
            let runtime = rt(); let fixture = pack(runtime.clone());
            let kept = task(&fixture, "kept", "local").await;
            let source = task(&fixture, "source", "local").await;
            if noop { cancel(&fixture, "gtd.transition", source, json!(kept)).await; }
            let prepared = plan(&runtime, source, kept).await;
            let before = snapshot(&runtime).await;
            let outcome = run_atomic_unit(runtime.sql().as_ref(), vec![preceding_write(source, statement), prepared]).await.unwrap();
            assert!(matches!(outcome, AtomicRunOutcome::RolledBack { failed_op_index: 1, .. }), "stale source must invalidate prepared cancellation: {outcome:?}");
            assert_eq!(snapshot(&runtime).await, before);
        }
    }
}

#[tokio::test]
async fn exact_duplicate_atomic_assertion_is_mutation_free_and_checks_partner() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let kept = task(&fixture, "kept", "local").await;
    let source = task(&fixture, "source", "local").await;
    cancel(&fixture, "gtd.transition", source, json!(kept)).await;
    let prepared = plan(&runtime, source, kept).await;
    let before = snapshot(&runtime).await;
    let outcome = run_atomic_unit(runtime.sql().as_ref(), vec![prepared])
        .await
        .unwrap();
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));
    assert_eq!(snapshot(&runtime).await, before);
    let prepared = plan(&runtime, source, kept).await;
    let outcome = run_atomic_unit(
        runtime.sql().as_ref(),
        vec![
            preceding_write(kept, "UPDATE notes SET deleted_at=1 WHERE id=?1"),
            prepared,
        ],
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        AtomicRunOutcome::RolledBack {
            failed_op_index: 1,
            ..
        }
    ));
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
async fn ordinary_cancellation_output_and_default_listing_remain_unchanged() {
    for verb in ["gtd.transition", "gtd.complete"] {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        let source = task(&fixture, "source", "local").await;
        let result = fixture
            .dispatch(verb, json!({"id":source,"status":"cancelled"}))
            .await
            .unwrap();
        assert!(result.get("duplicate_of").is_none());
        assert!(note(&runtime, source)
            .await
            .properties
            .unwrap()
            .get("duplicate_of")
            .is_none());
        let tasks = fixture.dispatch("gtd.tasks", json!({})).await.unwrap();
        assert!(tasks["filter_excluded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "cancelled"));
        let mut keys: Vec<_> = result
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut expected = if verb == "gtd.complete" {
            vec![
                "completed",
                "id",
                "full_id",
                "from",
                "to",
                "completed_at",
                "is_terminal",
                "audit_persisted",
            ]
        } else {
            vec![
                "transitioned",
                "id",
                "full_id",
                "from",
                "to",
                "is_terminal",
                "title",
                "priority",
                "assignee",
                "due",
                "due_timezone",
                "audit_persisted",
            ]
        };
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }
}

#[tokio::test]
async fn duplicate_reference_is_advertised_by_all_three_public_handlers() {
    let fixture = pack(rt());
    for verb in ["gtd.transition", "gtd.complete", "gtd.tasks"] {
        let handlers = fixture.verbs();
        let handler = handlers
            .iter()
            .find(|handler| handler.name == verb)
            .unwrap();
        let param = handler
            .params
            .iter()
            .find(|param| param.name == "duplicate_of")
            .unwrap();
        assert_eq!(param.param_type, "uuid");
        assert!(!param.required);
        assert!(matches!(
            param.resolution_mode,
            khive_runtime::pack::IdResolutionMode::UnscopedById
        ));
        let help = fixture.dispatch(verb, json!({"help":true})).await.unwrap();
        assert_eq!(help, fixture.registry.describe_verb(verb).unwrap());
        assert!(help["params"]
            .as_array()
            .unwrap()
            .iter()
            .any(|param| param["name"] == "duplicate_of"));
    }
}
