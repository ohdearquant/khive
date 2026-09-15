//! Tests for `gtd.next` and `gtd.tasks` query verbs: filtering, ordering, pagination.

mod common;

use common::{assign, pack, rt};
use serde_json::json;

#[tokio::test]
async fn next_returns_only_actionable_in_priority_order() {
    let pack = pack(rt());

    assign(
        &pack,
        json!({"title": "low", "status": "next", "priority": "p3"}),
    )
    .await;
    let _ = assign(&pack, json!({"title": "later", "status": "someday"})).await;
    assign(
        &pack,
        json!({"title": "urgent", "status": "next", "priority": "p0"}),
    )
    .await;
    assign(
        &pack,
        json!({"title": "mid", "status": "active", "priority": "p2"}),
    )
    .await;

    let resp = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 3, "only next/active count as actionable");
    let titles: Vec<&str> = arr.iter().map(|t| t["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["urgent", "mid", "low"]);
}

#[tokio::test]
async fn next_supports_assignee_filter() {
    let pack = pack(rt());
    assign(
        &pack,
        json!({"title": "alice's job", "status": "next", "assignee": "alice"}),
    )
    .await;
    assign(
        &pack,
        json!({"title": "bob's job", "status": "next", "assignee": "bob"}),
    )
    .await;

    let resp = pack
        .dispatch("gtd.next", json!({"assignee": "alice"}))
        .await
        .unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "alice's job");
}

#[tokio::test]
async fn tasks_filters_by_status_and_priority() {
    let pack = pack(rt());
    assign(
        &pack,
        json!({"title": "p0 waiting", "priority": "p0", "status": "waiting"}),
    )
    .await;
    assign(
        &pack,
        json!({"title": "p2 next", "priority": "p2", "status": "next"}),
    )
    .await;
    assign(
        &pack,
        json!({"title": "p0 next", "priority": "p0", "status": "next"}),
    )
    .await;

    let resp = pack
        .dispatch("gtd.tasks", json!({"status": "next"}))
        .await
        .unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 2);

    let resp = pack
        .dispatch("gtd.tasks", json!({"status": "next", "priority": "p0"}))
        .await
        .unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "p0 next");
}

#[tokio::test]
async fn tasks_priority_filter_excludes_terminal_by_default() {
    let pack = pack(rt());

    let a = assign(
        &pack,
        json!({"title": "A", "priority": "p0", "status": "inbox"}),
    )
    .await;
    let b = assign(
        &pack,
        json!({"title": "B", "priority": "p0", "status": "inbox"}),
    )
    .await;
    let _c = assign(
        &pack,
        json!({"title": "C", "priority": "p0", "status": "next"}),
    )
    .await;
    let d = assign(
        &pack,
        json!({"title": "D", "priority": "p0", "status": "inbox"}),
    )
    .await;

    let b_id = b["full_id"].as_str().unwrap().to_string();
    let d_id = d["full_id"].as_str().unwrap().to_string();
    pack.dispatch("gtd.transition", json!({"id": b_id, "status": "done"}))
        .await
        .expect("B->done");
    pack.dispatch("gtd.transition", json!({"id": d_id, "status": "cancelled"}))
        .await
        .expect("D->cancelled");

    let resp = pack
        .dispatch("gtd.tasks", json!({"priority": "p0"}))
        .await
        .unwrap();
    let arr = resp.as_array().unwrap();
    let titles: Vec<&str> = arr
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        !titles.contains(&"B"),
        "tasks(priority=p0) must exclude done task B; got: {titles:?}"
    );
    assert!(
        !titles.contains(&"D"),
        "tasks(priority=p0) must exclude cancelled task D; got: {titles:?}"
    );
    assert!(
        titles.contains(&"A"),
        "tasks(priority=p0) must include inbox task A; got: {titles:?}"
    );
    assert!(
        titles.contains(&"C"),
        "tasks(priority=p0) must include next task C; got: {titles:?}"
    );
    assert_eq!(arr.len(), 2, "expected exactly A and C; got: {titles:?}");

    let resp_done = pack
        .dispatch("gtd.tasks", json!({"priority": "p0", "status": "done"}))
        .await
        .unwrap();
    let arr_done = resp_done.as_array().unwrap();
    assert_eq!(
        arr_done.len(),
        1,
        "explicit status=done must return exactly B"
    );
    assert_eq!(arr_done[0]["title"], "B");

    let resp_all = pack.dispatch("gtd.tasks", json!({})).await.unwrap();
    let all_titles: Vec<&str> = resp_all
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        !all_titles.contains(&"B"),
        "tasks() default must exclude done task B; got: {all_titles:?}"
    );
    assert!(
        !all_titles.contains(&"D"),
        "tasks() default must exclude cancelled task D; got: {all_titles:?}"
    );

    let _ = a["full_id"].as_str();
}

/// #96: an empty `gtd.tasks()` result must be distinguishable from "no such
/// task" when the emptiness is caused by the default terminal-status filter.
/// Complete the ONLY task in this runtime, then query with no `status` —
/// the previous behavior was a bare `[]`, indistinguishable from the task
/// never having existed. It must now carry the applied-filter signal.
#[tokio::test]
async fn tasks_empty_default_result_signals_terminal_filter_applied() {
    let pack = pack(rt());

    let t = assign(&pack, json!({"title": "solo task", "status": "inbox"})).await;
    let t_id = t["full_id"].as_str().unwrap().to_string();
    pack.dispatch("gtd.transition", json!({"id": t_id, "status": "done"}))
        .await
        .expect("inbox -> done");

    let resp = pack.dispatch("gtd.tasks", json!({})).await.unwrap();
    assert!(
        resp.is_object(),
        "empty-because-filtered result must be an object carrying the applied \
         filter, not a bare array; got: {resp:?}"
    );
    let tasks = resp["tasks"]
        .as_array()
        .expect("the object must still carry the (empty) tasks array");
    assert!(
        tasks.is_empty(),
        "no non-terminal tasks exist; got: {tasks:?}"
    );
    let excluded = resp["filter_excluded"]
        .as_array()
        .expect("filter_excluded must list the statuses the default filter excluded");
    let excluded_strs: Vec<&str> = excluded.iter().filter_map(|v| v.as_str()).collect();
    assert!(excluded_strs.contains(&"done"));
    assert!(excluded_strs.contains(&"cancelled"));

    // Confirm the task is NOT actually missing — it still exists, just
    // filtered by default. Explicit status=done must surface it.
    let done_resp = pack
        .dispatch("gtd.tasks", json!({"status": "done"}))
        .await
        .unwrap();
    let done_arr = done_resp
        .as_array()
        .expect("explicit status= must keep the bare-array shape");
    assert_eq!(
        done_arr.len(),
        1,
        "the completed task must still be readable by id/status"
    );
    assert_eq!(done_arr[0]["full_id"], t_id);
}

#[tokio::test]
async fn tasks_empty_namespace_stays_bare_array() {
    let pack = pack(rt());

    let resp = pack.dispatch("gtd.tasks", json!({})).await.unwrap();
    assert_eq!(
        resp,
        json!([]),
        "a namespace with no tasks must keep the established empty-array response"
    );
}

#[tokio::test]
async fn tasks_unmatched_assignee_stays_bare_array() {
    let pack = pack(rt());
    let task = assign(
        &pack,
        json!({"title": "alice's task", "status": "inbox", "assignee": "alice"}),
    )
    .await;
    pack.dispatch(
        "gtd.transition",
        json!({"id": task["full_id"], "status": "done"}),
    )
    .await
    .expect("inbox -> done");

    let resp = pack
        .dispatch("gtd.tasks", json!({"assignee": "bob"}))
        .await
        .unwrap();
    assert_eq!(
        resp,
        json!([]),
        "a terminal task for another assignee must not trigger the hint"
    );
}

#[tokio::test]
async fn tasks_offset_beyond_last_active_match_stays_bare_array() {
    let pack = pack(rt());
    assign(&pack, json!({"title": "only task", "status": "inbox"})).await;

    let resp = pack
        .dispatch("gtd.tasks", json!({"offset": 1}))
        .await
        .unwrap();
    assert_eq!(
        resp,
        json!([]),
        "pagination past the final active task must keep the established empty-array response"
    );
}

/// A non-empty default `gtd.tasks()` result must keep the plain bare-array
/// shape every existing caller (`kkernel`, `li` surfaces, this crate's own
/// tests) already depends on via `.as_array()` — the new object wrapper is
/// strictly additive to the previously-uninformative empty case, never a
/// change to the common (non-empty) path.
#[tokio::test]
async fn tasks_non_empty_default_result_stays_bare_array() {
    let pack = pack(rt());
    assign(&pack, json!({"title": "still open", "status": "inbox"})).await;

    let resp = pack.dispatch("gtd.tasks", json!({})).await.unwrap();
    assert!(
        resp.is_array(),
        "a non-empty default result must stay a bare array; got: {resp:?}"
    );
}

#[tokio::test]
async fn next_excludes_terminal_tasks() {
    let pack = pack(rt());

    let t1 = assign(&pack, json!({"title": "active-task", "status": "next"})).await;
    let t2 = assign(&pack, json!({"title": "done-task", "status": "inbox"})).await;
    let t2_id = t2["full_id"].as_str().unwrap().to_string();

    pack.dispatch("gtd.transition", json!({"id": t2_id, "status": "done"}))
        .await
        .expect("done transition");

    let resp = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let titles: Vec<&str> = resp
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();

    assert!(
        titles.contains(&"active-task"),
        "next must include actionable task; got: {titles:?}"
    );
    assert!(
        !titles.contains(&"done-task"),
        "next must not include done task; got: {titles:?}"
    );

    let _ = t1["full_id"].as_str();
}

#[tokio::test]
async fn next_ordering_is_deterministic_on_equal_priority_and_timestamp() {
    let pack = pack(rt());

    for title in &["task-a", "task-b", "task-c"] {
        assign(
            &pack,
            json!({"title": title, "status": "next", "priority": "p1"}),
        )
        .await;
    }

    let first = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let second = pack.dispatch("gtd.next", json!({})).await.unwrap();

    assert_eq!(
        first, second,
        "gtd.next must return identical ordering on repeated calls with the same task set"
    );
}

// ── #744/#2679: gtd.tasks / gtd.next 200-row clamp + its disclosure ──────────

/// Under-limit control arm: `limit` below the 200 cap is unaffected — the response
/// stays the bare `Value::Array` it was before #2679 (`.as_array()` panics if the
/// handler switched shape, so this doubles as the shape-unchanged assertion).
#[tokio::test]
async fn next_limit_under_cap_returns_requested_count_unaffected() {
    let pack = pack(rt());
    for i in 0..5 {
        assign(
            &pack,
            json!({"title": format!("task-{i}"), "status": "next"}),
        )
        .await;
    }

    let resp = pack
        .dispatch("gtd.next", json!({"limit": 10}))
        .await
        .unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(
        arr.len(),
        5,
        "limit=10 with only 5 actionable tasks must return all 5, unaffected by the cap"
    );
}

/// Boundary: `limit=200` is exactly the cap — `requested == effective`, so this must
/// NOT be reported as clamped and must NOT switch response shape. Off-by-one here
/// (e.g. a `>=` where the handler means `>`) is the failure this shape invites.
#[tokio::test]
async fn next_limit_at_cap_boundary_is_not_clamped() {
    let pack = pack(rt());
    for i in 0..3 {
        assign(
            &pack,
            json!({"title": format!("task-{i}"), "status": "next"}),
        )
        .await;
    }

    let resp = pack
        .dispatch("gtd.next", json!({"limit": 200}))
        .await
        .unwrap();
    let arr = resp.as_array().expect(
        "limit == the 200 cap must not be reported as clamped, so the response stays a bare array",
    );
    assert_eq!(arr.len(), 3);
}

/// Over-limit: `gtd.next(limit=500)` clamps to 200 and #2679 now discloses it — the
/// response switches from a bare array to an object carrying the same
/// requested/effective/clamped field spelling as `khive-pack-kg`'s `list.rs` and
/// `context.rs`.
#[tokio::test]
async fn next_limit_over_cap_clamps_to_200_and_reports_it() {
    let pack = pack(rt());
    for i in 0..205 {
        assign(
            &pack,
            json!({"title": format!("task-{i}"), "status": "next"}),
        )
        .await;
    }

    let resp = pack
        .dispatch("gtd.next", json!({"limit": 500}))
        .await
        .unwrap();
    assert!(
        resp.as_array().is_none(),
        "a fired clamp must switch the response to an object, not stay a bare array"
    );
    let tasks = resp["tasks"]
        .as_array()
        .expect("clamped gtd.next response must carry the results under `tasks`");
    assert_eq!(
        tasks.len(),
        200,
        "limit=500 over 205 actionable tasks must clamp to exactly 200"
    );
    assert_eq!(resp["requested_limit"], 500);
    assert_eq!(resp["effective_limit"], 200);
    assert_eq!(resp["limit_clamped"], true);
}

/// Under-limit control arm: `gtd.tasks(limit=...)` below the cap is unaffected.
#[tokio::test]
async fn tasks_limit_under_cap_returns_requested_count_unaffected() {
    let pack = pack(rt());
    for i in 0..5 {
        assign(
            &pack,
            json!({"title": format!("task-{i}"), "status": "next"}),
        )
        .await;
    }

    let resp = pack
        .dispatch("gtd.tasks", json!({"limit": 10}))
        .await
        .unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 5, "limit=10 with only 5 tasks must return all 5");
}

/// Boundary: `limit=200` is exactly the cap for `gtd.tasks` too — must not clamp.
#[tokio::test]
async fn tasks_limit_at_cap_boundary_is_not_clamped() {
    let pack = pack(rt());
    for i in 0..3 {
        assign(
            &pack,
            json!({"title": format!("task-{i}"), "status": "next"}),
        )
        .await;
    }

    let resp = pack
        .dispatch("gtd.tasks", json!({"limit": 200}))
        .await
        .unwrap();
    let arr = resp.as_array().expect(
        "limit == the 200 cap must not be reported as clamped, so the response stays a bare array",
    );
    assert_eq!(arr.len(), 3);
}

/// Over-limit: `gtd.tasks(limit=500)` clamps to 200 and reports it, mirroring `gtd.next`.
#[tokio::test]
async fn tasks_limit_over_cap_clamps_to_200_and_reports_it() {
    let pack = pack(rt());
    for i in 0..205 {
        assign(
            &pack,
            json!({"title": format!("task-{i}"), "status": "next"}),
        )
        .await;
    }

    let resp = pack
        .dispatch("gtd.tasks", json!({"limit": 500}))
        .await
        .unwrap();
    assert!(
        resp.as_array().is_none(),
        "a fired clamp must switch the response to an object, not stay a bare array"
    );
    let tasks = resp["tasks"]
        .as_array()
        .expect("clamped gtd.tasks response must carry the results under `tasks`");
    assert_eq!(
        tasks.len(),
        200,
        "limit=500 over 205 tasks must clamp to exactly 200"
    );
    assert_eq!(resp["requested_limit"], 500);
    assert_eq!(resp["effective_limit"], 200);
    assert_eq!(resp["limit_clamped"], true);
}

/// The `filter_excluded`/`hint` object wrap (#96) already switches `gtd.tasks` to an
/// object for an unrelated reason (default filter hid a terminal-only match); when a
/// clamp *also* fires on that same call, the three clamp fields must land on that
/// object too, not be dropped because the object already existed for another reason.
#[tokio::test]
async fn tasks_filter_excluded_object_also_carries_a_fired_clamp_report() {
    let pack = pack(rt());
    let t = assign(&pack, json!({"title": "closed", "status": "inbox"})).await;
    let t_id = t["full_id"].as_str().unwrap().to_string();
    pack.dispatch("gtd.transition", json!({"id": t_id, "status": "done"}))
        .await
        .expect("inbox -> done");

    let resp = pack
        .dispatch("gtd.tasks", json!({"limit": 500}))
        .await
        .unwrap();
    assert_eq!(
        resp["tasks"].as_array().map(Vec::len),
        Some(0),
        "the only task is terminal, so the default (open-only) filter excludes it"
    );
    assert!(
        resp["filter_excluded"].is_array(),
        "the #96 wrap must still fire"
    );
    assert_eq!(resp["requested_limit"], 500);
    assert_eq!(resp["effective_limit"], 200);
    assert_eq!(resp["limit_clamped"], true);
}

/// The 200 cap is documented on both verbs' `limit` ParamDef (issue #744 fallback
/// ask 1); #2679 additionally makes the response itself disclose a fired clamp.
#[tokio::test]
async fn next_and_tasks_limit_param_documents_the_200_cap() {
    use khive_pack_gtd::GtdPack;
    use khive_runtime::pack::HandlerDef;
    use khive_types::Pack;

    let handlers: &[HandlerDef] = GtdPack::HANDLERS;
    for verb in ["gtd.next", "gtd.tasks"] {
        let h = handlers
            .iter()
            .find(|h| h.name == verb)
            .unwrap_or_else(|| panic!("{verb} must be declared"));
        let limit = h
            .params
            .iter()
            .find(|p| p.name == "limit")
            .unwrap_or_else(|| panic!("{verb} must declare a limit param"));
        assert!(
            limit.description.contains("200"),
            "{verb}.limit description must document the 200 cap; got: {:?}",
            limit.description
        );
    }
}

/// The task body is accepted by `gtd.assign` (stored as `properties.description`
/// and mirrored into content); the verb's help must advertise it, or a client
/// reading `help=true` cannot discover the body field.
#[tokio::test]
async fn assign_advertises_the_description_param_and_stores_it() {
    use khive_pack_gtd::GtdPack;
    use khive_runtime::pack::HandlerDef;
    use khive_types::Pack;

    let handlers: &[HandlerDef] = GtdPack::HANDLERS;
    let assign_def = handlers
        .iter()
        .find(|h| h.name == "gtd.assign")
        .expect("gtd.assign must be declared");
    let description = assign_def
        .params
        .iter()
        .find(|p| p.name == "description")
        .expect("gtd.assign must advertise a description param");
    assert_eq!(description.param_type, "string");
    assert!(!description.required);

    let fixture = pack(rt());
    let task = assign(
        &fixture,
        json!({"title": "body discovery", "description": "the body", "assignee": "lambda:test"}),
    )
    .await;
    let id = task["id"].as_str().expect("task id");
    let record = fixture
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get task record");
    let stored = record["properties"]["description"].as_str();
    assert_eq!(
        stored,
        Some("the body"),
        "task record must carry the body; got {record}"
    );
}

// Additive task-query filters must pass through the public registry and filter
// the SQL candidate set before either pagination or the excluded-state probe.
fn issue_2678_ids(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .expect("ordinary task results retain the existing array shape")
        .iter()
        .map(|task| task["full_id"].as_str().expect("full task UUID").to_owned())
        .collect()
}

fn issue_2678_id_set(value: &serde_json::Value) -> std::collections::BTreeSet<String> {
    issue_2678_ids(value).into_iter().collect()
}

async fn issue_2678_context(fixture: &common::Fixture, name: &str, namespace: &str) -> String {
    fixture
        .dispatch(
            "create",
            json!({"kind": "concept", "name": name, "namespace": namespace,
                   "skip_dedup_check": true}),
        )
        .await
        .expect("create a real context in assign's primary namespace")["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn issue_2678_tags_any_all_nocase_and_unfiltered_controls() {
    let fixture = pack(rt());
    let mut tasks = Vec::new();
    for (title, tags) in [
        ("red", vec!["red"]),
        ("blue", vec!["blue"]),
        ("both", vec!["red", "blue"]),
        ("untagged", vec![]),
    ] {
        tasks.push(assign(&fixture, json!({"title": title, "tags": tags})).await);
    }
    let legacy = fixture.dispatch("gtd.tasks", json!({})).await.unwrap();
    assert_eq!(issue_2678_ids(&legacy).len(), 4);
    for args in [
        json!({"tags": null}),
        json!({"tags": []}),
        json!({"tag_mode": null}),
        json!({"tag_mode": "all"}),
        json!({"tags": [], "tag_mode": "all", "context_entity_id": null}),
    ] {
        assert_eq!(fixture.dispatch("gtd.tasks", args).await.unwrap(), legacy);
    }
    let union: std::collections::BTreeSet<String> = tasks[..3]
        .iter()
        .map(|task| task["full_id"].as_str().unwrap().to_owned())
        .collect();
    for args in [
        json!({"tags": ["RED", "blue"]}),
        json!({"tags": ["red", "BLUE"], "tag_mode": "any"}),
        json!({"tags": ["RED", "red", "blue"], "tag_mode": null}),
    ] {
        let result = fixture.dispatch("gtd.tasks", args).await.unwrap();
        assert_eq!(issue_2678_id_set(&result), union);
        assert_eq!(
            issue_2678_ids(&result).len(),
            3,
            "duplicate tags do not duplicate rows"
        );
    }
    for tags in [json!(["RED", "blue"]), json!(["red", "RED", "BLUE"])] {
        let result = fixture
            .dispatch("gtd.tasks", json!({"tags": tags, "tag_mode": "all"}))
            .await
            .unwrap();
        assert_eq!(
            issue_2678_ids(&result),
            vec![tasks[2]["full_id"].as_str().unwrap()]
        );
    }
}

#[tokio::test]
async fn issue_2678_context_compares_canonical_uuid_without_requiring_live_anchor() {
    let fixture = pack(rt());
    let alpha = issue_2678_context(&fixture, "context-alpha", "local").await;
    let beta = issue_2678_context(&fixture, "context-beta", "local").await;
    let anchored = assign(
        &fixture,
        json!({"title": "alpha task", "context_entity_id": alpha}),
    )
    .await;
    assign(
        &fixture,
        json!({"title": "beta task", "context_entity_id": beta}),
    )
    .await;
    assign(&fixture, json!({"title": "unanchored task"})).await;
    let legacy = fixture.dispatch("gtd.tasks", json!({})).await.unwrap();
    assert_eq!(issue_2678_ids(&legacy).len(), 3);
    assert_eq!(
        fixture
            .dispatch("gtd.tasks", json!({"context_entity_id": null}))
            .await
            .unwrap(),
        legacy
    );
    let parsed = uuid::Uuid::parse_str(&alpha).unwrap();
    let noncanonical = parsed.simple().to_string();
    assert_ne!(noncanonical, alpha);
    assert_eq!(uuid::Uuid::parse_str(&noncanonical).unwrap(), parsed);
    let legacy_stored = assign(&fixture, json!({"title": "noncanonical stored context"})).await;
    fixture
        .dispatch(
            "update",
            json!({"id": legacy_stored["full_id"],
                   "properties": {"context_entity_id": noncanonical}}),
        )
        .await
        .expect("store the literal context property through the public update path");
    let stored = fixture
        .dispatch("get", json!({"id": legacy_stored["full_id"]}))
        .await
        .unwrap();
    assert_eq!(stored["properties"]["context_entity_id"], noncanonical);
    let unfiltered = fixture.dispatch("gtd.tasks", json!({})).await.unwrap();
    assert_eq!(issue_2678_ids(&unfiltered).len(), 4);
    assert!(issue_2678_ids(&unfiltered)
        .contains(&legacy_stored["full_id"].as_str().unwrap().to_owned()));
    assert_eq!(
        fixture
            .dispatch("gtd.tasks", json!({"context_entity_id": null}))
            .await
            .unwrap(),
        unfiltered
    );
    for spelling in [
        alpha.clone(),
        alpha.to_ascii_uppercase(),
        parsed.simple().to_string(),
        parsed.urn().to_string(),
        parsed.braced().to_string(),
    ] {
        let result = fixture
            .dispatch("gtd.tasks", json!({"context_entity_id": spelling}))
            .await
            .unwrap();
        assert_eq!(
            issue_2678_ids(&result),
            vec![anchored["full_id"].as_str().unwrap()]
        );
        let row = &result[0];
        assert_eq!(row["context_entity_id"], alpha);
        assert_eq!(row["dependency_state"], "ready");
        assert_eq!(row["blocked_by"], json!([]));
        for field in ["created_at", "updated_at"] {
            chrono::DateTime::parse_from_rfc3339(row[field].as_str().unwrap()).unwrap();
        }
    }
    let absent = "00000000-0000-4000-8000-000000000678";
    let before = fixture
        .dispatch("list", json!({"kind": "entity"}))
        .await
        .unwrap();
    assert!(fixture
        .dispatch("get", json!({"id": absent}))
        .await
        .is_err());
    assert_eq!(
        fixture
            .dispatch("gtd.tasks", json!({"context_entity_id": absent}))
            .await
            .unwrap(),
        json!([])
    );
    assert_eq!(
        fixture
            .dispatch("list", json!({"kind": "entity"}))
            .await
            .unwrap(),
        before
    );
    assert!(fixture
        .dispatch("get", json!({"id": absent}))
        .await
        .is_err());
    fixture
        .dispatch("delete", json!({"id": alpha}))
        .await
        .expect("soft-delete anchor");
    assert!(fixture.dispatch("get", json!({"id": alpha})).await.is_err());
    let result = fixture
        .dispatch("gtd.tasks", json!({"context_entity_id": alpha}))
        .await
        .unwrap();
    assert_eq!(
        issue_2678_ids(&result),
        vec![anchored["full_id"].as_str().unwrap()]
    );
    assert_eq!(
        fixture
            .dispatch("get", json!({"id": legacy_stored["full_id"]}))
            .await
            .unwrap()["properties"]["context_entity_id"],
        noncanonical,
        "filtering must not rewrite the noncanonical stored reference"
    );
}

#[tokio::test]
async fn issue_2678_filters_combine_with_status_owner_priority_and_namespace_visibility() {
    let runtime = rt();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.with_visible_namespaces(vec![
        khive_runtime::Namespace::parse("filter-alpha").expect("valid fixture namespace")
    ]);
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_gtd::GtdPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    let fixture = common::Fixture { registry };
    let alpha = issue_2678_context(&fixture, "visible context", "filter-alpha").await;
    let other = issue_2678_context(&fixture, "other context", "filter-alpha").await;
    let base = json!({"title": "matching task", "namespace": "filter-alpha", "status": "next",
                      "assignee": "alice", "priority": "p1", "tags": ["red"], "context_entity_id": alpha});
    let matching = assign(&fixture, base.clone()).await;
    for (field, value) in [
        ("tags", json!(["blue"])),
        ("context_entity_id", json!(other)),
        ("status", json!("waiting")),
        ("assignee", json!("bob")),
        ("priority", json!("p2")),
    ] {
        let mut args = base.clone();
        args["title"] = json!(format!("wrong {field}"));
        args[field] = value;
        assign(&fixture, args).await;
    }
    let local = assign(&fixture, json!({"title": "local red", "tags": ["red"]})).await;
    let beta = assign(
        &fixture,
        json!({"title": "hidden red", "namespace": "filter-beta", "tags": ["red"]}),
    )
    .await;
    let query = json!({"tags": ["RED"], "context_entity_id": alpha, "status": "next", "assignee": "alice", "priority": "p1"});
    // The configured visible set exposes the task despite its context living
    // outside the default primary namespace. Filtering must not resolve it there.
    assert_eq!(
        issue_2678_ids(&fixture.dispatch("gtd.tasks", query.clone()).await.unwrap()),
        vec![matching["full_id"].as_str().unwrap()]
    );
    let mut precise = query;
    precise["namespace"] = json!("filter-alpha");
    assert_eq!(
        issue_2678_ids(&fixture.dispatch("gtd.tasks", precise).await.unwrap()),
        vec![matching["full_id"].as_str().unwrap()]
    );
    let visible = fixture
        .dispatch("gtd.tasks", json!({"tags": ["red"]}))
        .await
        .unwrap();
    let ids = issue_2678_id_set(&visible);
    assert!(ids.contains(local["full_id"].as_str().unwrap()));
    assert!(ids.contains(matching["full_id"].as_str().unwrap()));
    assert!(!ids.contains(beta["full_id"].as_str().unwrap()));
    assert_eq!(
        issue_2678_ids(
            &fixture
                .dispatch(
                    "gtd.tasks",
                    json!({"namespace": "filter-beta", "tags": ["RED"]})
                )
                .await
                .unwrap()
        ),
        vec![beta["full_id"].as_str().unwrap()]
    );
}

#[tokio::test]
async fn issue_2678_tags_and_context_filter_before_limit_and_offset() {
    let fixture = pack(rt());
    let alpha = issue_2678_context(&fixture, "page alpha", "local").await;
    let beta = issue_2678_context(&fixture, "page beta", "local").await;
    let mut wanted = std::collections::BTreeSet::new();
    for index in 0..3 {
        let task = assign(&fixture, json!({"title": format!("older match {index}"), "tags": ["red"], "context_entity_id": alpha})).await;
        wanted.insert(task["full_id"].as_str().unwrap().to_owned());
    }
    for index in 0..6 {
        let (tags, context) = if index % 2 == 0 {
            (json!(["blue"]), &alpha)
        } else {
            (json!(["red"]), &beta)
        };
        assign(&fixture, json!({"title": format!("newer nonmatch {index}"), "tags": tags, "context_entity_id": context})).await;
    }
    let unfiltered = fixture.dispatch("gtd.tasks", json!({})).await.unwrap();
    let all_ids = issue_2678_ids(&unfiltered);
    assert_eq!(all_ids.len(), 9);
    assert!(
        all_ids[..2].iter().all(|id| !wanted.contains(id)),
        "newer nonmatches must occupy the unfiltered first page"
    );
    let expected: Vec<String> = all_ids
        .into_iter()
        .filter(|id| wanted.contains(id))
        .collect();
    let query = json!({"tags": ["RED"], "context_entity_id": alpha, "limit": 2, "offset": 1});
    let page = fixture.dispatch("gtd.tasks", query.clone()).await.unwrap();
    assert_eq!(issue_2678_ids(&page), expected[1..]);
    assert_eq!(
        fixture.dispatch("gtd.tasks", query).await.unwrap(),
        page,
        "stable order and rendering on repeat"
    );
    assert_eq!(
        issue_2678_ids(
            &fixture
                .dispatch(
                    "gtd.tasks",
                    json!({"tags": ["red"], "context_entity_id": alpha, "limit": 1})
                )
                .await
                .unwrap()
        ),
        expected[..1]
    );
    assert_eq!(
        fixture
            .dispatch(
                "gtd.tasks",
                json!({"tags": ["red"], "context_entity_id": alpha, "offset": 3})
            )
            .await
            .unwrap(),
        json!([])
    );
}

#[tokio::test]
async fn issue_2678_exclusion_probe_keeps_both_filters_and_tag_mode() {
    let fixture = pack(rt());
    let alpha = issue_2678_context(&fixture, "terminal alpha", "local").await;
    let beta = issue_2678_context(&fixture, "terminal beta", "local").await;
    let task = assign(
        &fixture,
        json!({"title": "done red alpha", "tags": ["red"], "context_entity_id": alpha}),
    )
    .await;
    fixture
        .dispatch(
            "gtd.transition",
            json!({"id": task["full_id"], "status": "done"}),
        )
        .await
        .unwrap();
    for args in [
        json!({"tags": ["blue"], "context_entity_id": alpha}),
        json!({"tags": ["red"], "context_entity_id": beta}),
        json!({"tags": ["red", "blue"], "tag_mode": "all", "context_entity_id": alpha}),
    ] {
        assert_eq!(
            fixture.dispatch("gtd.tasks", args).await.unwrap(),
            json!([]),
            "unrelated terminal rows must not trigger an exclusion hint"
        );
    }
    let legacy = fixture.dispatch("gtd.tasks", json!({})).await.unwrap();
    assert_eq!(legacy["tasks"], json!([]));
    assert_eq!(
        legacy["filter_excluded"],
        json!(["done", "cancelled", "unrecognized_status"])
    );
    for args in [
        json!({"tags": ["RED"], "context_entity_id": alpha}),
        json!({"tags": ["red", "blue"], "tag_mode": "any", "context_entity_id": alpha}),
        json!({"tags": [], "context_entity_id": null}),
    ] {
        assert_eq!(fixture.dispatch("gtd.tasks", args).await.unwrap(), legacy);
    }
    let done = fixture
        .dispatch(
            "gtd.tasks",
            json!({"status": "done", "tags": ["red"], "context_entity_id": alpha}),
        )
        .await
        .unwrap();
    assert_eq!(
        issue_2678_ids(&done),
        vec![task["full_id"].as_str().unwrap()]
    );
}

#[tokio::test]
async fn issue_2678_rejects_malformed_filters_without_domain_mutation() {
    let fixture = pack(rt());
    let context = issue_2678_context(&fixture, "refusal context", "local").await;
    assign(
        &fixture,
        json!({"title": "preserved task", "tags": ["red"], "context_entity_id": context}),
    )
    .await;
    let tasks = fixture.dispatch("gtd.tasks", json!({})).await.unwrap();
    let entities = fixture
        .dispatch("list", json!({"kind": "entity"}))
        .await
        .unwrap();
    for args in [
        json!({"tags": "red"}),
        json!({"tags": ["red", 1]}),
        json!({"tags": {"red": true}}),
        json!({"tag_mode": "ALL"}),
        json!({"tag_mode": "some"}),
        json!({"tag_mode": true}),
        json!({"context_entity_id": 7}),
        json!({"context_entity_id": {"id": context}}),
    ] {
        let error = fixture
            .dispatch("gtd.tasks", args)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("bad params"), "{error}");
    }
    for invalid in [
        "not-a-uuid",
        "",
        &context[..8],
        "0000000000004000800000000000067",
    ] {
        let error = fixture
            .dispatch("gtd.tasks", json!({"context_entity_id": invalid}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("context_entity_id") && error.contains("full UUID"),
            "{error}"
        );
    }
    assert_eq!(
        fixture.dispatch("gtd.tasks", json!({})).await.unwrap(),
        tasks
    );
    assert_eq!(
        fixture
            .dispatch("list", json!({"kind": "entity"}))
            .await
            .unwrap(),
        entities
    );
}
