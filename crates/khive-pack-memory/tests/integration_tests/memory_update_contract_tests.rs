// ---------------------------------------------------------------------------
// The generic property-update path for `memory` notes.
//
// `memory.remember` validates `memory_type` against a closed set and derives
// `salience` and `decay_factor` from it. These pin the second writer to the
// same predicate, and pin the derived pair to the label it was derived from.
// ---------------------------------------------------------------------------

/// Write one memory through the validating writer and return its stored row.
async fn remembered(registry: &VerbRegistry, extra: Value) -> Value {
    let mut args = json!({
        "content": "a memory written by the validating writer, for an update test",
        "memory_type": "episodic",
    });
    if let (Some(root), Some(extra)) = (args.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            root.insert(key.clone(), value.clone());
        }
    }
    registry
        .dispatch("memory.remember", args)
        .await
        .expect("memory.remember with a valid memory_type must succeed")
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_update_rejects_a_memory_type_outside_the_closed_set() {
    let rt = make_runtime();
    let registry = make_registry(rt);
    let created = remembered(&registry, json!({})).await;

    let err = registry
        .dispatch(
            "update",
            json!({"id": created["id"], "properties": {"memory_type": "NOT-A-TYPE"}}),
        )
        .await
        .expect_err("the generic update path must refuse a memory_type outside the closed set");
    let msg = err.to_string();
    assert!(
        msg.contains("episodic | semantic"),
        "the error must name the valid types, got: {msg}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_update_refuses_to_clear_memory_type() {
    let rt = make_runtime();
    let registry = make_registry(rt);
    let created = remembered(&registry, json!({})).await;

    let err = registry
        .dispatch(
            "update",
            json!({"id": created["id"], "properties": {"memory_type": null}}),
        )
        .await
        .expect_err("clearing memory_type would leave two derived numbers describing nothing");
    let msg = err.to_string();
    assert!(
        msg.contains("cannot be cleared"),
        "the error must say the field cannot be cleared, got: {msg}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_update_moves_the_derived_pair_when_the_type_changes() {
    let rt = make_runtime();
    let registry = make_registry(rt);
    let created = remembered(&registry, json!({})).await;
    assert_eq!(
        created["salience"], 0.3,
        "episodic starts at the episodic default"
    );
    assert_eq!(created["decay_factor"], 0.02);

    let updated = registry
        .dispatch(
            "update",
            json!({"id": created["id"], "properties": {"memory_type": "semantic"}}),
        )
        .await
        .expect("promoting a memory to semantic must be accepted");

    assert_eq!(
        updated["properties"]["memory_type"], "semantic",
        "the label must move"
    );
    assert_eq!(
        updated["salience"], 0.5,
        "and the ranking weight derived from it must move with it"
    );
    assert_eq!(
        updated["decay_factor"], 0.005,
        "and so must the ageing rate"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_update_keeps_a_salience_the_caller_chose() {
    let rt = make_runtime();
    let registry = make_registry(rt);
    // 0.9 is not either type's default, so it can only have been chosen deliberately.
    let created = remembered(&registry, json!({"salience": 0.9})).await;
    assert_eq!(created["salience"], 0.9);

    let updated = registry
        .dispatch(
            "update",
            json!({"id": created["id"], "properties": {"memory_type": "semantic"}}),
        )
        .await
        .expect("a type change on a hand-weighted memory must still be accepted");

    assert_eq!(
        updated["salience"], 0.9,
        "a deliberate weight survives a type change; only a derived one is replaced"
    );
    assert_eq!(
        updated["decay_factor"], 0.005,
        "the ageing rate was still derived, so it moves"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_update_keeps_a_salience_named_in_the_same_patch() {
    let rt = make_runtime();
    let registry = make_registry(rt);
    let created = remembered(&registry, json!({})).await;

    let updated = registry
        .dispatch(
            "update",
            json!({
                "id": created["id"],
                "salience": 0.42,
                "properties": {"memory_type": "semantic"},
            }),
        )
        .await
        .expect("naming salience beside the type change must be accepted");

    assert_eq!(
        updated["salience"], 0.42,
        "what the caller states in this patch wins over what is derived for them"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_update_naming_the_same_type_changes_nothing() {
    let rt = make_runtime();
    let registry = make_registry(rt);
    let created = remembered(&registry, json!({"salience": 0.9})).await;

    let updated = registry
        .dispatch(
            "update",
            json!({"id": created["id"], "properties": {"memory_type": "episodic"}}),
        )
        .await
        .expect("naming the type a memory already has must be accepted");

    assert_eq!(
        updated["salience"], 0.9,
        "nothing changed, so nothing is re-derived"
    );
    assert_eq!(updated["decay_factor"], 0.02);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_update_naming_the_same_type_invents_nothing_for_unset_fields() {
    // A memory row can carry no salience and no decay: the pack's own writer is what fills those
    // in. Naming the type it already has must not quietly populate them, because an unset field and
    // a field holding the default are different states and only the caller can decide which one
    // this row is in.
    //
    // The fixture is written at the store layer rather than through `create`, and that is the point
    // of it: since #2684 no verb produces this shape, so the rows that have it are the ones already
    // in a database, written before the generic path was closed. This arm exists for exactly those
    // rows, and building it through a verb would mean it no longer represents any of them.
    let rt = make_runtime();
    let registry = make_registry(rt.clone());
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let created = rt
        .create_note(
            &token,
            "memory",
            None,
            "a memory row stored without the fields the pack's writer derives",
            None,
            Some(json!({"memory_type": "episodic"})),
            Vec::new(),
        )
        .await
        .expect("writing a memory row at the store layer must succeed");
    assert!(
        created.salience.is_none(),
        "precondition: this row holds no salience, got {:?}",
        created.salience
    );
    assert!(
        created.decay_factor.is_none(),
        "precondition: this row holds no decay factor, got {:?}",
        created.decay_factor
    );

    let updated = registry
        .dispatch(
            "update",
            json!({"id": created.id.to_string(), "properties": {"memory_type": "episodic"}}),
        )
        .await
        .expect("naming the type it already has must be accepted");

    assert!(
        updated["salience"].is_null(),
        "nothing changed, so nothing is derived into a field the caller never set, got {}",
        updated["salience"]
    );
    assert!(
        updated["decay_factor"].is_null(),
        "and the same for the ageing rate, got {}",
        updated["decay_factor"]
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_update_not_naming_the_type_leaves_the_derived_pair_alone() {
    let rt = make_runtime();
    let registry = make_registry(rt);
    let created = remembered(&registry, json!({})).await;

    let updated = registry
        .dispatch(
            "update",
            json!({"id": created["id"], "properties": {"source": "an unrelated property"}}),
        )
        .await
        .expect("an update naming no memory_type must be accepted");

    assert_eq!(updated["salience"], 0.3);
    assert_eq!(updated["decay_factor"], 0.02);
    assert_eq!(updated["properties"]["memory_type"], "episodic");
}

/// Issue #2684. `create(kind="note", note_kind="memory", …)` used to store a row with
/// `salience`, `decay_factor` and `properties.memory_type` all unset, while `memory.recall`
/// rendered that same row as episodic at the default salience: one record reading one way from
/// storage and another through the verb that exists to read it. Per ADR-021's creation-admission
/// amendment this pack's own `KindHook::prepare_create` refuses the kind and names the writer that
/// owns it, so one refusal answers both admitting paths — shared `create` and a `stream.batch`
/// member — without the generic pack knowing this kind exists.
#[tokio::test]
async fn generic_create_refuses_the_memory_kind_and_names_its_writer() {
    let registry = make_registry(make_runtime());

    // An ordinary note kind still creates through the same request shape, so the refusal below
    // is attributable to the kind rather than to how the request was written.
    registry
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "observation",
                "content": "an ordinary note created through the generic verb"
            }),
        )
        .await
        .expect("the same request shape must still create an ordinary note");

    let refusal = registry
        .dispatch(
            "create",
            json!({
                "kind": "note",
                "note_kind": "memory",
                "content": "a memory written through the generic create verb"
            }),
        )
        .await
        .expect_err("kind=memory must be refused by generic create");
    let message = refusal.to_string();
    assert!(
        message.contains("memory.remember"),
        "a refusal has to name the verb that does the job: {message}"
    );

    // The kind is not disabled, only its entry point. The writer still creates, and the row it
    // writes carries the three fields whose absence was the defect.
    let created = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "a memory written through the verb that owns the kind" }),
        )
        .await
        .expect("memory.remember must still create a memory");
    let id = created["id"].as_str().expect("remember returns an id");

    let read_back = registry
        .dispatch("get", json!({ "id": id }))
        .await
        .expect("the created memory must be readable");
    let record = read_back.get("record").unwrap_or(&read_back);
    assert!(
        record["salience"].is_number(),
        "salience must be set at write time, not derived at read time: {record}"
    );
    assert!(
        record["decay_factor"].is_number(),
        "decay_factor must be set at write time: {record}"
    );
    assert_eq!(
        record["properties"]["memory_type"], "episodic",
        "the stored row must name its own memory_type: {record}"
    );
}
