//! Tests for pack metadata, schema plans, NoteKindSpecs, and response shapes.

mod common;

use common::{assign, pack, rt};
use khive_pack_gtd::GtdPack;
use khive_runtime::{NoteKindSpec, SchemaPlan};
use serde_json::json;

#[tokio::test]
async fn pack_metadata_matches_trait_consts() {
    let pack = pack(rt());
    assert_eq!(pack.name(), "gtd");
    assert!(pack.note_kinds().contains(&"task"));
    let verbs: Vec<&str> = pack.verbs().iter().map(|v| v.name).collect();
    assert!(verbs.contains(&"gtd.assign"));
    assert!(verbs.contains(&"gtd.next"));
    assert!(verbs.contains(&"gtd.complete"));
    assert!(verbs.contains(&"gtd.tasks"));
    assert!(verbs.contains(&"gtd.transition"));
}

#[tokio::test]
async fn unknown_verb_returns_invalid_input() {
    let pack = pack(rt());
    let err = pack.dispatch("retire", json!({})).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown verb"), "got: {msg}");
    assert!(msg.contains("retire"), "got: {msg}");
}

#[tokio::test]
async fn pack_runtime_exposes_schema_plan() {
    use khive_runtime::PackRuntime;
    let pack = GtdPack::new(rt());
    let plan: SchemaPlan = pack.schema_plan();
    assert!(
        !plan.is_empty(),
        "GtdPack must return a non-empty SchemaPlan"
    );
    assert_eq!(plan.pack, "gtd");
    assert!(
        !plan.statements.is_empty(),
        "schema plan must have at least one DDL statement"
    );
    let combined = plan.statements.join(" ");
    assert!(
        combined.contains("gtd_lifecycle_audit"),
        "schema plan must reference gtd_lifecycle_audit table; got: {combined}"
    );
    assert!(
        combined.contains("CREATE TABLE IF NOT EXISTS"),
        "schema plan DDL must be idempotent (CREATE TABLE IF NOT EXISTS)"
    );
}

#[tokio::test]
async fn verb_registry_aggregates_schema_plans() {
    let fixture = pack(rt());
    let plans = fixture.registry.all_schema_plans();
    assert!(
        plans.iter().any(|p| p.pack == "gtd"),
        "registry must expose GTD schema plan; got packs: {:?}",
        plans.iter().map(|p| p.pack).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn pack_runtime_exposes_note_kind_spec_for_task() {
    use khive_runtime::PackRuntime;
    let pack = GtdPack::new(rt());
    let specs: &[NoteKindSpec] = pack.note_kind_specs();
    assert!(
        !specs.is_empty(),
        "GtdPack must declare at least one NoteKindSpec"
    );

    let task_spec = specs
        .iter()
        .find(|s| s.kind == "task")
        .expect("GtdPack must have NoteKindSpec for 'task'");

    assert_eq!(
        task_spec.lifecycle.field, "kind_status",
        "lifecycle field must be 'kind_status' to avoid collision with NoteStatus"
    );
    assert_eq!(
        task_spec.lifecycle.initial, "inbox",
        "task lifecycle must start at 'inbox'"
    );
    assert!(
        task_spec.lifecycle.terminal.contains(&"done"),
        "terminal states must include 'done'"
    );
    assert!(
        task_spec.lifecycle.terminal.contains(&"cancelled"),
        "terminal states must include 'cancelled'"
    );
}

#[tokio::test]
async fn verb_registry_aggregates_note_kind_specs() {
    let fixture = pack(rt());
    let specs = fixture.registry.all_note_kind_specs();
    assert!(
        specs.iter().any(|s| s.kind == "task"),
        "registry must aggregate task NoteKindSpec"
    );
}

#[tokio::test]
async fn note_kind_spec_transitions_match_runtime_schema() {
    use khive_pack_gtd::schema::{can_transition, is_terminal};
    use khive_runtime::PackRuntime;

    let pack = GtdPack::new(rt());
    let specs = pack.note_kind_specs();
    let task_spec = specs.iter().find(|s| s.kind == "task").unwrap();

    for &(from, to) in task_spec.lifecycle.transitions {
        assert!(
            can_transition(from, to),
            "NoteKindSpec declares ({from}->{to}) but schema::can_transition disagrees"
        );
    }
    for &t in task_spec.lifecycle.terminal {
        assert!(
            is_terminal(t),
            "NoteKindSpec declares '{t}' as terminal but schema::is_terminal disagrees"
        );
    }
}

// ── Response shape tests ────────────────────────────────────────────────────

#[tokio::test]
async fn get_task_exposes_gtd_status_not_row_visibility() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "status remap test"})).await;
    let full_id = resp["full_id"].as_str().unwrap().to_string();

    let got = pack
        .dispatch("get", json!({"id": full_id}))
        .await
        .expect("get must succeed");

    assert!(
        got.get("data").is_none(),
        "get must NOT wrap in {{data: ...}} (P-H2); got: {got}"
    );
    assert_eq!(
        got["status"], "inbox",
        "get(task) must expose GTD status 'inbox' at top-level status; got: {got}"
    );
    assert_eq!(
        got["lifecycle"], "active",
        "get(task) must move row-visibility to top-level lifecycle; got: {got}"
    );
}

#[tokio::test]
async fn get_task_after_transition_exposes_updated_gtd_status() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "transition remap test"})).await;
    let full_id = resp["full_id"].as_str().unwrap().to_string();

    pack.dispatch("gtd.transition", json!({"id": full_id, "status": "active"}))
        .await
        .expect("transition to active must succeed");

    let got = pack
        .dispatch("get", json!({"id": full_id}))
        .await
        .expect("get after transition must succeed");

    assert_eq!(
        got["status"], "active",
        "after transition to active, status must be 'active' (GTD); got: {got}"
    );
    assert_eq!(
        got["lifecycle"], "active",
        "row-visibility must remain 'active' for a live task; got: {got}"
    );
}

#[tokio::test]
async fn get_task_after_complete_exposes_done_status() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "complete remap test"})).await;
    let full_id = resp["full_id"].as_str().unwrap().to_string();

    pack.dispatch("gtd.transition", json!({"id": full_id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    pack.dispatch("gtd.complete", json!({"id": full_id, "result": "shipped"}))
        .await
        .expect("complete must succeed");

    let got = pack
        .dispatch("get", json!({"id": full_id}))
        .await
        .expect("get after complete must succeed");

    assert_eq!(
        got["status"], "done",
        "after complete, status must be 'done'; got: {got}"
    );
    assert_eq!(
        got["lifecycle"], "active",
        "soft-completed task row-visibility is still 'active'; got: {got}"
    );
}

#[tokio::test]
async fn list_task_exposes_gtd_status_not_row_visibility() {
    let pack = pack(rt());
    assign(&pack, json!({"title": "list remap inbox"})).await;
    assign(&pack, json!({"title": "list remap next", "status": "next"})).await;

    let list_resp = pack
        .dispatch("list", json!({"kind": "task"}))
        .await
        .expect("list must succeed");
    let items = list_resp.as_array().expect("list must return array");

    let statuses: Vec<&str> = items.iter().filter_map(|n| n["status"].as_str()).collect();

    assert!(
        statuses.contains(&"inbox"),
        "list(task) must expose 'inbox' GTD status; got: {statuses:?}"
    );
    assert!(
        statuses.contains(&"next"),
        "list(task) must expose 'next' GTD status; got: {statuses:?}"
    );
    assert!(
        !statuses.iter().all(|&s| s == "active"),
        "list(task) must NOT return row-visibility 'active' as the only status; got: {statuses:?}"
    );

    for item in items {
        assert_eq!(
            item["lifecycle"], "active",
            "list(task) must include lifecycle field for row-visibility; got item: {item}"
        );
    }
}

#[tokio::test]
async fn transition_response_includes_task_fields() {
    let pack = pack(rt());
    let resp = assign(
        &pack,
        json!({"title": "snapshot task", "priority": "p1", "assignee": "alice"}),
    )
    .await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let r = pack
        .dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition must succeed");

    assert_eq!(r["transitioned"], true);
    assert_eq!(r["title"], "snapshot task", "response must include title");
    assert_eq!(r["priority"], "p1", "response must include priority");
    assert_eq!(r["assignee"], "alice", "response must include assignee");
    assert!(
        r.get("due").is_some(),
        "response must include due (null if unset)"
    );
}

#[tokio::test]
async fn timestamps_are_rfc3339_across_verbs() {
    let pack = pack(rt());
    let resp = assign(
        &pack,
        json!({"title": "ts check", "due": "2026-06-01T00:00:00Z"}),
    )
    .await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    for field in &["created_at", "updated_at"] {
        let ts = resp[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} missing"));
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap_or_else(|e| panic!("{field} not RFC 3339: {ts} -- {e}"));
    }
    let due = resp["due"].as_str().expect("due must be a string");
    chrono::DateTime::parse_from_rfc3339(due)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due} -- {e}"));

    let tasks = pack.dispatch("gtd.tasks", json!({})).await.unwrap();
    let task = tasks
        .as_array()
        .unwrap()
        .first()
        .expect("at least one task");
    for field in &["created_at", "updated_at"] {
        let ts = task[field]
            .as_str()
            .unwrap_or_else(|| panic!("tasks.{field} missing"));
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap_or_else(|e| panic!("tasks.{field} not RFC 3339: {ts} -- {e}"));
    }

    pack.dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    let done = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .unwrap();
    let completed_at = done["completed_at"].as_str().expect("completed_at missing");
    chrono::DateTime::parse_from_rfc3339(completed_at)
        .unwrap_or_else(|e| panic!("completed_at not RFC 3339: {completed_at} -- {e}"));
}

#[tokio::test]
async fn complete_writes_status_column_to_done() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "Write notes.status on complete"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    pack.dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    pack.dispatch("gtd.complete", json!({"id": id}))
        .await
        .expect("complete must succeed");

    let fetched = pack
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after complete must succeed");

    let status = fetched["status"].as_str().unwrap_or("<missing>");
    assert_eq!(
        status, "done",
        "notes.status column must be 'done' after complete (Fix 3); got: {status}"
    );
}

#[tokio::test]
async fn transition_writes_status_column() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "Write notes.status on transition"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    pack.dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition inbox->next must succeed");

    let fetched = pack
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after transition must succeed");

    let status = fetched["status"].as_str().unwrap_or("<missing>");
    assert_eq!(
        status, "next",
        "notes.status column must be 'next' after transition (Fix 3); got: {status}"
    );
}
