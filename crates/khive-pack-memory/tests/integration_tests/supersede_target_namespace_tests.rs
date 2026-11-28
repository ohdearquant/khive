// ADR-021 Delta 4: every writer of a `supersedes` edge onto a memory row applies
// `memory.apply_batch`'s old-row namespace rule. Each refusal runs beside a
// control that the same write is admitted for an allowed target.

async fn st_recall_ids(registry: &khive_runtime::VerbRegistry, query: &str) -> Vec<String> {
    registry
        .dispatch(
            "memory.recall",
            json!({
                "query": query,
                "limit": 10,
                "fusion_strategy": "keyword_only",
                "config": {"scoring": {"mmr_penalty": 0.0}}
            }),
        )
        .await
        .expect("recall")
        .as_array()
        .expect("recall returns an array")
        .iter()
        .filter_map(|hit| hit["id"].as_str().map(str::to_string))
        .collect()
}

fn st_registry(
    rt: KhiveRuntime,
    actor_id: &str,
    visible: &[&str],
    owned: &[&str],
) -> khive_runtime::VerbRegistry {
    let parse = |names: &[&str]| {
        names
            .iter()
            .map(|ns| Namespace::parse(ns).expect("namespace parses"))
            .collect::<Vec<_>>()
    };
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor_id.to_string()));
    if !visible.is_empty() {
        builder.with_visible_namespaces(parse(visible));
    }
    builder.with_owned_namespaces(parse(owned));
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt),
    ));
    builder.build().expect("registry builds")
}

async fn st_link(
    registry: &khive_runtime::VerbRegistry,
    source: Uuid,
    target: Uuid,
    relation: &str,
    namespace: &str,
) -> Result<Value, RuntimeError> {
    registry
        .dispatch(
            "link",
            json!({
                "source_id": source.to_string(),
                "target_id": target.to_string(),
                "relation": relation,
                "namespace": namespace
            }),
        )
        .await
}

fn st_refusal(result: Result<Value, RuntimeError>) -> String {
    match result {
        Err(error) => error.to_string(),
        Ok(value) => panic!("supersede_target_namespace_admitted: {value}"),
    }
}

#[tokio::test]
async fn link_supersedes_onto_foreign_memory_is_refused_like_apply_batch() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let bob = make_registry_with_actor(rt.clone(), "bob");
    let query = "heron roost survey supersede target rule";
    let bob_old = ab_seed(&bob, &format!("{query} bob original")).await;
    let alice_new = ab_seed(&alice, &format!("{query} alice replacement")).await;
    let alice_old = ab_seed(&alice, &format!("{query} alice earlier")).await;

    assert_eq!(
        ab_refusal_code(&rt, &alice, ab_supersede(bob_old, "heron revised")).await,
        "old-namespace",
        "apply_batch refuses the same old row"
    );
    st_link(&alice, alice_new, alice_old, "supersedes", "alice")
        .await
        .expect("control: a supersede between two own rows is written");
    assert_eq!(
        ab_source_namespace_edges(&rt, "alice", vec![alice_old], EdgeRelation::Supersedes)
            .await
            .len(),
        1
    );

    let message = st_refusal(st_link(&alice, alice_new, bob_old, "supersedes", "alice").await);
    assert!(
        message.contains("is in a namespace the caller cannot read")
            && message.contains("as memory.apply_batch requires"),
        "{message}"
    );
    assert!(
        ab_source_namespace_edges(&rt, "alice", vec![bob_old], EdgeRelation::Supersedes)
            .await
            .is_empty(),
        "a refused supersede stores no edge"
    );
}

#[tokio::test]
async fn refused_foreign_supersede_keeps_the_row_in_a_two_namespace_readers_recall() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let bob = make_registry_with_actor(rt.clone(), "bob");
    let reader = st_registry(rt.clone(), "carol", &["alice", "bob"], &[]);
    let query = "kestrel ledge tally supersede target reader";
    let bob_old = ab_seed(&bob, &format!("{query} bob original")).await;
    let bob_own_old = ab_seed(&bob, &format!("{query} bob own earlier")).await;
    let bob_own_new = ab_seed(&bob, &format!("{query} bob own replacement")).await;
    let alice_new = ab_seed(&alice, &format!("{query} alice replacement")).await;

    st_link(&bob, bob_own_new, bob_own_old, "supersedes", "bob")
        .await
        .expect("control: bob supersedes his own row");
    let after_own = st_recall_ids(&reader, query).await;
    assert!(
        !after_own.contains(&bob_own_old.to_string()) && after_own.contains(&bob_old.to_string()),
        "control: an own-namespace supersede hides only its target from this reader: {after_own:?}"
    );

    assert!(st_link(&alice, alice_new, bob_old, "supersedes", "alice")
        .await
        .is_err());
    let after = st_recall_ids(&reader, query).await;
    assert!(
        after.contains(&bob_old.to_string()),
        "the reader keeps bob's row: {after:?}"
    );
}

#[tokio::test]
async fn non_memory_source_supersede_onto_foreign_memory_is_refused() {
    let rt = make_runtime();
    let bob = make_registry_with_actor(rt.clone(), "bob");
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let bob_old = ab_seed(&bob, "grebe colony census bob original").await;
    let alice_old = ab_seed(&alice, "grebe colony census alice earlier").await;
    let token = rt
        .authorize(Namespace::parse("alice").unwrap())
        .expect("authorize alice");
    let observation = khive_storage::Note::new("alice", "observation", "grebe colony note");
    let observation_id = observation.id;
    rt.notes(&token)
        .unwrap()
        .upsert_note(observation)
        .await
        .expect("seed alice observation");

    st_link(&alice, observation_id, alice_old, "supersedes", "alice")
        .await
        .expect("control: the note supersedes an own memory row");
    st_refusal(st_link(&alice, observation_id, bob_old, "supersedes", "alice").await);
    assert!(
        ab_source_namespace_edges(&rt, "alice", vec![bob_old], EdgeRelation::Supersedes)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn supersede_targets_in_local_and_owned_namespaces_are_admitted() {
    let rt = make_runtime();
    let archive = make_registry_with_actor(rt.clone(), "alice:archive");
    let bob = make_registry_with_actor(rt.clone(), "bob");
    let local = rt
        .authorize(Namespace::parse("local").expect("local namespace"))
        .expect("authorize local");
    let alice = st_registry(rt.clone(), "alice", &[], &["alice:archive"]);
    let alice_new = ab_seed(&alice, "bittern call census alice replacement").await;
    let local_old = seed_memory_note(&rt, &local, "bittern call census shared original").await;
    let owned_old = ab_seed(&archive, "bittern call census archived original").await;
    let bob_old = ab_seed(&bob, "bittern call census bob original").await;

    st_link(&alice, alice_new, local_old, "supersedes", "alice")
        .await
        .expect("a shared-namespace target is admitted");
    st_link(&alice, alice_new, owned_old, "supersedes", "alice")
        .await
        .expect("an owned-namespace target is admitted");
    st_refusal(st_link(&alice, alice_new, bob_old, "supersedes", "alice").await);
    assert_eq!(
        ab_source_namespace_edges(
            &rt,
            "alice",
            vec![local_old, owned_old, bob_old],
            EdgeRelation::Supersedes
        )
        .await
        .len(),
        2
    );
}

#[tokio::test]
async fn supersede_refusal_names_the_target_namespace_only_when_readable() {
    let rt = make_runtime();
    let bob = make_registry_with_actor(rt.clone(), "bob");
    let blind = make_registry_with_actor(rt.clone(), "alice");
    let sighted = st_registry(rt.clone(), "alice", &["alice", "bob"], &[]);
    let alice_new = ab_seed(&blind, "plover nesting count alice replacement").await;
    let bob_old = ab_seed(&bob, "plover nesting count bob original").await;

    let hidden = st_refusal(st_link(&blind, alice_new, bob_old, "supersedes", "alice").await);
    assert!(
        hidden.contains("cannot read") && !hidden.contains("namespace bob"),
        "{hidden}"
    );
    // An explicit `namespace` argument narrows the request's visible set to
    // that namespace, so the readable case omits it and writes in `local`. Its
    // source sits in `local` too, so only the superseded row's namespace refuses.
    let local_new = rt
        .create_note(
            &rt.authorize(Namespace::local()).expect("authorize local"),
            "memory",
            None,
            "plover nesting count local replacement",
            Some(0.7),
            Some(json!({"memory_type": "semantic"})),
            vec![],
        )
        .await
        .expect("create local replacement memory")
        .id;
    let named = st_refusal(
        sighted
            .dispatch(
                "link",
                json!({
                    "source_id": local_new.to_string(),
                    "target_id": bob_old.to_string(),
                    "relation": "supersedes"
                }),
            )
            .await,
    );
    assert!(named.contains("is in namespace bob"), "{named}");
}

#[tokio::test]
async fn other_relations_onto_foreign_memory_keep_their_current_rule() {
    let rt = make_runtime();
    let alice = make_registry_with_actor(rt.clone(), "alice");
    let bob = make_registry_with_actor(rt.clone(), "bob");
    let alice_src = ab_seed(&alice, "osprey relation sweep alice source").await;
    let bob_tgt = ab_seed(&bob, "osprey relation sweep bob target").await;
    for relation in ["annotates", "supports", "refutes"] {
        st_link(&alice, alice_src, bob_tgt, relation, "alice")
            .await
            .unwrap_or_else(|error| panic!("{relation} onto a foreign memory: {error}"));
    }
    st_refusal(st_link(&alice, alice_src, bob_tgt, "supersedes", "alice").await);
}
