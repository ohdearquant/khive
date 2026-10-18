// ── `git.digest` verb (ADR-088 Amendment 1) ─────────────────────────────────

/// End-to-end over the `git.digest` verb itself (not `run_ingest` directly):
/// no `project` argument auto-creates the repo-anchor entity (reported via
/// `project_created`), a `Closes #N` commit message materializes an
/// `annotates` edge (with `ref_kind: "closes"`) from the commit to a
/// pre-existing `issue` note, a second commit's `parents[]` materializes a
/// `precedes` edge from parent to child, and both commit and issue notes get
/// the amendment's readable `name`.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_auto_creates_project_and_enriches_references() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], "Initial commit");

    let first = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "max_items": 10}),
        )
        .await
        .expect("digest ok (pass 1)");

    assert_eq!(first["done"], true, "{first}");
    assert_eq!(first["project_created"], true, "{first}");
    assert_eq!(first["commits_ingested"], 1, "{first}");
    assert_eq!(first["commits_total_in_db"], 1, "{first}");
    let project_id = first["project_id"]
        .as_str()
        .expect("project_id present")
        .to_string();
    let receipt_id = first["receipt_id"]
        .as_str()
        .expect("successful digest carries its durable receipt")
        .to_string();

    // Response-loss recovery uses the existing event query/get surface: no
    // separate receipt verb or table is required. The persisted result is the
    // exact complete response, including its own event UUID.
    let receipts = registry
        .dispatch(
            "list",
            json!({
                "kind": "event",
                "event_kind": "audit",
                "verb": "git.digest",
                "limit": 10
            }),
        )
        .await
        .expect("query digest receipts");
    let listed_receipt = list_items(&receipts)
        .iter()
        .find(|event| event["id"].as_str() == Some(receipt_id.as_str()))
        .expect("receipt is discoverable by the normal audit-event query");
    assert_eq!(listed_receipt["payload"]["result"], first);

    let recovered = registry
        .dispatch("get", json!({"id": receipt_id}))
        .await
        .expect("get receipt by id");
    assert_eq!(recovered["kind"], "event");
    assert_eq!(recovered["payload"]["result"], first);

    // Second digest call for the same local path resolves the SAME project
    // (no duplicate anchor entity).
    let second = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "max_items": 10}),
        )
        .await
        .expect("digest ok (pass 2, no new commits)");
    assert_eq!(second["project_created"], false, "{second}");
    assert_eq!(second["project_id"], project_id, "{second}");
    assert_eq!(
        second["commits_ingested"], 0,
        "no new commits since pass 1: {second}"
    );
    // Issue #1045: `commits_ingested` is the this-pass in-memory delta (0
    // here, since the one commit was already durably written in pass 1) —
    // but `commits_total_in_db` is derived fresh from the database each
    // pass, so it still truthfully reports the one commit that exists,
    // regardless of which pass's response a caller happened to receive.
    assert_eq!(
        second["commits_total_in_db"], 1,
        "DB-derived total survives even when this pass ingests nothing new: {second}"
    );

    // Pre-create an issue #42 the project already tracks (as if ingested by
    // an earlier `gh`-backed pass), then a commit whose message closes it.
    let issue_id = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 42, "title": "Some bug", "project_id": project_id},
            "annotates": [project_id],
        }),
    )
    .await;

    write(repo, "src/lib.rs", "// fix\n");
    commit(repo, &["src/lib.rs"], "Fix the bug\n\nCloses #42");
    let commit2_sha = head_sha(repo);

    let third = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "project": project_id, "max_items": 10}),
        )
        .await
        .expect("digest ok (pass 3)");
    assert_eq!(third["commits_ingested"], 1, "{third}");
    assert_eq!(
        third["commits_total_in_db"], 2,
        "two commits now durably present across all three passes: {third}"
    );
    assert_eq!(
        third["reference_edges_created"], 1,
        "the Closes #42 reference resolves: {third}"
    );
    assert_eq!(
        third["parent_edges_created"], 1,
        "the second commit's parent link to the first: {third}"
    );

    // The issue has exactly one incoming `annotates` edge, from the closing
    // commit, carrying ref_kind=closes.
    let issue_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": issue_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let hits = issue_neighbors.as_array().expect("array");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["kind"], "commit");

    // The closing commit's own record carries the readable name and its sha.
    let commit_note = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list_items(&commit_note);
    let closing = items
        .iter()
        .find(|c| c["properties"]["sha"] == commit2_sha)
        .expect("closing commit note present");
    let name = closing["name"].as_str().expect("name present");
    assert!(
        name.contains("Fix the bug"),
        "commit name must carry the subject: {name:?}"
    );

    // Parent -> child `precedes` edge: the first commit precedes the second.
    let first_commit = items
        .iter()
        .find(|c| c["properties"]["sha"] != commit2_sha)
        .expect("first commit note present");
    let first_commit_id = first_commit["id"].as_str().expect("id present");
    let precedes_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": first_commit_id, "direction": "outgoing", "relations": ["precedes"]}),
        )
        .await
        .expect("neighbors ok");
    let precedes_hits = precedes_neighbors.as_array().expect("array");
    assert_eq!(
        precedes_hits.len(),
        1,
        "the first commit precedes exactly the second: {precedes_hits:?}"
    );
    assert_eq!(
        precedes_hits[0]["id"].as_str().unwrap(),
        closing["id"].as_str().unwrap()
    );
}

/// An explicit namespace and per-request actor govern the entire digest pass,
/// including every nested canonical create/link dispatch and the final
/// receipt. They must never fall back to the warm registry's baked local
/// token/anonymous actor. The explicit namespace is not a by-ID isolation
/// boundary: under the fixture's `AllowAllGate`, ADR-007 makes `get(id=...)`
/// namespace-agnostic.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_receipt_discovery_uses_explicit_namespace_but_get_is_namespace_agnostic() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "tenant\n");
    commit(repo, &["README.md"], "Tenant receipt");
    write(repo, "README.md", "tenant second commit\n");
    commit(repo, &["README.md"], "Tenant parent edge");

    let response = registry
        .dispatch_with_identity(
            "git.digest",
            json!({
                "namespace": "receipt-test",
                "source": repo.to_str().unwrap(),
                "include": ["commits"],
                "max_items": 10
            }),
            Some(RequestIdentity {
                namespace: "identity-default-must-not-win".to_string(),
                actor_id: Some("lambda:tenant-ingester".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("non-local digest succeeds");
    let receipt_id = response["receipt_id"]
        .as_str()
        .expect("receipt id")
        .to_string();

    for kind in ["project", "commit", "edge"] {
        let local_rows = registry
            .dispatch("list", json!({"kind": kind, "limit": 100}))
            .await
            .expect("local list");
        assert!(
            list_items(&local_rows).is_empty(),
            "nested digest writes must not fall back to local for {kind}: {local_rows}"
        );
    }

    let tenant_projects = registry
        .dispatch(
            "list",
            json!({"namespace": "receipt-test", "kind": "project", "limit": 100}),
        )
        .await
        .expect("tenant project list");
    let projects = list_items(&tenant_projects);
    assert_eq!(
        projects.len(),
        1,
        "exactly one tenant project: {projects:?}"
    );
    assert_eq!(projects[0]["id"], response["project_id"]);
    assert_eq!(projects[0]["namespace"], "receipt-test");

    let tenant_commits = registry
        .dispatch(
            "list",
            json!({"namespace": "receipt-test", "kind": "commit", "limit": 100}),
        )
        .await
        .expect("tenant commit list");
    let commits = list_items(&tenant_commits);
    assert_eq!(
        commits.len(),
        2,
        "both commits stay in tenant scope: {commits:?}"
    );
    assert!(commits
        .iter()
        .all(|commit| commit["namespace"] == "receipt-test"));

    let tenant_edges = registry
        .dispatch(
            "list",
            json!({"namespace": "receipt-test", "kind": "edge", "limit": 100}),
        )
        .await
        .expect("tenant edge list");
    let edges = list_items(&tenant_edges);
    assert!(
        edges.len() >= 3,
        "two project annotations plus the parent edge must be tenant-scoped: {edges:?}"
    );
    assert!(edges.iter().all(|edge| edge["namespace"] == "receipt-test"));

    let local_receipts = registry
        .dispatch(
            "list",
            json!({
                "kind": "event",
                "event_kind": "audit",
                "verb": "git.digest",
                "limit": 100
            }),
        )
        .await
        .expect("local event list");
    assert!(
        list_items(&local_receipts)
            .iter()
            .all(|event| event["id"].as_str() != Some(receipt_id.as_str())),
        "this fixture's local-only list scope must not discover a tenant-attributed receipt"
    );

    let tenant_receipts = registry
        .dispatch(
            "list",
            json!({
                "namespace": "receipt-test",
                "kind": "event",
                "event_kind": "audit",
                "verb": "git.digest",
                "limit": 100
            }),
        )
        .await
        .expect("tenant event list");
    let listed = list_items(&tenant_receipts)
        .iter()
        .find(|event| event["id"].as_str() == Some(receipt_id.as_str()))
        .expect("tenant receipt is discoverable in its original namespace");
    assert_eq!(listed["payload"]["result"], response);
    assert_eq!(listed["actor"], "actor:lambda:tenant-ingester");

    let tenant_audits = registry
        .dispatch(
            "list",
            json!({
                "namespace": "receipt-test",
                "kind": "event",
                "event_kind": "audit",
                "limit": 100
            }),
        )
        .await
        .expect("tenant audit list");
    let nested_audits: Vec<&Value> = list_items(&tenant_audits)
        .iter()
        .filter(|event| {
            matches!(
                event["verb"].as_str(),
                Some("git.digest" | "create" | "link" | "update")
            )
        })
        .collect();
    assert!(
        nested_audits.iter().any(|event| event["verb"] == "create"),
        "the fixture must observe nested create audits: {tenant_audits}"
    );
    assert!(
        nested_audits.iter().any(|event| event["verb"] == "link"),
        "the two-commit fixture must observe a nested link audit: {tenant_audits}"
    );
    assert!(
        nested_audits
            .iter()
            .all(|event| event["actor"] == "actor:lambda:tenant-ingester"),
        "every nested gate/audit must retain the request actor: {nested_audits:?}"
    );

    let recovered = registry
        .dispatch("get", json!({"id": receipt_id}))
        .await
        .expect("AllowAllGate permits namespace-agnostic by-ID recovery");
    assert_eq!(recovered["payload"]["result"], response);
}

/// Recovery freezes one half-open timestamp window and advances `offset`
/// through every 1000-row event page. A matching receipt older than the first
/// page remains recoverable even if a newer receipt lands between queries,
/// because the later row is outside the frozen `until` boundary.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_receipt_recovery_pages_past_one_thousand_in_a_frozen_window() {
    let (rt, _token, registry) = fixture().await;
    let namespace = "receipt-pagination";
    let tenant_token = rt
        .authorize(Namespace::parse(namespace).expect("valid namespace"))
        .expect("authorize receipt namespace");
    let store = rt.events(&tenant_token).expect("tenant event store");
    let project_id = Uuid::new_v4();

    const SINCE: i64 = 9_999_999;
    const UNTIL: i64 = 10_001_001;
    const PAGE_LIMIT: usize = 1000;

    let receipt = |marker: String, created_at: i64| {
        let mut event = Event::new(
            namespace,
            "git.digest",
            EventKind::Audit,
            SubstrateKind::Event,
            "agent:receipt-recovery-test",
        )
        .with_outcome(EventOutcome::Success)
        .with_target(project_id)
        .with_payload_schema_version(2);
        event.created_at = created_at;
        event.payload = json!({
            "result": {
                "receipt_id": event.id,
                "project_id": project_id,
                "marker": marker,
                "done": true
            }
        });
        event
    };

    // The lost receipt is the oldest in-window row. Exactly 1000 newer
    // receipts force it onto the second page.
    let lost = receipt("lost".to_string(), SINCE + 1);
    let lost_id = lost.id;
    let lost_id_string = lost_id.to_string();
    let lost_result = lost.payload["result"].clone();
    let mut events = vec![lost];
    events.extend((0..PAGE_LIMIT).map(|index| {
        receipt(
            format!("decoy-{index}"),
            SINCE + 2 + i64::try_from(index).expect("index fits i64"),
        )
    }));
    let summary = store
        .append_events(events)
        .await
        .expect("seed paginated receipt window");
    assert_eq!(summary.affected, (PAGE_LIMIT + 1) as u64);

    let first_page = registry
        .dispatch(
            "list",
            json!({
                "namespace": namespace,
                "kind": "event",
                "event_kind": "audit",
                "verb": "git.digest",
                "since": SINCE,
                "until": UNTIL,
                "limit": PAGE_LIMIT,
                "offset": 0
            }),
        )
        .await
        .expect("first frozen-window page");
    let first_items = list_items(&first_page);
    assert_eq!(first_items.len(), PAGE_LIMIT);
    assert!(
        first_items
            .iter()
            .all(|event| event["id"].as_str() != Some(lost_id_string.as_str())),
        "the fixture must place the lost receipt beyond page one"
    );

    // Simulate a digest completing while recovery is paging. Reusing the
    // frozen UNTIL excludes it, preventing an offset shift or duplicate.
    store
        .append_event(receipt("late".to_string(), UNTIL + 1))
        .await
        .expect("append post-snapshot receipt");

    let second_page = registry
        .dispatch(
            "list",
            json!({
                "namespace": namespace,
                "kind": "event",
                "event_kind": "audit",
                "verb": "git.digest",
                "since": SINCE,
                "until": UNTIL,
                "limit": PAGE_LIMIT,
                "offset": PAGE_LIMIT
            }),
        )
        .await
        .expect("second frozen-window page");
    let second_items = list_items(&second_page);
    assert_eq!(second_items.len(), 1);
    assert_eq!(second_items[0]["id"], json!(lost_id));
    assert_eq!(second_items[0]["payload"]["result"], lost_result);

    let recovered = registry
        .dispatch("get", json!({"id": lost_id}))
        .await
        .expect("by-ID recovery is namespace-agnostic under AllowAllGate");
    assert_eq!(recovered["payload"]["result"], lost_result);
}
