//! `gtd.counts` integration tests.
//!
//! Tasks are created with `gtd.assign`, then driven into the exact stored shape a
//! case needs (status, priority, `updated_at`, soft delete) with raw `UPDATE`s on
//! `notes`, the same raw-SQL seeding the census tests use.

mod common;

use std::collections::BTreeSet;

use common::{assign, pack, rt, Fixture};
use khive_pack_gtd::schema::TASK_STATUSES;
use khive_runtime::KhiveRuntime;
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

struct Env {
    rtm: KhiveRuntime,
    pack: Fixture,
}

impl Env {
    fn new() -> Self {
        let rtm = rt();
        let pack = pack(rtm.clone());
        Env { rtm, pack }
    }

    async fn exec(&self, sql: String, params: Vec<SqlValue>) {
        self.rtm
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement {
                sql,
                params,
                label: Some("gtd-counts-test".into()),
            })
            .await
            .unwrap();
    }

    async fn set_prop(&self, id: &str, key: &str, value: SqlValue) {
        self.exec(
            format!(
                "UPDATE notes SET properties = json_set(properties, '$.{key}', ?1) WHERE id = ?2"
            ),
            vec![value, SqlValue::Text(id.to_string())],
        )
        .await;
    }

    async fn remove_prop(&self, id: &str, key: &str) {
        self.exec(
            format!(
                "UPDATE notes SET properties = json_remove(properties, '$.{key}') WHERE id = ?1"
            ),
            vec![SqlValue::Text(id.to_string())],
        )
        .await;
    }

    async fn delete(&self, id: &str) {
        self.exec(
            "UPDATE notes SET deleted_at = ?1 WHERE id = ?2".to_string(),
            vec![SqlValue::Integer(19), SqlValue::Text(id.to_string())],
        )
        .await;
    }

    /// Create one task, then force its stored status, priority and `updated_at`.
    async fn task(
        &self,
        title: &str,
        assignee: &str,
        tags: &[&str],
        status: &str,
        priority: Option<&str>,
        updated_at: i64,
    ) -> String {
        let mut body = json!({"title": title, "assignee": assignee});
        if !tags.is_empty() {
            body["tags"] = json!(tags);
        }
        let created = assign(&self.pack, body).await;
        let id = created["full_id"]
            .as_str()
            .expect("assign returns full_id")
            .to_string();
        if status != "inbox" {
            self.set_prop(&id, "status", SqlValue::Text(status.to_string()))
                .await;
        }
        if let Some(priority) = priority {
            self.set_prop(&id, "priority", SqlValue::Text(priority.to_string()))
                .await;
        }
        self.exec(
            "UPDATE notes SET updated_at = ?1 WHERE id = ?2".to_string(),
            vec![SqlValue::Integer(updated_at), SqlValue::Text(id.clone())],
        )
        .await;
        id
    }

    async fn counts(&self, args: Value) -> Value {
        self.pack
            .dispatch("gtd.counts", args)
            .await
            .expect("gtd.counts ok")
    }

    async fn tasks(&self, args: Value) -> Vec<Value> {
        self.pack
            .dispatch("gtd.tasks", args)
            .await
            .expect("gtd.tasks ok")
            .as_array()
            .expect("tasks array")
            .clone()
    }
}

fn key_set(value: &Value) -> BTreeSet<String> {
    value.as_object().expect("object").keys().cloned().collect()
}

fn names(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| (*k).to_string()).collect()
}

fn row_titles(tag_block: &Value) -> Vec<String> {
    tag_block["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["title"].as_str().expect("title").to_string())
        .collect()
}

/// Catches: counts drifting from `gtd.tasks` for any status, the second assignee
/// leaking into the first one's numbers, and a legacy task with an absent or
/// non-text status not counting as inbox in both reads.
#[tokio::test]
async fn counts_by_status_equal_task_listing_for_every_status() {
    let env = Env::new();
    let alice_plan = [
        ("inbox", 2),
        ("next", 1),
        ("waiting", 3),
        ("someday", 1),
        ("active", 2),
        ("done", 4),
        ("cancelled", 2),
    ];
    for (status, n) in alice_plan {
        for i in 0..n {
            env.task(
                &format!("alice {status} {i}"),
                "alice",
                &[],
                status,
                None,
                10,
            )
            .await;
        }
    }
    let absent = env
        .task("legacy absent", "alice", &[], "inbox", None, 10)
        .await;
    env.remove_prop(&absent, "status").await;
    let nontext = env
        .task("legacy nontext", "alice", &[], "inbox", None, 10)
        .await;
    env.set_prop(&nontext, "status", SqlValue::Integer(7)).await;
    for (status, n) in [("inbox", 2), ("next", 1), ("done", 1)] {
        for i in 0..n {
            env.task(&format!("bob {status} {i}"), "bob", &[], status, None, 10)
                .await;
        }
    }

    let r = env.counts(json!({"assignee": "alice"})).await;
    assert_eq!(r["assignee"], "alice");
    for &status in TASK_STATUSES.iter() {
        let listed = env
            .tasks(json!({"assignee": "alice", "status": status}))
            .await
            .len() as u64;
        assert_eq!(
            r["by_status"][status].as_u64(),
            Some(listed),
            "counts and gtd.tasks disagree for status {status}"
        );
    }
    let expected = [
        ("inbox", 4u64),
        ("next", 1),
        ("waiting", 3),
        ("someday", 1),
        ("active", 2),
        ("done", 4),
        ("cancelled", 2),
    ];
    for (status, want) in expected {
        assert_eq!(r["by_status"][status].as_u64(), Some(want), "{status}");
    }
    assert_eq!(r["open"], 11);
    assert_eq!(r["total"], 17);

    let bob = env.counts(json!({"assignee": "bob"})).await;
    assert_eq!(bob["by_status"]["inbox"], 2);
    assert_eq!(bob["by_status"]["next"], 1);
    assert_eq!(bob["by_status"]["done"], 1);
    assert_eq!(bob["total"], 4);
}

/// Catches: a missing key standing in for zero, `open` counting terminal
/// statuses, `total` not being the sum, and an unknown assignee erroring.
#[tokio::test]
async fn every_status_key_present_with_zero_and_open_total_add_up() {
    let env = Env::new();
    env.task("one next", "alice", &[], "next", None, 10).await;
    env.task("one done", "alice", &[], "done", None, 10).await;

    let r = env.counts(json!({"assignee": "alice"})).await;
    let by_status = r["by_status"].as_object().expect("by_status object");
    assert_eq!(by_status.len(), TASK_STATUSES.len());
    for &status in TASK_STATUSES.iter() {
        let want = match status {
            "next" | "done" => 1,
            _ => 0,
        };
        assert_eq!(by_status[status].as_u64(), Some(want), "{status}");
    }
    assert_eq!(r["open"], 1, "the done task must not count as open");
    assert_eq!(r["total"], 2);
    assert_eq!(
        key_set(&r),
        names(&["assignee", "by_status", "open", "total"])
    );

    let nobody = env.counts(json!({"assignee": "nobody"})).await;
    assert_eq!(nobody["open"], 0);
    assert_eq!(nobody["total"], 0);
    for &status in TASK_STATUSES.iter() {
        assert_eq!(nobody["by_status"][status].as_u64(), Some(0), "{status}");
    }
}

/// Catches: dropping the legacy-status fallback, in the per-status count (inbox
/// predicate) and in the tag block's open set.
#[tokio::test]
async fn legacy_task_without_text_status_counts_as_inbox() {
    let env = Env::new();
    env.task("finished", "alice", &[], "done", None, 1).await;
    let absent = env
        .task("absent", "alice", &["session:l"], "inbox", None, 2)
        .await;
    env.remove_prop(&absent, "status").await;
    let nontext = env
        .task("nontext", "alice", &["session:l"], "inbox", None, 3)
        .await;
    env.set_prop(&nontext, "status", SqlValue::Integer(7)).await;

    let r = env
        .counts(json!({"assignee": "alice", "tag": "session:l"}))
        .await;
    assert_eq!(r["by_status"]["inbox"], 2);
    assert_eq!(r["by_status"]["done"], 1);
    assert_eq!(r["open"], 2);
    let listed = env
        .tasks(json!({"assignee": "alice", "status": "inbox"}))
        .await;
    assert_eq!(listed.len(), 2, "gtd.tasks reads the legacy rows as inbox");
    assert_eq!(r["tag"]["open"], 2);
    for row in r["tag"]["rows"].as_array().expect("rows") {
        assert_eq!(row["status"], "inbox");
    }
}

/// Catches: done/cancelled tagged tasks listed, prefix tags (`session:xy`) or
/// untagged tasks matching, another assignee's tagged task leaking in, extra
/// fields in rows or in the result, and row forms differing from `gtd.tasks`.
#[tokio::test]
async fn tag_block_counts_and_lists_only_open_tasks_with_exact_tag() {
    let env = Env::new();
    env.task("A", "alice", &["session:x"], "next", Some("p1"), 200)
        .await;
    env.task("B", "alice", &["session:x"], "inbox", Some("p0"), 100)
        .await;
    env.task("C done", "alice", &["session:x"], "done", Some("p0"), 50)
        .await;
    env.task(
        "D cancelled",
        "alice",
        &["session:x"],
        "cancelled",
        Some("p0"),
        60,
    )
    .await;
    env.task("E prefix", "alice", &["session:xy"], "next", Some("p0"), 70)
        .await;
    env.task("F untagged", "alice", &[], "inbox", Some("p0"), 80)
        .await;
    env.task(
        "G",
        "alice",
        &["session:x", "other"],
        "active",
        Some("p3"),
        300,
    )
    .await;
    env.task("H bob", "bob", &["session:x"], "inbox", Some("p0"), 90)
        .await;

    let r = env
        .counts(json!({"assignee": "alice", "tag": "session:x"}))
        .await;
    assert_eq!(
        key_set(&r),
        names(&["assignee", "by_status", "open", "total", "tag"])
    );
    let tag = &r["tag"];
    assert_eq!(key_set(tag), names(&["name", "open", "rows", "truncated"]));
    assert_eq!(tag["name"], "session:x");
    assert_eq!(tag["open"], 3);
    assert_eq!(tag["truncated"], false);
    assert_eq!(row_titles(tag), vec!["B", "A", "G"]);

    let listed = env
        .tasks(json!({"assignee": "alice", "tags": ["session:x"]}))
        .await;
    for row in tag["rows"].as_array().expect("rows") {
        assert_eq!(
            key_set(row),
            names(&["id", "title", "status", "priority", "updated_at"])
        );
        let task = listed
            .iter()
            .find(|t| t["id"] == row["id"])
            .expect("row is listed by gtd.tasks");
        for field in ["id", "title", "status", "priority", "updated_at"] {
            assert_eq!(
                row[field], task[field],
                "field {field} differs from gtd.tasks"
            );
        }
    }

    let plain = env.counts(json!({"assignee": "alice"})).await;
    assert!(plain.get("tag").is_none(), "no tag block without a tag");
}

/// Catches: an empty tag, an unknown key, a missing assignee or an out-of-range
/// limit being accepted.
#[tokio::test]
async fn invalid_params_are_refused() {
    let env = Env::new();
    let refused = |args: Value| {
        let pack = &env.pack;
        async move { pack.dispatch("gtd.counts", args).await.is_err() }
    };
    assert!(refused(json!({"assignee": "alice", "tag": ""})).await);
    assert!(refused(json!({"assignee": "alice", "bogus": 1})).await);
    assert!(refused(json!({})).await);
    assert!(refused(json!({"assignee": "alice", "limit": 0})).await);
    assert!(refused(json!({"assignee": "alice", "limit": 201})).await);
    assert!(!refused(json!({"assignee": "alice", "limit": 200})).await);
    assert!(!refused(json!({"assignee": "alice", "limit": 1})).await);
}

/// Catches: rows not in priority order then oldest first (including a legacy
/// task with no priority, which reads as p2, and an unrecognised text priority,
/// which sorts last), `open` not counting every open tagged task, and
/// `truncated` being wrong when the page is exactly full.
#[tokio::test]
async fn rows_are_ordered_and_truncation_is_exact() {
    let env = Env::new();
    let tag = ["session:o"];
    env.task("t1", "alice", &tag, "next", Some("p2"), 50).await;
    env.task("t2", "alice", &tag, "active", Some("p0"), 300)
        .await;
    env.task("t3", "alice", &tag, "inbox", Some("p0"), 100)
        .await;
    env.task("t4", "alice", &tag, "waiting", Some("p1"), 200)
        .await;
    env.task("t5", "alice", &tag, "someday", Some("p3"), 10)
        .await;
    env.task("t6", "alice", &tag, "next", Some("zz"), 5).await;
    env.task("t7", "alice", &tag, "inbox", Some("p2"), 20).await;
    let t8 = env.task("t8", "alice", &tag, "inbox", None, 40).await;
    env.remove_prop(&t8, "priority").await;
    env.remove_prop(&t8, "status").await;
    let full = vec!["t3", "t2", "t4", "t7", "t8", "t1", "t5", "t6"];

    let cut = env
        .counts(json!({"assignee": "alice", "tag": "session:o", "limit": 5}))
        .await;
    assert_eq!(row_titles(&cut["tag"]), full[..5].to_vec());
    assert_eq!(cut["tag"]["open"], 8, "open counts every open tagged task");
    assert_eq!(cut["tag"]["truncated"], true);

    let exact = env
        .counts(json!({"assignee": "alice", "tag": "session:o", "limit": 8}))
        .await;
    assert_eq!(row_titles(&exact["tag"]), full);
    assert_eq!(exact["tag"]["open"], 8);
    assert_eq!(
        exact["tag"]["truncated"], false,
        "exactly `limit` open tasks is not truncated"
    );

    let one_short = env
        .counts(json!({"assignee": "alice", "tag": "session:o", "limit": 7}))
        .await;
    assert_eq!(row_titles(&one_short["tag"]), full[..7].to_vec());
    assert_eq!(one_short["tag"]["truncated"], true);

    let default = env
        .counts(json!({"assignee": "alice", "tag": "session:o"}))
        .await;
    assert_eq!(row_titles(&default["tag"]), full);
    assert_eq!(default["tag"]["truncated"], false);
}

/// Catches: a soft-deleted task still counted in `by_status`, `open`, `total`,
/// the tag block, or listed in its rows, while `gtd.tasks` no longer lists it.
#[tokio::test]
async fn deleted_task_is_not_counted() {
    let env = Env::new();
    let keep = env
        .task("keep", "alice", &["session:d"], "inbox", None, 1)
        .await;
    let gone = env
        .task("gone", "alice", &["session:d"], "inbox", None, 2)
        .await;
    assert_ne!(keep, gone);
    env.delete(&gone).await;

    let r = env
        .counts(json!({"assignee": "alice", "tag": "session:d"}))
        .await;
    assert_eq!(r["by_status"]["inbox"], 1);
    assert_eq!(r["open"], 1);
    assert_eq!(r["total"], 1);
    assert_eq!(r["tag"]["open"], 1);
    assert_eq!(row_titles(&r["tag"]), vec!["keep"]);
    assert_eq!(env.tasks(json!({"assignee": "alice"})).await.len(), 1);
}
