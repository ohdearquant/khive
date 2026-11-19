const ISSUE_558_PRIVATE_MARKER: &str = "private payload marker marigold";
const ISSUE_558_CONTENT: &str = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";

async fn issue_558_rows(runtime: &KhiveRuntime, sql: &str) -> Vec<khive_storage::SqlRow> {
    runtime
        .sql()
        .reader()
        .await
        .expect("snapshot reader")
        .query_all(SqlStatement {
            sql: sql.into(),
            params: vec![],
            label: Some("test.issue_558.snapshot".into()),
        })
        .await
        .expect("snapshot query")
}

async fn issue_558_snapshot(runtime: &KhiveRuntime) -> Value {
    let mut state = serde_json::Map::new();
    for table in [
        "knowledge_atoms",
        "knowledge_domains",
        "entities",
        "notes",
        "graph_edges",
    ] {
        let rows = issue_558_rows(runtime, &format!("SELECT * FROM {table} ORDER BY id")).await;
        state.insert(
            table.into(),
            serde_json::to_value(rows).expect("serialize rows"),
        );
    }
    let events = issue_558_rows(
        runtime,
        "SELECT * FROM events WHERE kind IN ('entity_merged', 'note_merged') ORDER BY id",
    )
    .await;
    state.insert(
        "merge_events".into(),
        serde_json::to_value(events).expect("serialize events"),
    );
    Value::Object(state)
}

async fn issue_558_private(runtime: &KhiveRuntime, f: &Fixture, kind: &str) -> String {
    let slug = format!("issue-558-{kind}");
    let verb = if kind == "atom" {
        "knowledge.upsert_atoms"
    } else {
        "knowledge.upsert_domains"
    };
    let args = if kind == "atom" {
        json!({"atoms": [{"slug": slug, "name": ISSUE_558_PRIVATE_MARKER,
            "content": format!("{ISSUE_558_CONTENT} {ISSUE_558_PRIVATE_MARKER}"),
            "tags": ["test", "resolver"], "properties": {"marker": ISSUE_558_PRIVATE_MARKER}}]})
    } else {
        json!({"domains": [{"slug": slug, "name": ISSUE_558_PRIVATE_MARKER,
            "description": format!("{ISSUE_558_CONTENT} {ISSUE_558_PRIVATE_MARKER}"),
            "members": ["rag", "dense-retrieval"], "tags": ["test", "resolver"]}]})
    };
    let created = f
        .dispatch(verb, args)
        .await
        .expect("create private fixture");
    assert_eq!(created["created"], 1, "{kind}: {created}");
    let by_slug = f
        .dispatch("knowledge.get", json!({"id": slug}))
        .await
        .expect("private slug get");
    let id = by_slug["id"].as_str().expect("private UUID").to_string();
    let by_id = f
        .dispatch("get", json!({"id": id}))
        .await
        .expect("real registry resolver positive control");
    assert_eq!(
        by_id, by_slug,
        "{kind}: by-ID resolver must preserve public payload"
    );
    assert_eq!(by_id["kind"], kind);
    if kind == "domain" {
        for table in ["knowledge_domains", "knowledge_atoms"] {
            let rows = issue_558_rows(
                runtime,
                &format!("SELECT * FROM {table} WHERE id = '{id}' AND deleted_at IS NULL"),
            )
            .await;
            assert_eq!(
                rows.len(),
                1,
                "domain must have its authoritative row and mirror"
            );
        }
    }
    id
}

async fn issue_558_ordinary(f: &Fixture, kind: &str, side: &str) -> String {
    let mut args = json!({"kind": kind, "name": "Merge counterpart",
        "properties": {side: side}, "skip_dedup_check": true});
    let field = if kind == "observation" {
        "content"
    } else {
        "description"
    };
    args[field] = json!(format!("{ISSUE_558_CONTENT} {side}"));
    let created = f
        .dispatch("create", args)
        .await
        .expect("create ordinary fixture");
    created["id"].as_str().expect("ordinary UUID").to_string()
}

async fn issue_558_edge(f: &Fixture, source: &str, target: &str, note: bool) {
    f.dispatch(
        "link",
        json!({"source_id": source, "target_id": target,
        "relation": if note { "annotates" } else { "contains" }, "weight": 0.75,
        "metadata": {"fixture": "issue-558"}}),
    )
    .await
    .expect("create incident edge");
}

fn issue_558_args(into: &str, from: &str, kind: Option<&str>) -> Value {
    let mut args = json!({"into_id": into, "from_id": from});
    if let Some(kind) = kind {
        args["kind"] = json!(kind);
    }
    args
}

async fn issue_558_unchanged_error(
    runtime: &KhiveRuntime,
    f: &Fixture,
    args: Value,
    arm: &str,
) -> RuntimeError {
    let before = issue_558_snapshot(runtime).await;
    let result = f.dispatch("merge", args).await;
    assert_eq!(
        issue_558_snapshot(runtime).await,
        before,
        "{arm}: mutation on refusal"
    );
    result.expect_err(arm)
}

fn issue_558_directing_error(error: RuntimeError, arm: &str) {
    let RuntimeError::InvalidInput(message) = error else {
        panic!("{arm}: expected directing InvalidInput, got {error:?}");
    };
    for required in [
        "merge",
        "not supported",
        "the knowledge pack's own verbs",
        "knowledge.upsert_atoms",
        "knowledge.upsert_domains",
        "knowledge.edit",
    ] {
        assert!(
            message.contains(required),
            "{arm}: missing {required:?} in {message:?}"
        );
    }
    assert!(
        !message.contains(ISSUE_558_PRIVATE_MARKER),
        "{arm}: private payload leaked"
    );
}

fn issue_558_not_found(error: RuntimeError, expected: &str, arm: &str) {
    let RuntimeError::NotFound(message) = error else {
        panic!("{arm}: expected original NotFound, got {error:?}");
    };
    assert_eq!(message, expected, "{arm}");
}

async fn issue_558_private_routes(kinds: &[Option<&str>], note: bool) {
    let runtime = rt();
    let f = pack_via_registry(runtime.clone());
    let ordinary =
        issue_558_ordinary(&f, if note { "observation" } else { "concept" }, "into").await;
    let anchor = issue_558_ordinary(&f, "concept", "anchor").await;
    issue_558_edge(&f, &ordinary, &anchor, note).await;
    for private_kind in ["atom", "domain"] {
        let private = issue_558_private(&runtime, &f, private_kind).await;
        for &kind in kinds {
            for private_into in [true, false] {
                let arm = format!("{private_kind}/{kind:?}/private_into={private_into}");
                let (into, from) = if private_into {
                    (&private, &ordinary)
                } else {
                    (&ordinary, &private)
                };
                let error =
                    issue_558_unchanged_error(&runtime, &f, issue_558_args(into, from, kind), &arm)
                        .await;
                issue_558_directing_error(error, &arm);
            }
        }
    }
}

#[tokio::test]
async fn issue_558_merge_private_entity_operands_refused() {
    issue_558_private_routes(&[None, Some("entity"), Some("concept")], false).await;
}

#[tokio::test]
async fn issue_558_merge_private_note_operands_refused() {
    issue_558_private_routes(&[Some("note"), Some("observation")], true).await;
}

#[tokio::test]
async fn issue_558_merge_private_flags_and_aliases_refused() {
    let runtime = rt();
    let f = pack_via_registry(runtime.clone());
    let atom = issue_558_private(&runtime, &f, "atom").await;
    let domain = issue_558_private(&runtime, &f, "domain").await;
    let ordinary = issue_558_ordinary(&f, "concept", "into").await;
    let anchor = issue_558_ordinary(&f, "concept", "anchor").await;
    issue_558_edge(&f, &ordinary, &anchor, false).await;
    for (into, from) in [
        (&atom, &ordinary),
        (&ordinary, &atom),
        (&domain, &ordinary),
        (&ordinary, &domain),
        (&atom, &domain),
        (&domain, &atom),
    ] {
        for (into_key, from_key) in [
            ("into_id", "from_id"),
            ("winner_id", "loser_id"),
            ("target_id", "source_id"),
        ] {
            for force in [false, true] {
                for dry_run in [false, true] {
                    let arm = format!("{into}/{from}/{into_key}/{force}/{dry_run}");
                    let args =
                        json!({into_key: into, from_key: from, "force": force, "dry_run": dry_run});
                    let error = issue_558_unchanged_error(&runtime, &f, args, &arm).await;
                    issue_558_directing_error(error, &arm);
                }
            }
        }
    }
}
