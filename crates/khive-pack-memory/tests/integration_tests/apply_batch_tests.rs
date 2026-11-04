// Tests for the memory.apply_batch verb: validation, dry run, supersede and create
// writes, re-runs, whole-call refusals and recall after a rewrite.

/// A registry with no actor id: the anonymous caller.
fn ab_anonymous_registry(rt: KhiveRuntime) -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt),
    ));
    builder.build().expect("anonymous registry builds")
}

fn ab_uuid(value: &Value) -> Uuid {
    value
        .as_str()
        .expect("value is a string id")
        .parse::<Uuid>()
        .expect("value parses as a uuid")
}

/// Write one memory row through `memory.remember` and return its id.
async fn ab_seed(registry: &khive_runtime::VerbRegistry, content: &str) -> Uuid {
    let result = registry
        .dispatch(
            "memory.remember",
            json!({"content": content, "memory_type": "semantic"}),
        )
        .await
        .expect("memory.remember seeds a row");
    ab_uuid(&result["id"])
}

/// Count the notes stored in a namespace.
async fn ab_note_count(rt: &KhiveRuntime, namespace: &str) -> usize {
    let token = rt
        .authorize(Namespace::parse(namespace).expect("namespace parses"))
        .expect("authorize namespace");
    rt.core()
        .notes(&token)
        .expect("note store")
        .query_notes_filtered(
            namespace,
            &khive_storage::note::NoteFilter::default(),
            PageRequest {
                limit: 1000,
                offset: 0,
            },
        )
        .await
        .expect("query notes")
        .items
        .len()
}

/// Ids of the live `supersedes` edges in a namespace. An empty `targets` list
/// means every such edge; otherwise only edges that point at one of `targets`.
async fn ab_edges(rt: &KhiveRuntime, namespace: &str, targets: Vec<Uuid>) -> Vec<Uuid> {
    let token = rt
        .authorize(Namespace::parse(namespace).expect("namespace parses"))
        .expect("authorize namespace");
    let page = rt
        .graph(&token)
        .expect("graph store")
        .query_edges(
            EdgeFilter {
                target_ids: targets,
                relations: vec![EdgeRelation::Supersedes],
                ..EdgeFilter::default()
            },
            vec![],
            PageRequest {
                offset: 0,
                limit: 1000,
            },
        )
        .await
        .expect("query edges");
    page.items.iter().map(|edge| Uuid::from(edge.id)).collect()
}

async fn ab_apply(
    registry: &khive_runtime::VerbRegistry,
    batch: &str,
    rows: Vec<Value>,
    dry_run: bool,
) -> Value {
    registry
        .dispatch(
            "memory.apply_batch",
            json!({"batch": batch, "rows": rows, "dry_run": dry_run}),
        )
        .await
        .expect("memory.apply_batch accepts the call")
}

/// A whole-call refusal, as the Debug text of the error.
async fn ab_apply_err(
    registry: &khive_runtime::VerbRegistry,
    batch: &str,
    rows: Vec<Value>,
) -> String {
    let error = registry
        .dispatch(
            "memory.apply_batch",
            json!({"batch": batch, "rows": rows, "dry_run": false}),
        )
        .await
        .expect_err("the call must be refused as a whole");
    format!("{error:?}")
}

fn ab_supersede(old: Uuid, content: &str) -> Value {
    json!({
        "id": old.to_string(),
        "action": "supersede",
        "content": content,
        "properties": {"who": "batch-tester"},
    })
}

fn ab_create(part: u32, content: &str) -> Value {
    json!({
        "action": "create",
        "part": part,
        "content": content,
        "properties": {"who": "batch-tester"},
    })
}

/// Run one row as a dry run and as a real run; both must refuse it with the
/// same code and neither may store anything. Returns the code.
async fn ab_refusal_code(
    rt: &KhiveRuntime,
    registry: &khive_runtime::VerbRegistry,
    row: Value,
) -> String {
    let before = ab_note_count(rt, "alice").await;
    let dry = ab_apply(registry, "refusals", vec![row.clone()], true).await;
    let real = ab_apply(registry, "refusals", vec![row], false).await;
    assert_eq!(dry["entries"][0]["outcome"], "refused");
    assert_eq!(real["entries"][0]["outcome"], "refused");
    assert_eq!(dry["counts"]["refused"], 1);
    assert_eq!(real["counts"]["refused"], 1);
    let code = dry["entries"][0]["reason"]["code"]
        .as_str()
        .expect("a refusal carries a code")
        .to_owned();
    assert_eq!(real["entries"][0]["reason"]["code"], code.as_str());
    assert!(dry["entries"][0]["reason"]["message"].is_string());
    assert_eq!(
        ab_note_count(rt, "alice").await,
        before,
        "a refused row must store nothing"
    );
    code
}

#[tokio::test]
async fn test_apply_batch_dry_run_writes_nothing() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let old = ab_seed(&alice, "heron roosting survey original").await;
    let notes_before = ab_note_count(&rt, "alice").await;
    let edges_before = ab_edges(&rt, "alice", vec![]).await.len();

    let rows = vec![
        ab_supersede(old, "heron roosting survey revised"),
        ab_create(1, "heron roosting survey addendum"),
        json!({"action": "create", "content": "x", "bogus": true}),
    ];
    let result = ab_apply(&alice, "dry", rows.clone(), true).await;
    assert_eq!(result["dry_run"], true);
    assert_eq!(result["entries"][0]["outcome"], "planned");
    assert_eq!(result["entries"][1]["outcome"], "planned");
    assert_eq!(result["entries"][2]["outcome"], "refused");
    assert!(result["entries"][0]["new_id"].is_null());
    assert_eq!(result["counts"]["planned"], 2);
    assert_eq!(result["counts"]["refused"], 1);
    assert_eq!(result["counts"]["written"], 0);
    assert_eq!(ab_note_count(&rt, "alice").await, notes_before);
    assert_eq!(ab_edges(&rt, "alice", vec![]).await.len(), edges_before);

    // Leaving dry_run out is a dry run too.
    let default_run = alice
        .dispatch("memory.apply_batch", json!({"batch": "dry", "rows": rows}))
        .await
        .expect("call without dry_run");
    assert_eq!(default_run["dry_run"], true);
    assert_eq!(default_run["counts"]["planned"], 2);
    assert_eq!(ab_note_count(&rt, "alice").await, notes_before);
}

#[tokio::test]
async fn test_apply_batch_refusal_codes() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let bob = make_registry_with_actor(rt.clone(), "bob");
    let old = ab_seed(&alice, "egret feeding notes original").await;
    let bob_old = ab_seed(&bob, "egret feeding notes from bob").await;
    let local = rt
        .authorize(Namespace::parse("local").expect("local namespace"))
        .expect("authorize local");
    let local_old = seed_memory_note(&rt, &local, "egret shared row").await;
    let concept = alice
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "egret taxonomy"}),
        )
        .await
        .expect("create a concept entity");
    let concept_id = ab_uuid(&concept["id"]);

    let cases: Vec<(&str, Value)> = vec![
        (
            "invalid-row",
            json!({"action": "create", "content": "x", "bogus": true}),
        ),
        (
            "id-missing",
            json!({"action": "supersede", "content": "x", "properties": {"who": "t"}}),
        ),
        (
            "id-not-allowed",
            json!({
                "id": old.to_string(),
                "action": "create",
                "content": "x",
                "properties": {"who": "t"},
            }),
        ),
        ("old-unresolved", ab_supersede(Uuid::new_v4(), "x")),
        ("old-not-memory", ab_supersede(concept_id, "x")),
        ("old-namespace", ab_supersede(bob_old, "x")),
        ("content-invalid", ab_supersede(old, "")),
        (
            "salience-range",
            json!({
                "id": old.to_string(),
                "action": "supersede",
                "content": "x",
                "salience": 1.5,
                "properties": {"who": "t"},
            }),
        ),
        (
            "salience-unverified",
            json!({
                "id": old.to_string(),
                "action": "supersede",
                "content": "x",
                "salience": 0.9,
                "properties": {"who": "t"},
            }),
        ),
        (
            // No salience given: a semantic row is stored at the semantic default,
            // 0.5, which is above the unverified ceiling.
            "salience-unverified",
            json!({
                "id": old.to_string(),
                "action": "supersede",
                "content": "x",
                "memory_type": "semantic",
                "properties": {"who": "t"},
            }),
        ),
        (
            "who-missing",
            json!({"id": old.to_string(), "action": "supersede", "content": "x"}),
        ),
        ("source-ids-missing", ab_supersede(local_old, "x")),
        (
            "reserved-property",
            json!({
                "id": old.to_string(),
                "action": "supersede",
                "content": "x",
                "properties": {"who": "t", "batch": "mine"},
            }),
        ),
        (
            // memory.remember refuses a supplied supersedes; a batch row gets the
            // batch's own code for it.
            "reserved-property",
            json!({
                "id": old.to_string(),
                "action": "supersede",
                "content": "x",
                "properties": {"who": "t", "supersedes": old.to_string()},
            }),
        ),
    ];
    for (expected, row) in cases {
        let code = ab_refusal_code(&rt, &alice, row).await;
        assert_eq!(code, expected, "refusal code for case {expected}");
    }

    // Credential-shaped content: the dry run refuses it as the write would, and
    // the message names the detector without echoing the matched text.
    let fixture = "dana:apply-batch-fixture-password";
    let secret_row = ab_supersede(
        old,
        &format!("egret feeding notes https://{fixture}@example.invalid/path"),
    );
    let code = ab_refusal_code(&rt, &alice, secret_row.clone()).await;
    assert_eq!(code, "secret-detected");
    let dry = ab_apply(&alice, "refusals", vec![secret_row], true).await;
    let message = dry["entries"][0]["reason"]["message"]
        .as_str()
        .expect("a refusal carries a message");
    assert!(
        !message.contains(fixture),
        "the refusal must not echo the matched text: {message}"
    );
}

#[tokio::test]
async fn test_apply_batch_duplicate_keys_in_one_call_are_refused() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let old = ab_seed(&alice, "plover tide table original").await;
    let notes_before = ab_note_count(&rt, "alice").await;

    let rows = vec![
        ab_supersede(old, "plover tide table first"),
        ab_supersede(old, "plover tide table second"),
    ];
    let result = ab_apply(&alice, "dup", rows, false).await;
    for index in 0..2 {
        assert_eq!(result["entries"][index]["outcome"], "refused");
        assert_eq!(result["entries"][index]["reason"]["code"], "duplicate-key");
    }
    assert_eq!(result["counts"]["refused"], 2);
    assert_eq!(result["counts"]["written"], 0);
    assert_eq!(ab_note_count(&rt, "alice").await, notes_before);
}

#[tokio::test]
async fn test_apply_batch_skip_and_delete_candidate_write_nothing() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let old = ab_seed(&alice, "tern colony count original").await;
    let notes_before = ab_note_count(&rt, "alice").await;
    let edges_before = ab_edges(&rt, "alice", vec![]).await.len();

    let rows = vec![
        json!({"id": old.to_string(), "action": "skip"}),
        json!({"id": old.to_string(), "action": "delete-candidate"}),
    ];
    let result = ab_apply(&alice, "idle", rows, false).await;
    assert_eq!(result["entries"][0]["outcome"], "skipped");
    assert_eq!(result["entries"][1]["outcome"], "listed");
    assert_eq!(result["counts"]["skipped"], 1);
    assert_eq!(result["counts"]["listed"], 1);
    assert_eq!(result["counts"]["written"], 0);
    assert_eq!(ab_note_count(&rt, "alice").await, notes_before);
    assert_eq!(ab_edges(&rt, "alice", vec![]).await.len(), edges_before);
}

#[tokio::test]
async fn test_apply_batch_supersede_writes_one_row_and_one_edge() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let old = ab_seed(&alice, "kestrel nesting notes original").await;
    let old_before = alice
        .dispatch("get", json!({"id": old.to_string()}))
        .await
        .expect("get old row");
    let notes_before = ab_note_count(&rt, "alice").await;
    let edges_before = ab_edges(&rt, "alice", vec![]).await.len();

    let result = ab_apply(
        &alice,
        "supersede-one",
        vec![ab_supersede(old, "kestrel nesting notes revised")],
        false,
    )
    .await;
    assert_eq!(result["entries"][0]["outcome"], "written");
    assert_eq!(result["counts"]["written"], 1);
    let new_id = ab_uuid(&result["entries"][0]["new_id"]);

    assert_eq!(ab_note_count(&rt, "alice").await, notes_before + 1);
    assert_eq!(ab_edges(&rt, "alice", vec![]).await.len(), edges_before + 1);
    assert_eq!(ab_edges(&rt, "alice", vec![old]).await.len(), 1);

    let old_after = alice
        .dispatch("get", json!({"id": old.to_string()}))
        .await
        .expect("get old row again");
    assert_eq!(old_after, old_before, "the old row must not change");

    let new_row = alice
        .dispatch("get", json!({"id": new_id.to_string()}))
        .await
        .expect("get new row");
    assert_eq!(new_row["namespace"], "alice");
    let props = &new_row["properties"];
    assert_eq!(props["batch"], "supersede-one");
    assert_eq!(props["supersedes"], old.to_string().as_str());
    let digest = props["batch_row"].as_str().expect("batch_row is a string");
    assert_eq!(digest.len(), 64);
    assert!(digest
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}

#[tokio::test]
async fn test_apply_batch_semantic_row_without_salience_is_written_when_verified() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let old = ab_seed(&alice, "dunlin roost count original").await;
    let notes_before = ab_note_count(&rt, "alice").await;

    // The semantic default, 0.5, is above the unverified ceiling, so the row is
    // accepted only because it names what was checked.
    let row = json!({
        "id": old.to_string(),
        "action": "supersede",
        "content": "dunlin roost count revised",
        "memory_type": "semantic",
        "properties": {"who": "batch-tester", "verified": "field sheet 12"},
    });
    let dry = ab_apply(&alice, "semantic-default", vec![row.clone()], true).await;
    assert_eq!(dry["entries"][0]["outcome"], "planned");
    let real = ab_apply(&alice, "semantic-default", vec![row], false).await;
    assert_eq!(real["entries"][0]["outcome"], "written");
    assert_eq!(ab_note_count(&rt, "alice").await, notes_before + 1);
}

#[tokio::test]
async fn test_apply_batch_create_writes_a_row_and_no_edge() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let notes_before = ab_note_count(&rt, "alice").await;
    let edges_before = ab_edges(&rt, "alice", vec![]).await.len();

    let result = ab_apply(
        &alice,
        "create-one",
        vec![ab_create(1, "avocet wintering ground survey")],
        false,
    )
    .await;
    assert_eq!(result["entries"][0]["outcome"], "written");
    assert_eq!(result["counts"]["written"], 1);
    let new_id = ab_uuid(&result["entries"][0]["new_id"]);
    assert_eq!(ab_note_count(&rt, "alice").await, notes_before + 1);
    assert_eq!(ab_edges(&rt, "alice", vec![]).await.len(), edges_before);

    let new_row = alice
        .dispatch("get", json!({"id": new_id.to_string()}))
        .await
        .expect("get new row");
    assert_eq!(new_row["properties"]["batch"], "create-one");
    assert!(new_row["properties"]["supersedes"].is_null());
}

#[tokio::test]
async fn test_apply_batch_rerun_of_a_chunk_is_deduped_and_writes_nothing() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let old = ab_seed(&alice, "curlew moult record original").await;
    let rows = vec![
        ab_supersede(old, "curlew moult record revised"),
        ab_create(1, "curlew moult record addendum"),
    ];

    let first = ab_apply(&alice, "chunk", rows.clone(), false).await;
    assert_eq!(first["counts"]["written"], 2);
    let notes_after_first = ab_note_count(&rt, "alice").await;
    let edges_after_first = ab_edges(&rt, "alice", vec![]).await.len();

    let second = ab_apply(&alice, "chunk", rows, false).await;
    assert_eq!(second["counts"]["deduped"], 2);
    assert_eq!(second["counts"]["written"], 0);
    for index in 0..2 {
        assert_eq!(second["entries"][index]["outcome"], "deduped");
        assert_eq!(
            second["entries"][index]["new_id"], first["entries"][index]["new_id"],
            "a deduped row points at the row the first run wrote"
        );
    }
    assert_eq!(ab_note_count(&rt, "alice").await, notes_after_first);
    assert_eq!(
        ab_edges(&rt, "alice", vec![]).await.len(),
        edges_after_first
    );
}

#[tokio::test]
async fn test_apply_batch_changed_row_on_rerun_is_refused_dedupe_name_taken() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let old = ab_seed(&alice, "godwit passage log original").await;
    let first = ab_apply(
        &alice,
        "changed",
        vec![ab_supersede(old, "godwit passage log revised")],
        false,
    )
    .await;
    assert_eq!(first["entries"][0]["outcome"], "written");
    let notes_after_first = ab_note_count(&rt, "alice").await;

    let second = ab_apply(
        &alice,
        "changed",
        vec![ab_supersede(old, "godwit passage log revised again")],
        false,
    )
    .await;
    assert_eq!(second["entries"][0]["outcome"], "refused");
    assert_eq!(second["entries"][0]["reason"]["code"], "dedupe-name-taken");
    assert_eq!(second["counts"]["refused"], 1);
    assert_eq!(ab_note_count(&rt, "alice").await, notes_after_first);
}

#[tokio::test]
async fn test_apply_batch_deduped_row_gets_its_removed_edge_back() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let old = ab_seed(&alice, "knot flock estimate original").await;
    let rows = vec![ab_supersede(old, "knot flock estimate revised")];
    ab_apply(&alice, "relink", rows.clone(), false).await;
    let notes_after_first = ab_note_count(&rt, "alice").await;

    let edge_ids = ab_edges(&rt, "alice", vec![old]).await;
    assert_eq!(edge_ids.len(), 1);
    let token = rt
        .authorize(Namespace::parse("alice").expect("alice namespace"))
        .expect("authorize alice");
    // A hard delete leaves no edge row, the state of a run that stopped after
    // the write and before the link. A soft delete would leave a tombstone,
    // which `link` does not resurrect.
    rt.delete_edge(&token, edge_ids[0], true)
        .await
        .expect("remove the supersedes edge");
    assert_eq!(ab_edges(&rt, "alice", vec![old]).await.len(), 0);

    let rerun = ab_apply(&alice, "relink", rows, false).await;
    assert_eq!(rerun["entries"][0]["outcome"], "deduped");
    assert_eq!(rerun["counts"]["written"], 0);
    assert_eq!(ab_note_count(&rt, "alice").await, notes_after_first);
    assert_eq!(
        ab_edges(&rt, "alice", vec![old]).await.len(),
        1,
        "a deduped row asserts its supersedes edge again"
    );
}

#[tokio::test]
async fn test_apply_batch_supersedes_a_shared_row_when_sources_are_named() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let local = rt
        .authorize(Namespace::parse("local").expect("local namespace"))
        .expect("authorize local");
    let local_old = seed_memory_note(&rt, &local, "bittern call census shared original").await;
    let local_notes_before = ab_note_count(&rt, "local").await;
    let alice_notes_before = ab_note_count(&rt, "alice").await;

    let row = json!({
        "id": local_old.to_string(),
        "action": "supersede",
        "content": "bittern call census shared revised",
        "properties": {"who": "batch-tester", "source_ids": ["census-2026"]},
    });
    let result = ab_apply(&alice, "shared", vec![row], false).await;
    assert_eq!(result["entries"][0]["outcome"], "written");
    assert_eq!(result["entries"][0]["old_namespace"], "local");
    assert_eq!(ab_note_count(&rt, "alice").await, alice_notes_before + 1);
    assert_eq!(
        ab_note_count(&rt, "local").await,
        local_notes_before,
        "the shared row is not rewritten"
    );
    assert_eq!(ab_edges(&rt, "alice", vec![local_old]).await.len(), 1);
}

/// A registry for `actor_id` whose configuration declares `owned` namespaces
/// (ADR-021 Delta 1). Tokens minted by `dispatch` carry the declaration.
fn ab_registry_owning(
    rt: KhiveRuntime,
    actor_id: &str,
    owned: &[&str],
) -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor_id.to_string()));
    builder.with_owned_namespaces(
        owned
            .iter()
            .map(|ns| Namespace::parse(ns).expect("owned namespace parses"))
            .collect(),
    );
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt),
    ));
    builder.build().expect("registry with owned namespaces builds")
}

#[tokio::test]
async fn test_apply_batch_admits_an_old_row_in_an_owned_namespace() {
    let rt = make_runtime();
    let archive = make_registry_with_actor(rt.clone(), "alice:archive");
    let old = ab_seed(&archive, "kestrel migration tally archived original").await;
    let alice = ab_registry_owning(rt.clone(), "alice", &["alice:archive"]);
    let notes_before = ab_note_count(&rt, "alice").await;
    let archive_notes_before = ab_note_count(&rt, "alice:archive").await;

    let row = ab_supersede(old, "kestrel migration tally revised");
    let result = ab_apply(&alice, "owned", vec![row], true).await;
    assert_eq!(result["entries"][0]["outcome"], "planned");
    assert_eq!(result["entries"][0]["old_namespace"], "alice:archive");
    assert_eq!(result["counts"]["planned"], 1);
    assert_eq!(result["counts"]["refused"], 0);
    assert_eq!(ab_note_count(&rt, "alice").await, notes_before);
    assert_eq!(
        ab_note_count(&rt, "alice:archive").await,
        archive_notes_before,
        "a dry run changes no row in the owned namespace"
    );
}

#[tokio::test]
async fn test_apply_batch_refuses_an_owned_namespace_row_when_the_list_is_empty() {
    let rt = make_runtime();
    let archive = make_registry_with_actor(rt.clone(), "alice:archive");
    let old = ab_seed(&archive, "curlew shoreline tally archived original").await;
    let alice = make_registry_with_actor(rt.clone(), "alice");

    let row = ab_supersede(old, "curlew shoreline tally revised");
    let code = ab_refusal_code(&rt, &alice, row.clone()).await;
    assert_eq!(code, "old-namespace");
    let dry = ab_apply(&alice, "refusals", vec![row], true).await;
    let message = dry["entries"][0]["reason"]["message"]
        .as_str()
        .expect("a refusal carries a message");
    assert!(
        message.contains("alice:archive"),
        "the refusal names the namespace found: {message}"
    );
}

#[tokio::test]
async fn test_apply_batch_refuses_a_prefix_namespace_that_is_not_listed() {
    let rt = make_runtime();
    let archive = make_registry_with_actor(rt.clone(), "alice:archive");
    let scratch = make_registry_with_actor(rt.clone(), "alice:scratch");
    let listed = ab_seed(&archive, "ibis nesting count listed original").await;
    let unlisted = ab_seed(&scratch, "ibis nesting count unlisted original").await;
    let alice = ab_registry_owning(rt.clone(), "alice", &["alice:archive"]);

    // The listed namespace is admitted ...
    let admitted = ab_apply(
        &alice,
        "prefix",
        vec![ab_supersede(listed, "ibis nesting count listed revised")],
        true,
    )
    .await;
    assert_eq!(admitted["entries"][0]["outcome"], "planned");

    // ... while a namespace that shares the actor's prefix but is not listed
    // keeps the refusal it has without a declaration.
    let code = ab_refusal_code(
        &rt,
        &alice,
        ab_supersede(unlisted, "ibis nesting count unlisted revised"),
    )
    .await;
    assert_eq!(code, "old-namespace");
}

#[tokio::test]
async fn test_apply_batch_admits_an_owned_namespace_declared_by_the_request_identity() {
    let rt = make_runtime();
    let archive = make_registry_with_actor(rt.clone(), "alice:archive");
    let old = ab_seed(&archive, "heron wetland census archived original").await;
    // The registry bakes no owned list; the request identity carries it, the
    // way a daemon request does.
    let alice = make_registry_with_actor(rt.clone(), "alice");

    let result = alice
        .dispatch_with_identity(
            "memory.apply_batch",
            json!({
                "batch": "identity",
                "rows": [ab_supersede(old, "heron wetland census revised")],
                "dry_run": true,
            }),
            Some(khive_runtime::RequestIdentity {
                namespace: "alice".into(),
                actor_id: Some("alice".into()),
                owned_namespaces: vec!["alice:archive".into()],
                ..khive_runtime::RequestIdentity::default()
            }),
        )
        .await
        .expect("memory.apply_batch accepts the call");
    assert_eq!(result["entries"][0]["outcome"], "planned");
    assert_eq!(result["entries"][0]["old_namespace"], "alice:archive");
}

#[tokio::test]
async fn test_apply_batch_refuses_an_anonymous_caller() {
    let rt = make_runtime();
    let anonymous = ab_anonymous_registry(rt.clone());
    let local_before = ab_note_count(&rt, "local").await;

    let text = ab_apply_err(&anonymous, "anon", vec![ab_create(1, "anonymous write")]).await;
    assert!(text.contains("InvalidInput"), "unexpected error: {text}");
    assert_eq!(ab_note_count(&rt, "local").await, local_before);
}

#[tokio::test]
async fn test_apply_batch_row_count_bounds() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let notes_before = ab_note_count(&rt, "alice").await;

    let rows = |count: u32| -> Vec<Value> {
        (1..=count)
            .map(|part| ab_create(part, &format!("bound row {part}")))
            .collect()
    };

    let too_many = ab_apply_err(&alice, "bounds", rows(201)).await;
    assert!(
        too_many.contains("InvalidInput"),
        "unexpected error: {too_many}"
    );
    let none = ab_apply_err(&alice, "bounds", Vec::new()).await;
    assert!(none.contains("InvalidInput"), "unexpected error: {none}");
    assert_eq!(ab_note_count(&rt, "alice").await, notes_before);

    let at_limit = ab_apply(&alice, "bounds", rows(200), true).await;
    assert_eq!(at_limit["counts"]["planned"], 200);
    assert_eq!(ab_note_count(&rt, "alice").await, notes_before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_apply_batch_rewrite_is_recalled_and_the_old_row_is_not() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let old = ab_seed(&alice, "zebraquartz marker original wording").await;

    let result = ab_apply(
        &alice,
        "recall",
        vec![ab_supersede(old, "zebraquartz marker replacement wording")],
        false,
    )
    .await;
    let new_id = ab_uuid(&result["entries"][0]["new_id"]);

    let recalled = alice
        .dispatch(
            "memory.recall",
            json!({
                "query": "zebraquartz",
                "limit": 10,
                "fusion_strategy": "keyword_only",
                "config": {"scoring": {
                    "enable_supersedes_suppression": true,
                    "mmr_penalty": 0.0,
                }},
            }),
        )
        .await
        .expect("memory.recall");
    let hits = recalled
        .as_array()
        .cloned()
        .or_else(|| recalled["hits"].as_array().cloned())
        .expect("recall returns a list of hits");
    let ids: Vec<String> = hits
        .iter()
        .filter_map(|hit| hit["id"].as_str().map(str::to_owned))
        .collect();
    assert!(
        ids.contains(&new_id.to_string()),
        "the new row must be recalled, got {ids:?}"
    );
    assert!(
        !ids.contains(&old.to_string()),
        "the superseded row must not be recalled, got {ids:?}"
    );
}

#[tokio::test]
async fn test_apply_batch_pins_row_rerun_and_edge_to_one_write_namespace() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let alice = ab_registry_owning(rt.clone(), "alice", &[]);
    let old = ab_seed(&alice, "sanderling shore survey original").await;
    let alice_notes_before = ab_note_count(&rt, "alice").await;
    let local_notes_before = ab_note_count(&rt, "local").await;
    let rows = vec![ab_supersede(old, "sanderling shore survey revised")];

    let first = ab_apply(&alice, "one-write-namespace", rows.clone(), false).await;
    assert_eq!(first["entries"][0]["outcome"], "written");
    assert_eq!(first["entries"][0]["old_namespace"], "alice");
    assert_eq!(first["counts"]["written"], 1);
    let new_id = ab_uuid(&first["entries"][0]["new_id"]);
    let stored = alice
        .dispatch("get", json!({"id": new_id.to_string()}))
        .await
        .expect("get the batch-written row");
    assert_eq!(stored["namespace"], "alice");
    assert_eq!(ab_note_count(&rt, "alice").await, alice_notes_before + 1);
    assert_eq!(ab_note_count(&rt, "local").await, local_notes_before);
    assert_eq!(ab_edges(&rt, "alice", vec![old]).await.len(), 1);
    assert_eq!(ab_edges(&rt, "local", vec![old]).await.len(), 0);

    // Reuse the public request rather than rebuilding its generated key.
    let rerun = ab_apply(&alice, "one-write-namespace", rows, false).await;
    assert_eq!(rerun["entries"][0]["outcome"], "deduped");
    assert_eq!(rerun["entries"][0]["new_id"], new_id.to_string());
    assert_eq!(rerun["counts"]["deduped"], 1);
    assert_eq!(rerun["counts"]["written"], 0);
    let stored_again = alice
        .dispatch("get", json!({"id": new_id.to_string()}))
        .await
        .expect("get the deduped row");
    assert_eq!(stored_again["namespace"], "alice");
    assert_eq!(ab_note_count(&rt, "alice").await, alice_notes_before + 1);
    assert_eq!(ab_note_count(&rt, "local").await, local_notes_before);
    assert_eq!(ab_edges(&rt, "alice", vec![old]).await.len(), 1);
    assert_eq!(ab_edges(&rt, "local", vec![old]).await.len(), 0);
}
