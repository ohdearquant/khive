// ── auto-compose draft member filter regression ──────────────

// When compose runs in explicit domain_ids mode (is_auto=false), draft member atoms
// must NOT be filtered — the caller opted in by supplying the domain directly.
#[tokio::test]
async fn explicit_domain_ids_compose_includes_draft_member_atoms() {
    let f = pack(rt());

    // Seed a draft member atom.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "compose-draft-member",
                "name": "Compose Draft Member",
                "content": "compose explicit domain draft member atom dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique cmp78d"
            }]
        }),
    )
    .await
    .expect("seed draft atom");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("compose-draft-member".into())],
    )
    .await;

    // Upsert a domain whose member list includes the draft atom.
    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "compose-explicit-domain",
                "name": "Compose Explicit Domain",
                "description": "compose explicit domain ids draft member test dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique cmp78e",
                "members": ["compose-draft-member"]
            }]
        }),
    )
    .await
    .expect("upsert domain");

    // Get the domain id.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "compose explicit domain ids draft member unique cmp78e", "type": "domain", "rerank": false }),
        )
        .await
        .expect("search domain");
    let domain_id = resp["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|r| r["slug"].as_str() == Some("compose-explicit-domain"))
        .and_then(|r| r["id"].as_str())
        .expect("domain id in results")
        .to_string();

    // Explicit domain_ids compose: draft member atom must appear (caller opted in).
    let compose_resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "query": "compose explicit domain ids draft member unique cmp78e",
                "domain_ids": [&domain_id]
            }),
        )
        .await
        .expect("compose ok");

    let atoms = compose_resp["data"]["atoms"].as_array().expect("atoms");
    let atom_names: Vec<&str> = atoms.iter().filter_map(|a| a["slug"].as_str()).collect();
    assert!(
        atom_names.contains(&"compose-draft-member"),
        "explicit domain_ids compose must include draft member atoms (caller opted in): {atom_names:?}"
    );
}

// ── #80: stats.total_events must count real knowledge events ─────────────────

fn pack_with_events(rt: KhiveRuntime) -> Fixture {
    let rt_clone = rt.clone();
    let mut builder = VerbRegistryBuilder::new();
    builder
        .with_runtime_event_store(&rt)
        .expect("configure trusted runtime audit store");
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture {
        registry,
        rt: rt_clone,
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn stats_total_events_counts_knowledge_verbs() {
    let f = pack_with_events(rt());

    let long_content = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu \
                        nu xi omicron pi rho sigma tau upsilon phi chi psi omega";

    // Dispatch two knowledge verbs so their audit events land in the events table.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "evt-atom-a", "name": "Event Atom A", "content": long_content }
            ]
        }),
    )
    .await
    .expect("upsert atoms");

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "evt-atom-b", "name": "Event Atom B", "content": long_content }
            ]
        }),
    )
    .await
    .expect("upsert atoms second");

    // Non-knowledge verbs in the SAME namespace must NOT be counted: this proves the
    // `verb LIKE 'knowledge.%'` predicate actually filters, not merely that events
    // exist. Two of them push a broken (unfiltered) count to >= 4, outside [2, 3].
    for name in ["evt-entity-a", "evt-entity-b"] {
        f.dispatch("create", json!({ "kind": "concept", "name": name }))
            .await
            .expect("create concept");
    }

    let stats = f
        .dispatch("knowledge.stats", json!({}))
        .await
        .expect("stats ok");

    let total_events = stats["total_events"]
        .as_i64()
        .expect("total_events must be an integer");

    // 2 = the knowledge.upsert_atoms dispatches; +1 if the knowledge.stats audit event
    // is recorded before the handler's COUNT runs. The 2 non-knowledge `create` events
    // must be excluded, so an unfiltered predicate would yield >= 4.
    assert!(
        (2..=3).contains(&total_events),
        "total_events must count the knowledge.* dispatches but EXCLUDE the 2 \
         non-knowledge `create` events; expected 2 or 3, got {total_events}"
    );
}
