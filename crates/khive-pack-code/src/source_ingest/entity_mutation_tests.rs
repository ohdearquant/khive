#[cfg(test)]
#[tokio::test]
async fn issue2673_code_entity_mutation_rebases_persisted_versions() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let id = Uuid::new_v4();
    let mut report = CodeSourceIngestReport::default();
    for version in 1..=3 {
        let name = format!("code revision {version}");
        let outcome = mutate_entity(&runtime, &token, id, "version.rs", &mut report, |_| {
            let mut entity = Entity::new("local", "concept", &name);
            entity.id = id;
            Some(entity)
        })
        .await
        .unwrap();
        assert!(outcome.wrote());
        let stored = runtime.get_entity(&token, id).await.unwrap();
        assert_eq!(stored.version, version);
        assert_eq!(stored.name, name);
    }
    assert_eq!(
        mutate_entity(&runtime, &token, id, "version.rs", &mut report, |_| None)
            .await
            .unwrap(),
        RowMutationOutcome::Unchanged
    );
    assert_eq!(runtime.get_entity(&token, id).await.unwrap().version, 3);
}

#[cfg(test)]
#[tokio::test]
async fn code_entity_mutation_refuses_reserved_candidate_and_carried_properties() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let mut report = CodeSourceIngestReport::default();
    let reserved = json!({"khive:secret_gate": "exempted:content-sha256-manifest-v1"});

    let candidate =
        Entity::new("local", "concept", "reserved candidate").with_properties(reserved.clone());
    let candidate_id = candidate.id;
    let error = mutate_entity(
        &runtime,
        &token,
        candidate_id,
        "reserved.rs",
        &mut report,
        |_| Some(candidate.clone()),
    )
    .await
    .expect_err("a reserved candidate must not be inserted");
    assert!(
        matches!(error, CodeSourceIngestError::Runtime(RuntimeError::InvalidInput(ref message)) if message.contains("khive:secret_gate")),
        "unexpected error: {error:?}"
    );
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(candidate_id)
        .await
        .unwrap()
        .is_none());

    let current = Entity::new("local", "concept", "original").with_properties(reserved);
    let current_id = current.id;
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(current)
        .await
        .unwrap();
    let before = runtime
        .entities(&token)
        .unwrap()
        .get_entity(current_id)
        .await
        .unwrap()
        .unwrap();
    let error = mutate_entity(
        &runtime,
        &token,
        current_id,
        "reserved.rs",
        &mut report,
        |current| {
            let mut replacement = current.cloned().expect("seeded row");
            replacement.name = "changed".into();
            Some(replacement)
        },
    )
    .await
    .expect_err("a carried reserved key must not be replaced");
    assert!(
        matches!(error, CodeSourceIngestError::Runtime(RuntimeError::InvalidInput(ref message)) if message.contains("khive:secret_gate")),
        "unexpected error: {error:?}"
    );
    let after = runtime
        .entities(&token)
        .unwrap()
        .get_entity(current_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap()
    );
}
