// ── resolver e2e tests (ADR-061): registry wired via PackRegistry::register_packs ────────

/// Build a VerbRegistry the same way the production MCP server does: via
/// `PackRegistry::register_packs`. This path calls `create_resolver` and wires
/// the knowledge `PackByIdResolver` into the registry, so generic `get` /
/// `delete` / `update` can reach knowledge-private tables.
fn pack_via_registry(rt: KhiveRuntime) -> Fixture {
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(
        &["kg".to_string(), "knowledge".to_string()],
        rt.clone(),
        &mut builder,
    )
    .expect("register_packs must succeed for kg+knowledge");
    let registry = builder.build().expect("registry build");
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture { registry }
}

/// Generic `get(id=<atom-uuid>)` via the resolver returns the same wire shape
/// as `knowledge.get(id=<slug>)`: tags is a JSON array, properties is a JSON
/// object (or null), and created_at/updated_at are ISO 8601 strings.
#[tokio::test]
async fn resolver_generic_get_atom_returns_public_wire_shape() {
    let f = pack_via_registry(rt());

    // Create an atom with tags and properties.
    let upsert = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "resolver-atom-e2e",
                    "name": "Resolver Atom E2E",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
                    "tags": ["test", "resolver"],
                    "properties": { "source": "test" }
                }]
            }),
        )
        .await
        .expect("upsert atom");
    assert_eq!(upsert["created"], 1);

    // Fetch the UUID from knowledge.get by slug.
    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "resolver-atom-e2e" }))
        .await
        .expect("knowledge.get by slug");
    let uuid = by_slug["id"].as_str().expect("id string");

    // Now fetch via generic get using the UUID — this exercises the resolver path.
    let by_uuid = f
        .dispatch("get", json!({ "id": uuid }))
        .await
        .expect("generic get by uuid");

    // kind must be atom.
    assert_eq!(by_uuid["kind"], "atom", "wrong kind: {by_uuid}");
    assert_eq!(by_uuid["slug"], "resolver-atom-e2e");
    assert_eq!(by_uuid["name"], "Resolver Atom E2E");

    // tags must be a JSON array, not a comma-separated string.
    let tags = by_uuid["tags"]
        .as_array()
        .expect("tags must be a JSON array");
    assert!(
        tags.iter().any(|t| t == "test"),
        "expected 'test' tag, got: {tags:?}"
    );

    // created_at and updated_at must be ISO 8601 strings, not raw microsecond integers.
    let created_at = by_uuid["created_at"]
        .as_str()
        .expect("created_at must be a string");
    assert!(
        created_at.contains('T'),
        "created_at must be ISO 8601, got: {created_at:?}"
    );
    let updated_at = by_uuid["updated_at"]
        .as_str()
        .expect("updated_at must be a string");
    assert!(
        updated_at.contains('T'),
        "updated_at must be ISO 8601, got: {updated_at:?}"
    );

    // properties must be a JSON object (or null), not a string.
    assert!(
        by_uuid["properties"].is_object() || by_uuid["properties"].is_null(),
        "properties must be object or null, got: {:?}",
        by_uuid["properties"]
    );
}

/// Generic `get(id=<domain-uuid>)` via the resolver returns the public wire
/// shape: tags and members are JSON arrays, timestamps are ISO 8601 strings.
#[tokio::test]
async fn resolver_generic_get_domain_returns_public_wire_shape() {
    let f = pack_via_registry(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "resolver-domain-e2e",
                "name": "Resolver Domain E2E",
                "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity tags members",
                "members": ["rag", "dense-retrieval"],
                "tags": ["test", "resolver"]
            }]
        }),
    )
    .await
    .expect("upsert domain");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "resolver-domain-e2e" }))
        .await
        .expect("knowledge.get domain by slug");
    let uuid = by_slug["id"].as_str().expect("id string");

    let by_uuid = f
        .dispatch("get", json!({ "id": uuid }))
        .await
        .expect("generic get domain by uuid");

    assert_eq!(by_uuid["kind"], "domain", "wrong kind: {by_uuid}");
    assert_eq!(by_uuid["slug"], "resolver-domain-e2e");

    // tags must be a JSON array, not a raw string.
    let tags = by_uuid["tags"]
        .as_array()
        .expect("tags must be a JSON array");
    assert!(
        tags.iter().any(|t| t == "test"),
        "expected 'test' tag, got: {tags:?}"
    );

    // members must be a JSON array, not a raw string.
    let members = by_uuid["members"]
        .as_array()
        .expect("members must be a JSON array");
    assert!(
        members.iter().any(|m| m == "rag"),
        "expected 'rag' member, got: {members:?}"
    );

    // timestamps must be ISO 8601 strings.
    let created_at = by_uuid["created_at"]
        .as_str()
        .expect("created_at must be a string");
    assert!(
        created_at.contains('T'),
        "created_at must be ISO 8601, got: {created_at:?}"
    );
}

/// Generic `delete(id=<domain-uuid>)` soft-deletes; subsequent generic
/// `get` returns NotFound.
#[tokio::test]
async fn resolver_generic_soft_delete_domain() {
    let f = pack_via_registry(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "resolver-delete-domain",
                "name": "Resolver Delete Domain",
                "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            }]
        }),
    )
    .await
    .expect("upsert domain");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "resolver-delete-domain" }))
        .await
        .expect("get domain before delete");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    // Soft-delete via generic delete.
    let del = f
        .dispatch("delete", json!({ "id": uuid }))
        .await
        .expect("generic soft delete");
    assert_eq!(del["deleted"], true, "soft delete response: {del}");

    // Generic get must now return NotFound.
    let not_found = f.dispatch("get", json!({ "id": uuid })).await;
    assert!(
        matches!(not_found, Err(RuntimeError::NotFound(_))),
        "expected NotFound after soft delete, got: {not_found:?}"
    );
}

/// Generic `delete(id=<atom-uuid>, hard=true)` hard-deletes a live atom.
#[tokio::test]
async fn resolver_generic_hard_delete_atom() {
    let f = pack_via_registry(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "resolver-hard-delete-atom",
                "name": "Resolver Hard Delete Atom",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert atom");

    let by_slug = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "resolver-hard-delete-atom" }),
        )
        .await
        .expect("get atom before delete");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    // Hard-delete the live atom directly.
    let hard_del = f
        .dispatch("delete", json!({ "id": uuid, "hard": true }))
        .await
        .expect("generic hard delete");
    assert_eq!(
        hard_del["deleted"], true,
        "hard delete response: {hard_del}"
    );

    // Generic get must now return NotFound.
    let not_found = f.dispatch("get", json!({ "id": uuid })).await;
    assert!(
        matches!(not_found, Err(RuntimeError::NotFound(_))),
        "expected NotFound after hard delete, got: {not_found:?}"
    );
}

/// Generic `update(id=<atom-uuid>)` returns InvalidInput because the knowledge
/// pack defers generic update (pack-private records require pack-specific verbs).
#[tokio::test]
async fn resolver_generic_update_atom_returns_invalid_input() {
    let runtime = rt();
    let f = pack_via_registry(runtime.clone());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "resolver-update-atom",
                "name": "Resolver Update Atom",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert atom");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "resolver-update-atom" }))
        .await
        .expect("get atom");
    let uuid = by_slug["id"].as_str().expect("id string");

    let before = issue_558_snapshot(&runtime).await;
    let err = f
        .dispatch("update", json!({ "id": uuid, "name": "New Name" }))
        .await
        .expect_err("update on knowledge atom must return an error");
    assert_eq!(issue_558_snapshot(&runtime).await, before);
    let RuntimeError::InvalidInput(message) = err else {
        panic!("expected InvalidInput, got: {err:?}");
    };
    for verb in [
        "the knowledge pack's own verbs",
        "knowledge.upsert_atoms",
        "knowledge.upsert_domains",
        "knowledge.edit",
    ] {
        assert!(
            message.contains(verb),
            "missing direction {verb}: {message}"
        );
    }
    assert!(!message.contains(by_slug["content"].as_str().expect("atom content")));
    assert_eq!(
        f.dispatch("knowledge.get", json!({"id": uuid}))
            .await
            .expect("unchanged atom"),
        by_slug
    );
}

/// Hard-delete an atom that has sections via `knowledge.edit`.
///
/// Without the fix this fails with `FOREIGN KEY constraint failed` because
/// `knowledge_sections` has a FK to `knowledge_atoms(id)` without `ON DELETE
/// CASCADE`.
#[tokio::test]
async fn resolver_generic_hard_delete_atom_with_sections() {
    let f = pack_via_registry(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "hard-delete-atom-with-sections",
                "name": "Hard Delete Atom With Sections",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert atom");

    let by_slug = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "hard-delete-atom-with-sections" }),
        )
        .await
        .expect("get atom before edit");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    // Add sections to the atom.
    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "hard-delete-atom-with-sections",
            "sections": [{
                "section_type": "overview",
                "content": "This section tests that hard-delete correctly removes dependent knowledge_sections rows before deleting the parent atom to satisfy the foreign key constraint."
            }]
        }),
    )
    .await
    .expect("add section to atom");

    // Hard-delete must succeed even though sections exist.
    let hard_del = f
        .dispatch("delete", json!({ "id": uuid, "hard": true }))
        .await
        .expect("hard delete atom with sections");
    assert_eq!(
        hard_del["deleted"], true,
        "hard delete response: {hard_del}"
    );

    // Generic get must now return NotFound.
    let not_found = f.dispatch("get", json!({ "id": uuid })).await;
    assert!(
        matches!(not_found, Err(RuntimeError::NotFound(_))),
        "expected NotFound after hard delete, got: {not_found:?}"
    );
}

/// Hard-delete a domain whose mirror atom has sections via `knowledge.edit`.
///
/// Without the fix the domain row is deleted first and then the mirror atom
/// delete fails with `FOREIGN KEY constraint failed`, leaving a partial delete.
#[tokio::test]
async fn resolver_generic_hard_delete_domain_with_mirror_sections() {
    let f = pack_via_registry(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "hard-delete-domain-with-sections",
                "name": "Hard Delete Domain With Sections",
                "description": "Domain whose mirror atom will have sections before hard-delete to verify that cascade-delete of dependent knowledge_sections rows works correctly here."
            }]
        }),
    )
    .await
    .expect("upsert domain");

    let domain = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "hard-delete-domain-with-sections" }),
        )
        .await
        .expect("get domain before edit");
    let domain_uuid = domain["id"].as_str().expect("id string").to_string();

    // Add sections to the mirror atom (same UUID as the domain).
    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "hard-delete-domain-with-sections",
            "sections": [{
                "section_type": "overview",
                "content": "This section tests that hard-delete of a domain removes dependent knowledge_sections rows from the mirror atom before deleting the atom and domain rows."
            }]
        }),
    )
    .await
    .expect("add section to domain mirror atom");

    // Hard-delete the domain — must succeed even though mirror atom has sections.
    let hard_del = f
        .dispatch("delete", json!({ "id": domain_uuid, "hard": true }))
        .await
        .expect("hard delete domain with mirror sections");
    assert_eq!(
        hard_del["deleted"], true,
        "hard delete response: {hard_del}"
    );

    // Domain must now be NotFound.
    let domain_not_found = f.dispatch("get", json!({ "id": domain_uuid })).await;
    assert!(
        matches!(domain_not_found, Err(RuntimeError::NotFound(_))),
        "expected NotFound for domain after hard delete, got: {domain_not_found:?}"
    );
}
