use std::collections::HashSet;
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, KhiveRuntime, Namespace, NamespaceToken,
    RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::Note;
use serde_json::{json, Value};

#[derive(Debug)]
struct ListPolicy(bool);
impl Gate for ListPolicy {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(if request.verb == "list" && !self.0 {
            GateDecision::deny("listing denied")
        } else {
            GateDecision::allow()
        })
    }
}

fn fixture(list_allowed: bool) -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(Arc::new(ListPolicy(list_allowed)));
    builder.register(KgPack::new(runtime.clone()));
    (runtime, token, builder.build().unwrap())
}

fn details(error: RuntimeError) -> Value {
    let RuntimeError::Khive(error) = error else {
        panic!("structured error expected, got {error:?}");
    };
    serde_json::to_value(error.details().unwrap()).unwrap()
}

async fn seed(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    kind: &str,
    key: Option<&str>,
    time: i64,
    tags: &[&str],
) -> Note {
    let mut note = Note::new(token.namespace().as_str(), kind, "{}");
    note.key = key.map(str::to_owned);
    note.created_at = time;
    note.updated_at = time;
    note.properties = Some(json!({"tags": tags}));
    runtime
        .notes(token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    note
}

async fn walk(registry: &VerbRegistry, mut args: Value) -> Vec<Value> {
    args["after"] = json!("");
    args["limit"] = json!(1);
    let mut rows = Vec::new();
    for _ in 0..100 {
        let page = registry.dispatch("list", args.clone()).await.unwrap();
        rows.extend(page["notes"].as_array().unwrap().iter().cloned());
        if page["next_after"].is_null() {
            return rows;
        }
        args["after"] = page["next_after"].clone();
    }
    panic!("cursor walk did not terminate");
}

#[tokio::test]
async fn version_head_roundtrip_cas_key_conflicts_and_kind_isolation() {
    let (_, _, registry) = fixture(true);
    let create = json!({"kind":"note", "note_kind":"head", "key":"run/1/lease", "content":"{}", "tags":["kind:lease"]});
    let note = registry.dispatch("create", create.clone()).await.unwrap();
    assert_eq!(note["version"], 1);
    assert_eq!(
        registry
            .dispatch("get", json!({"key":"run/1/lease", "note_kind":"head"}))
            .await
            .unwrap()["id"],
        note["id"]
    );
    let error = details(registry.dispatch("create", create).await.unwrap_err());
    assert_eq!(error["reason"], "key_conflict");
    assert_eq!(error["existing_id"], note["id"]);
    let update = json!({"id":note["id"], "expected_version":1, "content":"{\"owner\":1}"});
    assert_eq!(
        registry.dispatch("update", update.clone()).await.unwrap()["version"],
        2
    );
    let error = details(registry.dispatch("update", update).await.unwrap_err());
    assert_eq!(error["reason"], "version_conflict");
    assert_eq!(error["current_version"], "2");
    registry.dispatch("create", json!({"kind":"observation", "key":"run/1/lease", "content":"same key, different kind", "embed":false})).await.unwrap();
    assert_eq!(
        details(
            registry
                .dispatch("get", json!({"key":"run/1/lease"}))
                .await
                .unwrap_err()
        )["reason"],
        "key_ambiguous"
    );
    for invalid in [
        json!({"kind":"head", "name":"forbidden", "content":"{}"}),
        json!({"kind":"head", "content":"not JSON"}),
        json!({"id":note["id"], "key":"changed"}),
    ] {
        let verb = if invalid.get("id").is_some() {
            "update"
        } else {
            "create"
        };
        assert!(registry.dispatch(verb, invalid).await.is_err());
    }
}

#[tokio::test]
async fn version_key_conflict_disclosure_obeys_second_list_gate() {
    for allow in [true, false] {
        let (_, _, registry) = fixture(allow);
        let args = json!({"kind":"head", "key":"private/key", "content":"{}"});
        let holder = registry.dispatch("create", args.clone()).await.unwrap();
        let conflict = details(registry.dispatch("create", args).await.unwrap_err());
        assert_eq!(conflict["reason"], "key_conflict");
        assert_eq!(conflict["key"], "private/key");
        assert_eq!(conflict.get("existing_id"), allow.then_some(&holder["id"]));
    }
}

#[tokio::test]
async fn version_keyed_unicode_prefix_is_literal_and_complete() {
    let (runtime, token, registry) = fixture(true);
    for (prefix, successor) in [
        ("p", "q"),
        ("a\u{10ffff}", "b"),
        ("b\u{d7ff}", "b\u{e000}"),
        ("literal%_", "literal%`"),
    ] {
        let expected = [
            prefix.to_string(),
            format!("{prefix}a"),
            format!("{prefix}\u{10ffff}"),
            format!("{prefix}\u{10ffff}x"),
        ];
        for key in &expected {
            seed(&runtime, &token, "head", Some(key), 10, &[]).await;
        }
        seed(&runtime, &token, "head", Some(successor), 10, &[]).await;
        let rows = walk(&registry, json!({"kind":"note", "key_prefix":prefix})).await;
        let actual: HashSet<_> = rows
            .iter()
            .map(|row| row["key"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(rows.len(), expected.len());
        assert_eq!(actual, HashSet::from(expected));
        assert!(rows.iter().all(|row| row["version"] == 1));
    }
}

#[tokio::test]
async fn version_keyed_ties_after_key_and_filtered_anchor() {
    let (runtime, token, registry) = fixture(true);
    let mut tied = [
        seed(&runtime, &token, "head", Some("same"), 10, &["job", "blue"]).await,
        seed(&runtime, &token, "observation", Some("same"), 10, &["job"]).await,
    ];
    tied.sort_by_key(|note| note.id);
    seed(&runtime, &token, "head", Some("lower"), 9, &["job", "blue"]).await;
    let rows = walk(&registry, json!({"kind":"note", "key_prefix":""})).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["id"], tied[0].id.to_string());
    assert_eq!(rows[1]["id"], tied[1].id.to_string());
    assert_eq!(
        details(
            registry
                .dispatch(
                    "list",
                    json!({"kind":"note", "key_prefix":"", "after_key":"same"})
                )
                .await
                .unwrap_err()
        )["reason"],
        "key_ambiguous"
    );
    assert_eq!(
        details(
            registry
                .dispatch(
                    "list",
                    json!({"kind":"note", "key_prefix":"", "after_key":"missing"})
                )
                .await
                .unwrap_err()
        )["reason"],
        "after_key_missing"
    );
    let outside = seed(
        &runtime,
        &token,
        "head",
        Some("outside-prefix"),
        11,
        &["lease"],
    )
    .await;
    let page = registry.dispatch("list", json!({"kind":"note", "note_kind":"head", "key_prefix":"", "tags":["job"], "after_key":outside.key})).await.unwrap();
    assert_eq!(page["notes"].as_array().unwrap().len(), 2);
    let all = walk(
        &registry,
        json!({"kind":"note", "key_prefix":"", "tags":["JOB","blue"], "tag_mode":"all"}),
    )
    .await;
    let any = walk(
        &registry,
        json!({"kind":"note", "key_prefix":"", "tags":["JOB","blue"], "tag_mode":"any"}),
    )
    .await;
    assert_eq!(all.len(), 2);
    assert_eq!(any.len(), 3);
}

#[tokio::test]
async fn version_time_filters_include_boundary_and_preserve_unkeyed_insertion_walk() {
    let (runtime, token, registry) = fixture(true);
    let first = seed(&runtime, &token, "head", Some("old-updated"), 5, &[]).await;
    let unkeyed = seed(&runtime, &token, "observation", None, 10, &[]).await;
    let newer = seed(&runtime, &token, "head", Some("new"), 10, &[]).await;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(Note {
            updated_at: 11,
            ..first.clone()
        })
        .await
        .unwrap();
    let cutoff = "1970-01-01T00:00:00.000010Z";
    let plain = walk(&registry, json!({"kind":"note", "updated_after":cutoff})).await;
    assert_eq!(
        plain
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            first.id.to_string(),
            unkeyed.id.to_string(),
            newer.id.to_string()
        ]
    );
    assert_eq!(
        walk(&registry, json!({"kind":"note", "created_after":cutoff}))
            .await
            .len(),
        2
    );
    let keyed = walk(
        &registry,
        json!({"kind":"note", "key_prefix":"", "updated_after":cutoff}),
    )
    .await;
    assert_eq!(keyed.len(), 2);
    assert_eq!(keyed[0]["id"], first.id.to_string());
    assert_eq!(keyed[0]["version"], 2);
    assert!(registry
        .dispatch("list", json!({"kind":"note", "updated_after":"not-a-time"}))
        .await
        .is_err());
}
