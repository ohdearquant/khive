// ── Kind hooks (ADR-030) — shared CRUD reaches gtd-owned `task` via TaskHook ──

#[tokio::test]
async fn kg_create_with_note_kind_task_invokes_gtd_hook_defaults() -> anyhow::Result<()> {
    let client = connect().await?;
    // Drive the kg `create` verb with note_kind="task" — the kg handler
    // consults the registry, finds gtd's TaskHook, and the hook fills GTD
    // defaults (status=inbox) before the storage write.
    let created = ok_one(
        &client,
        r#"create(kind="note", note_kind="task", title="ship release", priority="p0")"#,
    )
    .await?;

    // Response is the kg note envelope, NOT the gtd task envelope.
    assert_eq!(created["kind"], "task", "note stored with kind=task");
    assert_eq!(created["name"], "ship release", "title folded into name");
    assert_eq!(
        created["properties"]["status"], "inbox",
        "TaskHook applies default status"
    );
    assert_eq!(
        created["properties"]["priority"], "p0",
        "user-supplied priority preserved in properties"
    );
    Ok(())
}

#[tokio::test]
async fn kg_create_note_kind_task_resolves_depends_on_against_task_target() -> anyhow::Result<()> {
    let client = connect().await?;

    // Stand up a task that the new task will depend on. The GTD ADR-031 edge
    // rule allows depends_on between two task notes, so this is the only
    // shape the kg-create-with-task-kind path will accept.
    let blocker = ok_one(&client, r#"gtd.assign(title="write spec")"#).await?;
    let blocker_full = blocker["full_id"].as_str().unwrap().to_string();

    let task = ok_one(
        &client,
        &format!(
            r#"create(kind="note", note_kind="task", title="depends on something", depends_on=["{}"])"#,
            blocker_full
        ),
    )
    .await?;

    // Hook resolved the short/full id into a canonical UUID string and
    // placed it in `properties.depends_on` — same shape gtd's `assign`
    // produces.
    let deps = task["properties"]["depends_on"].as_array().unwrap();
    assert_eq!(deps.len(), 1, "exactly one resolved dependency");
    let resolved = deps[0].as_str().unwrap();
    assert!(
        resolved.contains('-'),
        "depends_on stored as full UUID string, got: {resolved}"
    );
    assert_eq!(resolved, &blocker_full, "depends_on resolves to blocker");
    Ok(())
}

#[tokio::test]
async fn kg_create_note_kind_task_rejects_non_task_depends_on_before_write() -> anyhow::Result<()> {
    let client = connect().await?;

    // Stand up an entity target. The GTD ADR-031 edge rule is task→task only,
    // so the kg-create path must reject this BEFORE the task is persisted —
    // otherwise we'd leave a task with `properties.depends_on` pointing at a
    // non-task (ADR-030 forbids reporting failure after a successful write).
    let entity = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="DependencyTarget")"#,
    )
    .await?;
    // Entity create returns the storage-layer struct keyed on `id` (full UUID),
    // not the GTD task envelope shape.
    let entity_full = entity["id"].as_str().unwrap().to_string();

    let result = call(
        &client,
        "request",
        json!({"ops": format!(
            r#"create(kind="note", note_kind="task", title="depends on entity", depends_on=["{}"])"#,
            entity_full
        )}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false, "expected rejection: {first}");
    let err = first["error"]["message"].as_str().unwrap();
    assert!(
        err.contains("must be a task note"),
        "error must point to the GTD edge rule: {err}"
    );

    // And there should be no task with the supplied title — write was prevented.
    let listed = ok_one(&client, r#"list(kind="note", note_kind="task")"#).await?;
    let notes = listed["items"]
        .as_array()
        .expect("note list items envelope");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["name"].as_str()).collect();
    assert!(
        !titles.contains(&"depends on entity"),
        "task must not be persisted when depends_on validation fails: {titles:?}"
    );
    Ok(())
}

#[tokio::test]
async fn gtd_assign_creates_depends_on_edge_between_two_tasks() -> anyhow::Result<()> {
    let client = connect().await?;

    let blocker = ok_one(&client, r#"gtd.assign(title="write spec")"#).await?;
    let blocker_full = blocker["full_id"].as_str().unwrap().to_string();
    let dependent = ok_one(
        &client,
        &format!(
            r#"gtd.assign(title="implement feature", depends_on=["{}"])"#,
            blocker_full
        ),
    )
    .await?;
    let dep_full = dependent["full_id"].as_str().unwrap().to_string();

    // ADR-031: the GTD pack's EDGE_RULES adds task→task `depends_on`.
    // `neighbors(node_id=dependent, direction="out", relations=["depends_on"])`
    // should surface the blocker — proving the edge landed.
    let neighbors = ok_one(
        &client,
        &format!(
            r#"neighbors(node_id="{}", direction="out", relations=["depends_on"])"#,
            dep_full
        ),
    )
    .await?;

    let hits = neighbors.as_array().expect("neighbors returns array");
    // Response uses canonical `id` (legacy `node_id` accepted as alias on input only).
    let targets: Vec<&str> = hits.iter().filter_map(|h| h["id"].as_str()).collect();
    assert!(
        targets.iter().any(|t| *t == blocker_full),
        "task→task depends_on edge missing — got targets {targets:?}"
    );
    Ok(())
}

#[tokio::test]
async fn kg_create_unknown_note_kind_lists_merged_pack_vocabulary() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"create(kind="note", note_kind="bogus", content="x")"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false);
    let err = first["error"]["message"].as_str().unwrap();
    assert!(err.contains("bogus"), "error names the bad kind: {err}");
    // The merged vocabulary list must include "task" (gtd) alongside kg kinds.
    assert!(
        err.contains("task"),
        "error must list gtd-registered 'task' kind: {err}"
    );
    assert!(
        err.contains("observation"),
        "error must list kg's 'observation' kind: {err}"
    );
    Ok(())
}

// This leaf is include!-expanded at the integration target root, so this
// selector is intentionally unprefixed (there is no kind_hooks module).
#[tokio::test]
async fn schema_declares_every_entity_and_note_kind_written_by_driven_store() -> anyhow::Result<()>
{
    disable_daemon();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test")?,
        packs: vec!["kg".into(), "gtd".into()],
        ..RuntimeConfig::no_embeddings()
    })?;
    let server = KhiveMcpServer::new(runtime.clone())?;
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server_task = tokio::spawn(async move {
        if let Ok(service) = server.serve(server_transport).await {
            let _ = service.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    let concept = ok_one(&client,
        r#"create(kind="concept", name="kind census concept", description="actual entity", skip_dedup_check=true)"#).await?;
    let concept_id = uuid::Uuid::parse_str(concept["id"].as_str().expect("created entity id"))?;
    let observation = ok_one(&client,
        r#"create(kind="note", note_kind="observation", content="actual note", skip_dedup_check=true)"#).await?;
    let observation_id =
        uuid::Uuid::parse_str(observation["id"].as_str().expect("created observation id"))?;
    let task = ok_one(
        &client,
        r#"create(kind="note", note_kind="task", title="kind census blocker")"#,
    )
    .await?;
    let task_id = task["id"].as_str().expect("created task id");
    let dependent = ok_one(
        &client,
        &format!(
            r#"gtd.assign(title="kind census dependent", depends_on=["{}"])"#,
            task_id
        ),
    )
    .await?;
    let dependent_id = dependent["full_id"]
        .as_str()
        .expect("assigned task full id");
    let neighbors = ok_one(
        &client,
        &format!(
            r#"neighbors(id="{}", direction="out", relations=["depends_on"])"#,
            dependent_id
        ),
    )
    .await?;
    assert!(
        neighbors
            .as_array()
            .expect("neighbors record array")
            .iter()
            .any(|hit| hit["id"].as_str() == Some(task_id)),
        "actual dependency edge is persisted: {neighbors}"
    );
    let token = runtime.authorize(Namespace::parse("test")?)?;
    assert_eq!(
        runtime.get_entity(&token, concept_id).await?.kind,
        "concept"
    );
    let stored_observation = runtime
        .backend()
        .notes()?
        .get_note(observation_id)
        .await?
        .expect("publicly created observation is stored");
    assert_eq!(stored_observation.kind, "observation");

    let schema = ok_one(&client, "schema()").await?;
    let declared_entities: std::collections::BTreeSet<String> = schema["entity_kinds"]
        .as_array()
        .expect("merged entity vocabulary")
        .iter()
        .map(|kind| kind.as_str().expect("entity kind string").to_owned())
        .collect();
    let declared_notes: std::collections::BTreeSet<String> = schema["note_kinds"]
        .as_array()
        .expect("merged note vocabulary")
        .iter()
        .map(|kind| kind.as_str().expect("note kind string").to_owned())
        .collect();
    // Inspect every stored kind, including tombstones/startup/hook writes, not
    // just the returned IDs or one limited public page of the driven store.
    let (stored_entities, stored_notes) =
        runtime.backend().pool().writer()?.autocommit(|conn| {
            let mut entities = conn.prepare("SELECT DISTINCT kind FROM entities")?;
            let entity_rows = entities.query_map([], |row| row.get::<_, String>(0))?;
            let entity_kinds = entity_rows.collect::<Result<std::collections::BTreeSet<_>, _>>()?;
            let mut notes = conn.prepare("SELECT DISTINCT kind FROM notes")?;
            let note_rows = notes.query_map([], |row| row.get::<_, String>(0))?;
            let note_kinds = note_rows.collect::<Result<std::collections::BTreeSet<_>, _>>()?;
            Ok((entity_kinds, note_kinds))
        })?;
    let undeclared_entities: Vec<_> = stored_entities.difference(&declared_entities).collect();
    let undeclared_notes: Vec<_> = stored_notes.difference(&declared_notes).collect();
    assert!(
        undeclared_entities.is_empty(),
        "driven store wrote undeclared entity kinds: {undeclared_entities:?}"
    );
    assert!(
        undeclared_notes.is_empty(),
        "driven store wrote undeclared note kinds: {undeclared_notes:?}"
    );
    assert!(
        stored_entities.contains("concept"),
        "entity drive cannot be vacuous"
    );
    for kind in ["observation", "task"] {
        assert!(
            stored_notes.contains(kind),
            "stored note drive must include {kind}"
        );
    }
    drop(client);
    server_task.abort();
    if let Err(error) = server_task.await {
        assert!(error.is_cancelled(), "MCP server task failed: {error}");
    }
    Ok(())
}
