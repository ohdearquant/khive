// =============================================================================
// ADR-007 Rev 4 regression tests — additive read visible-set
// =============================================================================

/// (b) Writes stamp the local namespace even when the visible set names another namespace.
///
/// The registry has no actor, so the caller is anonymous. A `memory.remember`
/// dispatched with no explicit `namespace=` param must produce a note with
/// `namespace == "local"`. The visible set widens reads only; it never routes
/// storage writes.
#[tokio::test]
async fn adr007_rev4_writes_stamp_local() {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        // No real embedder: this test exercises namespace scoping, not embedding.
        // The CI runner has no model files, so loading one would fail (see a/a2).
        embedding_model: None,
        additional_embedding_models: vec![],
        visible_namespaces: vec![Namespace::parse("lambda:khive").unwrap()],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");

    let mut builder = VerbRegistryBuilder::new();
    builder.with_visible_namespaces(rt.config().visible_namespaces.clone());
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt.clone()),
    ));
    let registry = builder.build().expect("registry builds");

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "adr007 rev4 write test — must land in local",
                "memory_type": "semantic",
                "salience": 0.6
            }),
        )
        .await
        .expect("memory.remember must succeed");

    let note_id = result["id"].as_str().expect("note id present");
    let tok_local = rt.authorize(Namespace::local()).expect("authorize local");
    let note = rt
        .get_note_including_deleted(&tok_local, Uuid::parse_str(note_id).unwrap())
        .await
        .expect("get must succeed")
        .expect("note must exist in local");
    assert_eq!(
        note.namespace, "local",
        "write must stamp local, not actor namespace; got: {}",
        note.namespace
    );
}

/// (c) No actor configured → default visible-set is exactly {local}.
///
/// A registry built with no visible_namespaces must NOT surface notes written
/// to a foreign namespace via default-path recall.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn adr007_rev4_no_actor_yields_local_only_visible_set() {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        // No real embedder: scoping test, FTS recall only (see a/a2). CI has no model files.
        embedding_model: None,
        additional_embedding_models: vec![],
        // no visible_namespaces — same as Rev 3 behavior
        visible_namespaces: vec![],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");

    // Write a note directly into the foreign namespace.
    let tok_foreign = rt
        .authorize(Namespace::parse("ns-foreign-c").unwrap())
        .expect("authorize foreign");
    rt.create_note(
        &tok_foreign,
        "memory",
        None,
        "adr007-c-foreign-content-unique-marker",
        None,
        None,
        vec![],
    )
    .await
    .expect("create note in foreign ns");

    // Build the registry with no visible_namespaces (zero actor extras).
    let mut builder = VerbRegistryBuilder::new();
    // deliberately NOT calling with_visible_namespaces
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt.clone()),
    ));
    let registry = builder.build().expect("registry builds");

    // Default recall must NOT surface the foreign note.
    let hits = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "adr007-c-foreign-content-unique-marker" }),
        )
        .await
        .expect("memory.recall must succeed");
    let hits = hits.as_array().expect("recall returns array");
    let foreign_visible = hits.iter().any(|h| {
        h["content"]
            .as_str()
            .unwrap_or("")
            .contains("adr007-c-foreign-content-unique-marker")
    });
    assert!(
        !foreign_visible,
        "no-actor config: foreign-namespace note must NOT appear in default recall (visible-set={{local}} only)"
    );
}

/// (d) Explicit namespace= is strict — Reading 2 regression guard.
///
/// When `namespace="lambda:khive"` is passed explicitly, the result set must
/// be EXACTLY that namespace. A note written to `local` must NOT appear in the
/// results (the explicit param is not widened by visible_namespaces).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn adr007_rev4_explicit_namespace_is_strict_reading2() {
    const MODEL_A: &str = "custom-enc-a";
    const DIMS: usize = 4;

    // No real model (CI has no model files); a ConstVecProvider drives the vector leg
    // instead. The constant vector is identical across notes, so the local note WOULD
    // surface if the explicit namespace= weren't strict — making this a sharp Reading-2
    // guard rather than relying on FTS lexical luck for the hyphenated marker query.
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        visible_namespaces: vec![Namespace::parse("lambda:khive").unwrap()],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");
    rt.register_embedder(ConstVecProvider::new(MODEL_A, DIMS, 0.9));

    // Write a note to local (default path — no explicit namespace).
    let tok_local = rt.authorize(Namespace::local()).expect("authorize local");
    rt.create_note(
        &tok_local,
        "memory",
        None,
        "adr007-d-local-content-marker",
        None,
        None,
        vec![],
    )
    .await
    .expect("create local note");

    // Write a note to lambda:khive (explicit namespace).
    let tok_lk = rt
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .expect("authorize lambda:khive");
    let lk_note = rt
        .create_note(
            &tok_lk,
            "memory",
            None,
            "adr007-d-lambdakhive-content-marker",
            None,
            None,
            vec![],
        )
        .await
        .expect("create lambda:khive note");

    let mut builder = VerbRegistryBuilder::new();
    builder.with_visible_namespaces(rt.config().visible_namespaces.clone());
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt.clone()),
    ));
    let registry = builder.build().expect("registry builds");

    // Explicit namespace="lambda:khive" → strict {lambda:khive} only.
    // The local note must NOT appear even though visible_namespaces includes it.
    let results = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "adr007-d",
                "namespace": "lambda:khive"
            }),
        )
        .await
        .expect("recall with explicit namespace must succeed");
    let hits = results.as_array().expect("recall returns array");

    let local_leaked = hits.iter().any(|h| {
        h["content"]
            .as_str()
            .unwrap_or("")
            .contains("adr007-d-local-content-marker")
    });
    assert!(
        !local_leaked,
        "Reading 2 violation: explicit namespace=lambda:khive must NOT return local-namespace notes; \
         visible_namespaces must not widen explicit-namespace escapes"
    );

    let lk_found = hits.iter().any(|h| {
        h["id"].as_str() == Some(&lk_note.id.to_string())
            || h["content"]
                .as_str()
                .unwrap_or("")
                .contains("adr007-d-lambdakhive-content-marker")
    });
    assert!(
        lk_found,
        "explicit namespace=lambda:khive must return the lambda:khive note"
    );
}

/// (e) By-ID get of a lambda:khive-attributed note works namespace-agnostically.
///
/// Rule 2 (shipped PR-A1): get by UUID is globally resolved regardless of token.
/// This test is a regression guard that the Rev 4 changes did not re-introduce
/// any namespace check on the by-ID path.
#[tokio::test]
async fn adr007_rev4_get_byid_is_namespace_agnostic() {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        // No real embedder: by-ID get needs no embedding (see a/a2). CI has no model files.
        embedding_model: None,
        additional_embedding_models: vec![],
        visible_namespaces: vec![],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");

    // Create a note attributed to lambda:khive.
    let tok_lk = rt
        .authorize(Namespace::parse("lambda:khive").unwrap())
        .expect("authorize lambda:khive");
    let lk_note = rt
        .create_note(
            &tok_lk,
            "memory",
            None,
            "adr007-e-byid-content",
            None,
            None,
            vec![],
        )
        .await
        .expect("create lambda:khive note");

    let mut builder = VerbRegistryBuilder::new();
    // No visible_namespaces — local-only token will be minted by dispatch default.
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt.clone()),
    ));
    let registry = builder.build().expect("registry builds");

    // get by UUID with no namespace= param → must resolve the lambda:khive note.
    let result = registry
        .dispatch("get", json!({ "id": lk_note.id.to_string() }))
        .await
        .expect("get by UUID must succeed regardless of namespace");

    let record = result.get("record").unwrap_or(&result);
    let returned_id = record["id"].as_str().expect("id present in get result");
    assert_eq!(
        returned_id,
        lk_note.id.to_string().as_str(),
        "by-ID get must return the lambda:khive-attributed note (Rule 2 / PR-A1 regression guard)"
    );
}
