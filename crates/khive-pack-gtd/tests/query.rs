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

// ── #744: gtd.tasks / gtd.next silent 200-row clamp ──────────────────────────

/// Under-limit: `limit` below the 200 cap is unaffected — no change from before #744.
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

/// Over-limit: `gtd.next(limit=500)` is silently clamped to 200 (issue #744 — the cap
/// itself is intentional and unchanged; #744 is about the *silence*, addressed here by
/// documenting the cap on the `limit` ParamDef rather than a response-shape change,
/// since the response is a bare JSON array consumed via `.as_array()` throughout the
/// codebase and a sibling `truncated` field would require a breaking wrap-in-object
/// change).
#[tokio::test]
async fn next_limit_over_cap_clamps_to_200() {
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
    let arr = resp.as_array().unwrap();
    assert_eq!(
        arr.len(),
        200,
        "limit=500 over 205 actionable tasks must clamp to exactly 200"
    );
}

/// Under-limit: `gtd.tasks(limit=...)` below the cap is unaffected.
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

/// Over-limit: `gtd.tasks(limit=500)` is silently clamped to 200, mirroring `gtd.next`.
#[tokio::test]
async fn tasks_limit_over_cap_clamps_to_200() {
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
    let arr = resp.as_array().unwrap();
    assert_eq!(
        arr.len(),
        200,
        "limit=500 over 205 tasks must clamp to exactly 200"
    );
}

/// The 200 cap is documented on both verbs' `limit` ParamDef (issue #744 fallback
/// ask 1), so `help=true`/verb introspection surfaces it even though the response
/// itself carries no truncation signal.
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
