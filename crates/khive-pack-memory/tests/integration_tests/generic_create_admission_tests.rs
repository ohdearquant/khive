// ── ADR-021 creation-admission amendment: acceptance arms (#2684) ──────────
//
// Source: docs/adr/ADR-021-memory-pack.md, "Amendment: new-memory creation
// admission", acceptance sentence. `generic_create_refuses_the_memory_kind_and_names_its_writer`
// above already covers the `note_kind="memory"` spelling and the observation-kind
// control (the amendment's C1).

/// Both generic spellings refuse. This covers `create(kind="memory", ...)`; the
/// existing arm above covers `create(kind="note", note_kind="memory", ...)`.
#[tokio::test]
async fn generic_create_refuses_the_memory_kind_via_its_direct_spelling() {
    let registry = make_registry(make_runtime());

    let refusal = registry
        .dispatch(
            "create",
            json!({
                "kind": "memory",
                "content": "a memory written through the direct kind spelling"
            }),
        )
        .await
        .expect_err("create(kind=\"memory\", ...) must be refused");
    assert!(
        refusal.to_string().contains("memory.remember"),
        "a refusal has to name the verb that does the job: {refusal}"
    );
}

/// Refusal changes nothing: notes, edges and key holders are unchanged. The
/// control (mine, not the amendment's): `stats()` is read by the same call
/// before and after a known-positive write, so a counter that never moves
/// cannot pass the later assertion for free.
#[tokio::test]
async fn generic_create_refusal_of_the_memory_kind_changes_no_notes_edges_or_key_holders() {
    let registry = make_registry(make_runtime());

    // A pre-existing key holder, under an ordinary kind, that the refused
    // request below targets by the SAME key.
    let holder = registry
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "key": "held note",
                "content": "a pre-existing key holder"
            }),
        )
        .await
        .expect("seed a keyed observation note");
    let holder_id = holder["id"].clone();

    let notes_before = registry.dispatch("stats", json!({})).await.unwrap()["notes"]
        .as_u64()
        .expect("stats().notes is a number");
    let edges_before = registry.dispatch("stats", json!({})).await.unwrap()["edges"]
        .as_u64()
        .expect("stats().edges is a number");

    // Control: a known-positive write must move the same instrument the
    // refusal check below reads, or a counter that always answers one number
    // would pass that check trivially.
    registry
        .dispatch(
            "create",
            json!({"kind": "note", "note_kind": "observation", "content": "a control write"}),
        )
        .await
        .expect("the control write must succeed");
    let notes_after_control = registry.dispatch("stats", json!({})).await.unwrap()["notes"]
        .as_u64()
        .unwrap();
    assert_eq!(
        notes_after_control,
        notes_before + 1,
        "the note-count instrument must be live: a known-positive write must move it"
    );

    // The same control for the edge counter the refusal is asserted against
    // below. `create` has no edge-writing parameter at all, so the control is
    // an explicit `link`, in the entity-to-entity shape the kg pack's own
    // passing tests use. What it proves is narrow and worth stating plainly:
    // `stats().edges` is a live counter rather than one frozen at a constant,
    // so the "no edge" assertion below is read from an instrument that can
    // move — it is not a claim that a refused `create` had a route to an edge.
    let base_concept = registry
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "EdgeControlBase"}),
        )
        .await
        .expect("the base concept must be created");
    let derived_concept = registry
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "EdgeControlDerived"}),
        )
        .await
        .expect("the derived concept must be created");
    registry
        .dispatch(
            "link",
            json!({
                "source_id": derived_concept["id"],
                "target_id": base_concept["id"],
                "relation": "extends"
            }),
        )
        .await
        .expect("the edge control write must succeed");
    let edges_after_control = registry.dispatch("stats", json!({})).await.unwrap()["edges"]
        .as_u64()
        .unwrap();
    assert_eq!(
        edges_after_control,
        edges_before + 1,
        "the edge-count instrument must be live: an admitted link must move it"
    );
    let notes_after_control = registry.dispatch("stats", json!({})).await.unwrap()["notes"]
        .as_u64()
        .unwrap();

    let refusal = registry
        .dispatch(
            "create",
            json!({
                "kind": "memory",
                "key": "held note",
                "content": "a refused memory write aimed at an existing key"
            }),
        )
        .await
        .expect_err("kind=memory must refuse even a keyed request against a live holder");
    assert!(refusal.to_string().contains("memory.remember"));

    let notes_after_refusal = registry.dispatch("stats", json!({})).await.unwrap()["notes"]
        .as_u64()
        .unwrap();
    assert_eq!(
        notes_after_refusal, notes_after_control,
        "a refused create must not add a note"
    );

    let edges_after_refusal = registry.dispatch("stats", json!({})).await.unwrap()["edges"]
        .as_u64()
        .unwrap();
    assert_eq!(
        edges_after_refusal, edges_after_control,
        "a refused create must not add an edge"
    );

    let still_holds = registry
        .dispatch("get", json!({"key": "held note", "kind": "observation"}))
        .await
        .expect("the pre-existing key holder must still resolve");
    let still_holds = still_holds.get("record").unwrap_or(&still_holds);
    assert_eq!(
        still_holds["id"], holder_id,
        "the refused create must not disturb the existing key holder"
    );
}

/// Keyed, explicit-namespace and defaults-supplied requests refuse too. The
/// third case is the interesting one: a caller who names exactly the fields
/// `memory.remember` would have derived is refused anyway — the refusal is
/// about the contract, not about which fields happen to be present.
#[tokio::test]
async fn generic_create_refuses_keyed_explicit_namespace_and_self_supplied_default_requests() {
    let registry = make_registry(make_runtime());

    let keyed = registry
        .dispatch(
            "create",
            json!({"kind": "memory", "key": "keyed memory", "content": "a keyed request"}),
        )
        .await
        .expect_err("a keyed create must refuse the memory kind too");
    assert!(keyed.to_string().contains("memory.remember"));

    let namespaced = registry
        .dispatch(
            "create",
            json!({
                "kind": "memory",
                "namespace": "acceptance-2684-ns",
                "content": "an explicit-namespace request"
            }),
        )
        .await
        .expect_err("an explicit-namespace create must refuse the memory kind too");
    assert!(namespaced.to_string().contains("memory.remember"));

    let self_supplied_defaults = registry
        .dispatch(
            "create",
            json!({
                "kind": "memory",
                "content": "a request supplying its own derived-looking fields",
                "salience": 0.3,
                "properties": {"memory_type": "episodic"}
            }),
        )
        .await
        .expect_err("a request supplying its own derived defaults must still refuse");
    assert!(self_supplied_defaults
        .to_string()
        .contains("memory.remember"));
}
