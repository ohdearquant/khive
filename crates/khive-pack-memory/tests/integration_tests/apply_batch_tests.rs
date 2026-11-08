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

async fn ab_source_namespace_edges(
    rt: &KhiveRuntime,
    namespace: &str,
    targets: Vec<Uuid>,
    relation: EdgeRelation,
) -> Vec<khive_storage::types::Edge> {
    let token = rt
        .authorize(Namespace::parse(namespace).expect("namespace parses"))
        .expect("authorize namespace");
    let edges = rt
        .graph(&token)
        .expect("graph store")
        .query_edges(
            EdgeFilter {
                target_ids: targets,
                relations: vec![relation],
                ..EdgeFilter::default()
            },
            vec![],
            PageRequest {
                offset: 0,
                limit: 1000,
            },
        )
        .await
        .expect("query stored edges")
        .items;
    assert!(
        edges.iter().all(|edge| edge.namespace == namespace),
        "d1_fixture_namespace_scope"
    );
    edges
}

#[tokio::test]
async fn test_apply_batch_and_kg_link_share_source_namespace_then_refuse_mismatch() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let old = ab_seed(&alice, "source namespace crosswriter original").await;
    let result = ab_apply(
        &alice,
        "source-namespace-crosswriter",
        vec![ab_supersede(old, "source namespace crosswriter revised")],
        false,
    )
    .await;
    assert_eq!(result["entries"][0]["outcome"], "written");
    let source_id = ab_uuid(&result["entries"][0]["new_id"]);
    let source = alice
        .dispatch("get", json!({"id": source_id.to_string()}))
        .await
        .expect("get actual source memory row");
    assert_eq!(source["namespace"], "alice");
    let memory_edges =
        ab_source_namespace_edges(&rt, "alice", vec![old], EdgeRelation::Supersedes).await;
    assert_eq!(memory_edges.len(), 1, "apply_batch_source_namespace_edge");
    assert_eq!(memory_edges[0].namespace, "alice");
    assert_eq!(memory_edges[0].source_id, source_id);
    assert_eq!(memory_edges[0].target_id, old);

    alice
        .dispatch(
            "link",
            json!({
                "source_id": source_id,
                "target_id": old,
                "relation": "supersedes",
                "weight": 1.0,
                "namespace": "alice"
            }),
        )
        .await
        .expect("KG link accepts its source write namespace");
    let linked =
        ab_source_namespace_edges(&rt, "alice", vec![old], EdgeRelation::Supersedes).await;
    assert_eq!(linked.len(), 1, "crosswriters_share_source_namespace");
    assert_eq!(linked[0].namespace, "alice");
    assert_eq!(
        Uuid::from(linked[0].id),
        Uuid::from(memory_edges[0].id),
        "crosswriter_natural_key_keeps_persisted_id"
    );

    let alice_before = ab_edges(&rt, "alice", vec![old]).await.len();
    let local_before = ab_edges(&rt, "local", vec![old]).await.len();
    assert_eq!(local_before, 0, "same_source_positive_has_no_local_edge");
    let mismatch = alice
        .dispatch(
            "link",
            json!({"source_id": source_id, "target_id": old, "relation": "supersedes", "weight": 1.0}),
        )
        .await;
    let alice_after = ab_edges(&rt, "alice", vec![old]).await.len();
    let local_after = ab_edges(&rt, "local", vec![old]).await.len();
    assert!(
        mismatch.is_err(),
        "memory_source_namespace_mismatch_refused"
    );
    let error = format!("{:?}", mismatch.expect_err("mismatched namespace is refused"));
    assert!(error.contains("link") && error.contains("namespace=alice"));
    assert_eq!(alice_after, alice_before, "mismatch_keeps_source_edge_count");
    assert_eq!(local_after, local_before, "mismatch_writes_no_local_edge");
}

#[tokio::test]
async fn test_apply_batch_nonlocal_source_accepts_local_superseded_target() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let local = make_registry_with_actor(rt.clone(), "local");
    let old = ab_seed(&local, "local target for nonlocal source").await;
    let old_row = local
        .dispatch("get", json!({"id": old.to_string()}))
        .await
        .expect("get local target");
    assert_eq!(old_row["namespace"], "local");
    let alice_before = ab_edges(&rt, "alice", vec![old]).await.len();
    let local_before = ab_edges(&rt, "local", vec![old]).await.len();
    let mut row = ab_supersede(old, "nonlocal source replacing local target");
    row["properties"]["source_ids"] = json!(["census-2026"]);
    let result = ab_apply(
        &alice,
        "nonlocal-source-local-target",
        vec![row],
        false,
    )
    .await;
    assert_eq!(
        result["entries"][0]["outcome"],
        "written",
        "nonlocal source/shared-local target entry: {}",
        result["entries"][0]
    );
    let source_id = ab_uuid(&result["entries"][0]["new_id"]);
    let source = alice
        .dispatch("get", json!({"id": source_id.to_string()}))
        .await
        .expect("get new nonlocal source");
    assert_eq!(source["namespace"], "alice");
    let edges =
        ab_source_namespace_edges(&rt, "alice", vec![old], EdgeRelation::Supersedes).await;
    assert_eq!(edges.len(), alice_before + 1);
    assert!(edges.iter().any(|edge| {
        edge.source_id == source_id && edge.target_id == old && edge.namespace == "alice"
    }));
    assert_eq!(ab_edges(&rt, "local", vec![old]).await.len(), local_before);
}

#[tokio::test]
async fn test_memory_link_many_mixed_source_namespaces_refuses_before_any_edge() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let bob = make_registry_with_actor(rt.clone(), "bob");
    let local = make_registry_with_actor(rt.clone(), "local");
    let source_alice = ab_seed(&alice, "mixed bulk source alice").await;
    let source_bob = ab_seed(&bob, "mixed bulk source bob").await;
    let target = ab_seed(&local, "mixed bulk local target").await;
    let token = rt
        .authorize_with_visibility(
            Namespace::parse("alice").expect("alice namespace"),
            vec![Namespace::parse("bob").expect("bob namespace"), Namespace::local()],
        )
        .expect("read visibility is not another write grant");
    let before = [
        ab_edges(&rt, "alice", vec![target]).await.len(),
        ab_edges(&rt, "bob", vec![target]).await.len(),
        ab_edges(&rt, "local", vec![target]).await.len(),
    ];
    assert_eq!(before, [0, 0, 0], "mixed_bulk_fixture_starts_without_edges");
    let specs = [source_alice, source_bob]
        .into_iter()
        .map(|source_id| khive_runtime::LinkSpec {
            namespace: None,
            source_id,
            target_id: target,
            relation: EdgeRelation::Supersedes,
            weight: 1.0,
            metadata: None,
            resurrect: false,
        })
        .collect();
    let result = rt.link_many(&token, specs).await;
    let after = [
        ab_edges(&rt, "alice", vec![target]).await.len(),
        ab_edges(&rt, "bob", vec![target]).await.len(),
        ab_edges(&rt, "local", vec![target]).await.len(),
    ];
    assert!(result.is_err(), "mixed_memory_namespace_bulk_refused");
    assert_eq!(after, before, "mixed_bulk_has_no_partial_edge_write");
}

#[tokio::test]
async fn test_entity_link_keeps_token_namespace_for_non_memory_endpoints() {
    let rt = make_runtime();
    let registry = ab_anonymous_registry(rt.clone());
    let entity_token = rt
        .authorize(Namespace::parse("alice").expect("alice namespace"))
        .expect("entity seed namespace");
    let mut ids = Vec::new();
    for name in ["nonmemory source", "nonmemory target"] {
        let id = Uuid::new_v4();
        let mut entity = khive_storage::Entity::new("alice", "concept", name);
        entity.id = id;
        rt.entities(&entity_token)
            .expect("entity store")
            .upsert_entity(entity)
            .await
            .expect("seed entity outside the edge write namespace");
        ids.push(id);
    }
    registry
        .dispatch(
            "link",
            json!({"source_id": ids[0], "target_id": ids[1], "relation": "supports", "weight": 1.0}),
        )
        .await
        .expect("non-memory rule remains token namespace");
    let local =
        ab_source_namespace_edges(&rt, "local", vec![ids[1]], EdgeRelation::Supports).await;
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].namespace, "local");
    assert!(ab_source_namespace_edges(&rt, "alice", vec![ids[1]], EdgeRelation::Supports)
        .await
        .is_empty());
}

#[tokio::test]
async fn test_local_memory_none_stamp_refuses_mismatch_remote_some_out_of_scope() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let local = make_registry_with_actor(rt.clone(), "local");
    let source = ab_seed(&alice, "direct None memory source").await;
    let target = ab_seed(&local, "direct None local target").await;
    let source_token = rt
        .authorize(Namespace::parse("alice").expect("alice namespace"))
        .expect("source write token");
    let accepted = rt
        .link_with_target_backend_observed(
            &source_token,
            source,
            target,
            khive_runtime::EdgeEndpointKind::Note,
            khive_runtime::EdgeEndpointKind::Note,
            EdgeRelation::Supersedes,
            1.0,
            None,
            None,
            false,
        )
        .await
        .expect("direct same-backend None accepts matching memory source namespace");
    assert_eq!(accepted.edge.namespace, "alice");
    let before = [
        ab_edges(&rt, "alice", vec![target]).await.len(),
        ab_edges(&rt, "local", vec![target]).await.len(),
    ];
    assert_eq!(before, [1, 0], "direct_none_positive_has_only_source_edge");
    let wrong_token = rt
        .authorize_with_visibility(
            Namespace::local(),
            vec![Namespace::parse("alice").expect("alice namespace")],
        )
        .expect("source read visibility grants no source write authority");
    let mismatch = rt
        .link_with_target_backend_observed(
            &wrong_token,
            source,
            target,
            khive_runtime::EdgeEndpointKind::Note,
            khive_runtime::EdgeEndpointKind::Note,
            EdgeRelation::Supersedes,
            1.0,
            None,
            None,
            false,
        )
        .await;
    let after = [
        ab_edges(&rt, "alice", vec![target]).await.len(),
        ab_edges(&rt, "local", vec![target]).await.len(),
    ];
    assert!(
        mismatch.is_err(),
        "direct_none_memory_namespace_mismatch_refused"
    );
    let error = format!("{:?}", mismatch.expect_err("direct None mismatch refused"));
    assert!(error.contains("link") && error.contains("namespace=alice"));
    assert_eq!(after, before, "direct_none_mismatch_writes_no_edge");
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

fn ab_payload_row(old: Uuid, part: u32, content: &str) -> Value {
    let mut row = ab_supersede(old, content);
    row["part"] = json!(part);
    row["memory_type"] = json!("episodic");
    row["salience"] = json!(0.3);
    row["properties"]["source_ids"] = json!(["reed-warbler-field-sheet"]);
    row["tags"] = json!(["wetland", "field"]);
    row
}

async fn ab_payload_apply(
    registry: &khive_runtime::VerbRegistry, batch: &str, rows: Vec<Value>, dry_run: bool,
) -> Value {
    let count = rows.len();
    let result = ab_apply(registry, batch, rows, dry_run).await;
    assert_eq!(result["entries"].as_array().expect("entries array").len(), count);
    assert_eq!(result["counts"]["failed"], 0, "a storage/link failure is not a grouping result");
    result
}

fn ab_payload_id(result: &Value, index: usize) -> Uuid {
    ab_uuid(&result["entries"][index]["new_id"])
}

fn ab_payload_duplicate(result: &Value, index: usize, winner_index: usize, winner_part: u32) {
    let entry = &result["entries"][index];
    assert_eq!(entry["reason"]["code"], "duplicate-payload", "later matching New row must be refused");
    assert_eq!(entry["outcome"], "refused");
    assert!(entry["new_id"].is_null(), "refusal must not expose a second successor");
    let message = entry["reason"]["message"].as_str().expect("duplicate reason message");
    let words: Vec<&str> = message.split_whitespace()
        .map(|word| word.trim_matches(|character: char| !character.is_ascii_alphanumeric()))
        .collect();
    let index_text = winner_index.to_string();
    let part_text = winner_part.to_string();
    assert!(words.windows(2).any(|pair| pair == ["index", index_text.as_str()]),
        "winner index is zero-based in the whole input: {message}");
    assert!(words.windows(2).any(|pair| pair == ["part", part_text.as_str()]),
        "winner part is the actual row's part: {message}");
}

fn ab_payload_targets(rows: &[Value]) -> Vec<Uuid> {
    let mut targets = Vec::new();
    for row in rows {
        if row["action"] == "supersede" {
            let id = ab_uuid(&row["id"]);
            if !targets.contains(&id) { targets.push(id); }
        }
    }
    targets
}

async fn ab_payload_counts(rt: &KhiveRuntime, targets: &[Uuid]) -> (usize, usize) {
    (ab_note_count(rt, "alice").await, ab_edges(rt, "alice", targets.to_vec()).await.len())
}

async fn ab_payload_stored(
    registry: &khive_runtime::VerbRegistry, batch: &str, old: Uuid, id: Uuid,
) -> Value {
    let stored = registry.dispatch("get", json!({"id":id.to_string()}))
        .await.expect("get the actual stored successor");
    assert_eq!(stored["namespace"], "alice");
    assert_eq!(stored["properties"]["batch"], batch);
    assert_eq!(stored["properties"]["supersedes"], old.to_string());
    let digest = stored["properties"]["batch_row"].as_str().expect("stored batch_row digest");
    assert_eq!(digest.len(), 64);
    assert!(digest.chars().all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase()));
    stored
}

async fn ab_payload_two_successors(
    rt: &KhiveRuntime, registry: &khive_runtime::VerbRegistry, batch: &str, rows: Vec<Value>,
) -> [Uuid; 2] {
    assert_eq!(rows.len(), 2);
    let targets = ab_payload_targets(&rows);
    let before = ab_payload_counts(rt, &targets).await;
    let dry = ab_payload_apply(registry, batch, rows.clone(), true).await;
    assert_eq!(dry["counts"]["planned"], 2, "different complete payloads stay independently planned");
    assert_eq!(dry["counts"]["refused"], 0);
    assert_eq!(ab_payload_counts(rt, &targets).await, before, "positive dry run writes nothing");
    let result = ab_payload_apply(registry, batch, rows.clone(), false).await;
    assert_eq!(result["counts"]["written"], 2, "different payloads positively write two successors");
    assert_eq!(result["counts"]["refused"], 0);
    let ids = [ab_payload_id(&result, 0), ab_payload_id(&result, 1)];
    assert_ne!(ids[0], ids[1], "the two successful rows name distinct stored notes");
    assert_eq!(ab_payload_counts(rt, &targets).await, (before.0 + 2, before.1 + 2),
        "positive fixture stores two notes and two supersedes edges");
    for index in 0..2 {
        assert_eq!(result["entries"][index]["outcome"], "written");
        ab_payload_stored(registry, batch, ab_uuid(&rows[index]["id"]), ids[index]).await;
    }
    ids
}

async fn ab_payload_fixture(label: &str) -> (KhiveRuntime, khive_runtime::VerbRegistry, Uuid) {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let old = ab_seed(&alice, &format!("reed warbler original observation {label}")).await;
    let stored = alice.dispatch("get", json!({"id":old.to_string()}))
        .await.expect("resolve the real named-actor seed before grouping assertions");
    assert_eq!(stored["namespace"], "alice");
    ab_payload_two_successors(&rt, &alice, &format!("payload-positive-{label}"), vec![
        ab_payload_row(old, 1, "reed warbler positive first revision"),
        ab_payload_row(old, 2, "reed warbler positive different revision"),
    ]).await;
    (rt, alice, old)
}

async fn ab_payload_duplicate_group(
    rt: &KhiveRuntime, registry: &khive_runtime::VerbRegistry, batch: &str,
    old: Uuid, rows: Vec<Value>, winner: usize, loser: usize,
) {
    let winner_part = u32::try_from(rows[winner]["part"].as_u64().expect("winner part")).expect("u32 part");
    let before = ab_payload_counts(rt, &[old]).await;
    let dry = ab_payload_apply(registry, batch, rows.clone(), true).await;
    ab_payload_duplicate(&dry, loser, winner, winner_part);
    assert_eq!(dry["entries"][winner]["outcome"], "planned");
    assert_eq!(dry["counts"]["planned"], 1);
    assert_eq!(dry["counts"]["refused"], 1);
    assert_eq!(ab_payload_counts(rt, &[old]).await, before, "duplicate dry run writes nothing");
    let result = ab_payload_apply(registry, batch, rows.clone(), false).await;
    ab_payload_duplicate(&result, loser, winner, winner_part);
    assert_eq!(result["entries"][winner]["outcome"], "written");
    assert_eq!(result["counts"]["written"], 1);
    assert_eq!(result["counts"]["refused"], 1);
    assert_eq!(ab_payload_counts(rt, &[old]).await, (before.0 + 1, before.1 + 1),
        "duplicate refusal must leave no second successor memory or supersedes edge");
    for index in 0..rows.len() {
        if index != winner && index != loser {
            assert_eq!(rows[index]["action"], "skip");
            assert_eq!(result["entries"][index]["outcome"], "skipped");
        }
    }
    let new_id = ab_payload_id(&result, winner);
    ab_payload_stored(registry, batch, old, new_id).await;
    let replay = ab_payload_apply(registry, batch, vec![rows[winner].clone()], false).await;
    assert_eq!(replay["entries"][0]["outcome"], "deduped", "single-key replay remains unchanged");
    assert_eq!(ab_payload_id(&replay, 0), new_id);
    assert_eq!(replay["counts"]["written"], 0);
    assert_eq!(ab_payload_counts(rt, &[old]).await, (before.0 + 1, before.1 + 1));
}

#[tokio::test]
async fn test_apply_batch_duplicate_payload_uses_first_input_and_global_index() {
    let (rt, alice, old) = ab_payload_fixture("first-input").await;
    let first = ab_payload_row(old, 2, "reed warbler census revised");
    let second = ab_payload_row(old, 1, "reed warbler census revised");
    ab_payload_duplicate_group(&rt, &alice, "payload-first", old,
        vec![first.clone(), second.clone()], 0, 1).await;
    ab_payload_duplicate_group(&rt, &alice, "payload-global", old,
        vec![json!({"id":old.to_string(), "action":"skip"}), first, second], 1, 2).await;
}

#[tokio::test]
async fn test_apply_batch_duplicate_payload_preserves_historical_replay_in_either_position() {
    let (rt, alice, old) = ab_payload_fixture("historical").await;
    for (batch, new_first) in [("payload-held-first", false), ("payload-held-last", true)] {
        let held = ab_payload_row(old, 2, "reed warbler historical successor");
        let new = ab_payload_row(old, 1, "reed warbler historical successor");
        let before_seed = ab_payload_counts(&rt, &[old]).await;
        let seeded = ab_payload_apply(&alice, batch, vec![held.clone()], false).await;
        assert_eq!(seeded["entries"][0]["outcome"], "written", "historical row is a successful prior write");
        assert_eq!(ab_payload_counts(&rt, &[old]).await, (before_seed.0 + 1, before_seed.1 + 1));
        let held_id = ab_payload_id(&seeded, 0);
        ab_payload_stored(&alice, batch, old, held_id).await;
        let rows = if new_first { vec![new, held.clone()] } else { vec![held.clone(), new] };
        let held_index = usize::from(new_first);
        let new_index = 1 - held_index;
        let before = ab_payload_counts(&rt, &[old]).await;
        for dry_run in [true, false] {
            let result = ab_payload_apply(&alice, batch, rows.clone(), dry_run).await;
            ab_payload_duplicate(&result, new_index, held_index, 2);
            assert_eq!(result["entries"][held_index]["outcome"], "deduped");
            assert_eq!(ab_payload_id(&result, held_index), held_id);
            assert_eq!(result["counts"]["deduped"], 1);
            assert_eq!(result["counts"]["written"], 0);
            assert_eq!(result["counts"]["refused"], 1);
            assert_eq!(ab_payload_counts(&rt, &[old]).await, before,
                "historical replay dominates matching New regardless of array position");
        }
        let replay = ab_payload_apply(&alice, batch, vec![held], false).await;
        assert_eq!(replay["entries"][0]["outcome"], "deduped");
        assert_eq!(ab_payload_id(&replay, 0), held_id);
        assert_eq!(ab_payload_counts(&rt, &[old]).await, before);
    }
}

#[tokio::test]
async fn test_apply_batch_duplicate_payload_preserves_two_existing_replays() {
    let (rt, alice, old) = ab_payload_fixture("both-held").await;
    let batch = "payload-two-held";
    let first = ab_payload_row(old, 1, "reed warbler prior independently written successor");
    let second = ab_payload_row(old, 2, "reed warbler prior independently written successor");
    let before = ab_payload_counts(&rt, &[old]).await;
    let one = ab_payload_apply(&alice, batch, vec![first.clone()], false).await;
    assert_eq!(one["entries"][0]["outcome"], "written");
    assert_eq!(ab_payload_counts(&rt, &[old]).await, (before.0 + 1, before.1 + 1));
    let two = ab_payload_apply(&alice, batch, vec![second.clone()], false).await;
    assert_eq!(two["entries"][0]["outcome"], "written", "separate calls are outside in-call grouping");
    assert_eq!(ab_payload_counts(&rt, &[old]).await, (before.0 + 2, before.1 + 2));
    let ids = [ab_payload_id(&one, 0), ab_payload_id(&two, 0)];
    assert_ne!(ids[0], ids[1]);
    let stored_one = ab_payload_stored(&alice, batch, old, ids[0]).await;
    let stored_two = ab_payload_stored(&alice, batch, old, ids[1]).await;
    assert_ne!(stored_one["properties"]["batch_row"], stored_two["properties"]["batch_row"],
        "stored batch_row identity still includes the part");
    for dry_run in [true, false] {
        let result = ab_payload_apply(&alice, batch, vec![second.clone(), first.clone()], dry_run).await;
        assert_eq!(result["counts"]["deduped"], 2);
        assert_eq!(result["counts"]["written"], 0);
        assert_eq!(result["counts"]["refused"], 0, "existing Deduped plans are preserved");
        assert_eq!(ab_payload_id(&result, 0), ids[1]);
        assert_eq!(ab_payload_id(&result, 1), ids[0]);
        assert_eq!(ab_payload_counts(&rt, &[old]).await, (before.0 + 2, before.1 + 2));
    }
}

#[tokio::test]
async fn test_apply_batch_duplicate_payload_keeps_complete_typed_field_differences() {
    let (rt, alice, old) = ab_payload_fixture("typed-fields").await;
    let first = ab_payload_row(old, 1, "reed warbler typed successor");
    let second = ab_payload_row(old, 2, "reed warbler typed successor");
    let mut cases: Vec<(&str, Value, Value)> = Vec::new();
    let mut changed = second.clone();
    changed["content"] = json!("reed warbler typed successor ");
    cases.push(("content-not-trimmed", first.clone(), changed));
    let mut changed = second.clone();
    changed["memory_type"] = json!("semantic");
    cases.push(("memory-type", first.clone(), changed));
    let mut changed = second.clone();
    changed["salience"] = json!(0.2);
    cases.push(("salience", first.clone(), changed));
    let mut left = first.clone();
    let mut right = second.clone();
    left["properties"]["site"] = json!("north-marsh");
    right["properties"]["site"] = json!("south-marsh");
    cases.push(("property-value", left, right));
    let mut changed = second.clone();
    changed["tags"] = json!(["wetland", "forest"]);
    cases.push(("tag-value", first.clone(), changed));
    let mut changed = second.clone();
    changed["tags"] = json!(["field", "wetland"]);
    cases.push(("tag-order", first.clone(), changed));
    let mut left = first.clone();
    let mut right = second.clone();
    left["properties"]["source_ids"] = json!(["sheet-a", "sheet-b"]);
    right["properties"]["source_ids"] = json!(["sheet-b", "sheet-a"]);
    cases.push(("property-array-order", left, right));
    let mut omitted = first.clone();
    omitted.as_object_mut().expect("row object").remove("memory_type");
    cases.push(("omitted-type", omitted, second.clone()));
    let mut omitted = first.clone();
    omitted.as_object_mut().expect("row object").remove("salience");
    omitted["properties"]["verified"] = json!(true);
    let mut explicit = second.clone();
    explicit["properties"]["verified"] = json!(true);
    cases.push(("omitted-salience", omitted, explicit));
    let mut omitted = first.clone();
    omitted.as_object_mut().expect("row object").remove("tags");
    let mut explicit = second.clone();
    explicit["tags"] = json!([]);
    cases.push(("omitted-tags", omitted, explicit));
    for (name, left, right) in cases {
        ab_payload_two_successors(&rt, &alice, &format!("payload-field-{name}"), vec![left, right]).await;
    }
    let other_old = ab_seed(&alice, "reed warbler other old observation").await;
    let stored = alice.dispatch("get", json!({"id":other_old.to_string()}))
        .await.expect("resolve the second real old memory");
    assert_eq!(stored["namespace"], "alice");
    ab_payload_two_successors(&rt, &alice, "payload-different-old",
        vec![first, ab_payload_row(other_old, 2, "reed warbler typed successor")]).await;
}

#[tokio::test]
async fn test_apply_batch_duplicate_payload_canonical_uuid_and_object_order_match() {
    let (rt, alice, old) = ab_payload_fixture("canonical").await;
    let mut first = ab_payload_row(old, 2, "reed warbler canonical successor");
    let mut second = ab_payload_row(old, 1, "reed warbler canonical successor");
    let mut first_properties = serde_json::Map::new();
    first_properties.insert("who".to_string(), json!("batch-tester"));
    first_properties.insert("source_ids".to_string(), json!(["sheet-a", "sheet-b"]));
    first_properties.insert("site".to_string(), json!("north-marsh"));
    let mut second_properties = serde_json::Map::new();
    second_properties.insert("site".to_string(), json!("north-marsh"));
    second_properties.insert("source_ids".to_string(), json!(["sheet-a", "sheet-b"]));
    second_properties.insert("who".to_string(), json!("batch-tester"));
    first["properties"] = Value::Object(first_properties);
    second["properties"] = Value::Object(second_properties);
    second["id"] = json!(old.to_string().replace('-', "").to_ascii_uppercase());
    ab_payload_duplicate_group(&rt, &alice, "payload-canonical", old, vec![first, second], 0, 1).await;
}

async fn ab_payload_precedence_case(
    rt: &KhiveRuntime, registry: &khive_runtime::VerbRegistry, batch: &str, old: Uuid,
    rows: Vec<Value>, codes: &[Option<&str>], winner: usize,
) -> Value {
    assert_eq!(rows.len(), codes.len(),
        "batch={batch} dry_run=unavailable winner_index={winner} winner_refusal_code=unavailable phase=before-result");
    let before = ab_payload_counts(rt, &[old]).await;
    let mut applied = Value::Null;
    for dry_run in [true, false] {
        let result = ab_payload_apply(registry, batch, rows.clone(), dry_run).await;
        let context = format!(
            "batch={batch} dry_run={dry_run} winner_index={winner} winner_refusal_code={}",
            result["entries"][winner]["reason"]["code"],
        );
        for (index, code) in codes.iter().enumerate() {
            if let Some(code) = code {
                assert_eq!(result["entries"][index]["reason"]["code"], *code,
                    "{context} entry_index={index}: refusal precedence");
                assert_eq!(result["entries"][index]["outcome"], "refused",
                    "{context} entry_index={index}: refused-row outcome");
                assert!(result["entries"][index]["new_id"].is_null(),
                    "{context} entry_index={index}: refused row has no successor id");
            } else {
                assert_eq!(index, winner, "{context} entry_index={index}: designated winner");
                assert_eq!(result["entries"][index]["outcome"], if dry_run { "planned" } else { "written" },
                    "{context} entry_index={index}: winner-row outcome");
            }
        }
        assert_eq!(result["counts"]["refused"], json!(rows.len() - 1), "{context}: refused count");
        if dry_run {
            assert_eq!(result["counts"]["written"], 0, "{context}: dry-run written count");
            assert_eq!(ab_payload_counts(rt, &[old]).await, before, "{context}: dry-run note/edge counts");
        } else {
            assert_eq!(result["counts"]["written"], 1, "{context}: real written count");
            assert_eq!(ab_payload_counts(rt, &[old]).await, (before.0 + 1, before.1 + 1),
                "{context}: refused rows neither win nor cause extra note/edge writes");
            ab_payload_stored(registry, batch, old, ab_payload_id(&result, winner)).await;
            applied = result;
        }
    }
    applied
}

#[tokio::test]
async fn test_apply_batch_duplicate_payload_preserves_invalid_and_key_refusals() {
    let (rt, alice, old) = ab_payload_fixture("precedence").await;
    let candidate = "reed warbler precedence successor";
    let mut invalid = ab_payload_row(old, 2, candidate);
    invalid["unrecognized"] = json!(true);
    let result = ab_payload_precedence_case(&rt, &alice, "payload-invalid", old,
        vec![invalid, ab_payload_row(old, 1, candidate), ab_payload_row(old, 3, candidate)],
        &[Some("invalid-row"), None, Some("duplicate-payload")], 1).await;
    ab_payload_duplicate(&result, 2, 1, 1);
    ab_payload_precedence_case(&rt, &alice, "payload-key-peers", old,
        vec![ab_payload_row(old, 2, candidate), ab_payload_row(old, 2, candidate), ab_payload_row(old, 1, candidate)],
        &[Some("duplicate-key"), Some("duplicate-key"), None], 2).await;
    let mut missing_who = ab_payload_row(old, 2, candidate);
    missing_who["properties"].as_object_mut().expect("properties object").remove("who");
    ab_payload_precedence_case(&rt, &alice, "payload-who-precedence", old,
        vec![missing_who, ab_payload_row(old, 2, candidate), ab_payload_row(old, 1, candidate)],
        &[Some("who-missing"), Some("duplicate-key"), None], 2).await;
    let mut default_part = ab_payload_row(old, 1, candidate);
    default_part.as_object_mut().expect("row object").remove("part");
    ab_payload_precedence_case(&rt, &alice, "payload-default-part", old,
        vec![default_part, ab_payload_row(old, 1, candidate), ab_payload_row(old, 2, candidate)],
        &[Some("duplicate-key"), Some("duplicate-key"), None], 2).await;

    let batch = "payload-held-refusal";
    let before_seed = ab_payload_counts(&rt, &[old]).await;
    let held = ab_payload_apply(&alice, batch,
        vec![ab_payload_row(old, 2, "reed warbler already stored different payload")], false).await;
    assert_eq!(held["entries"][0]["outcome"], "written");
    assert_eq!(ab_payload_counts(&rt, &[old]).await, (before_seed.0 + 1, before_seed.1 + 1));
    let held_id = ab_payload_id(&held, 0);
    let stored_before = ab_payload_stored(&alice, batch, old, held_id).await;
    let result = ab_payload_precedence_case(&rt, &alice, batch, old,
        vec![ab_payload_row(old, 2, candidate), ab_payload_row(old, 1, candidate)],
        &[Some("dedupe-name-taken"), None], 1).await;
    assert_ne!(ab_payload_id(&result, 1), held_id);
    assert_eq!(ab_payload_stored(&alice, batch, old, held_id).await, stored_before,
        "held-key refusal preserves its previously stored note");
}
