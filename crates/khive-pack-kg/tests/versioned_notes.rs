use std::collections::HashSet;
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, KhiveRuntime, Namespace, NamespaceToken,
    RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::Note;
use serde_json::{json, Value};

#[derive(Debug)]
struct ListPolicy(bool);
impl Gate for ListPolicy {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(if request.verb == "list" && !self.0 {
            GateDecision::deny("listing denied")
        } else {
            GateDecision::allow()
        })
    }
}

fn fixture(list_allowed: bool) -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(Arc::new(ListPolicy(list_allowed)));
    builder.register(KgPack::new(runtime.clone()));
    (runtime, token, builder.build().unwrap())
}

fn details(error: RuntimeError) -> Value {
    let RuntimeError::Khive(error) = error else {
        panic!("structured error expected, got {error:?}");
    };
    serde_json::to_value(error.details().unwrap()).unwrap()
}

async fn seed(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    kind: &str,
    key: Option<&str>,
    time: i64,
    tags: &[&str],
) -> Note {
    let mut note = Note::new(token.namespace().as_str(), kind, "{}");
    note.key = key.map(str::to_owned);
    note.created_at = time;
    note.updated_at = time;
    note.properties = Some(json!({"tags": tags}));
    runtime
        .notes(token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    note
}

async fn walk(registry: &VerbRegistry, mut args: Value) -> Vec<Value> {
    args["after"] = json!("");
    args["limit"] = json!(1);
    let mut rows = Vec::new();
    for _ in 0..100 {
        let page = registry.dispatch("list", args.clone()).await.unwrap();
        rows.extend(page["notes"].as_array().unwrap().iter().cloned());
        if page["next_after"].is_null() {
            return rows;
        }
        args["after"] = page["next_after"].clone();
    }
    panic!("cursor walk did not terminate");
}

#[tokio::test]
async fn version_head_roundtrip_cas_key_conflicts_and_kind_isolation() {
    let (_, _, registry) = fixture(true);
    let create = json!({"kind":"note", "note_kind":"head", "key":"run/1/lease", "content":"{}", "tags":["kind:lease"]});
    let note = registry.dispatch("create", create.clone()).await.unwrap();
    assert_eq!(note["version"], 1);
    assert_eq!(
        registry
            .dispatch("get", json!({"key":"run/1/lease", "note_kind":"head"}))
            .await
            .unwrap()["id"],
        note["id"]
    );
    // An identical repeat is an exact replay (covered by the keyed_create_c1
    // case); a differing payload under the same key is the conflict.
    let mut differing = create.clone();
    differing["content"] = json!("{\"owner\":0}");
    let error = details(registry.dispatch("create", differing).await.unwrap_err());
    assert_eq!(error["reason"], "key_conflict");
    assert_eq!(error["existing_id"], note["id"]);
    let update = json!({"id":note["id"], "expected_version":1, "content":"{\"owner\":1}"});
    assert_eq!(
        registry.dispatch("update", update.clone()).await.unwrap()["version"],
        2
    );
    let error = details(registry.dispatch("update", update).await.unwrap_err());
    assert_eq!(error["reason"], "version_conflict");
    assert_eq!(error["current_version"], "2");
    registry.dispatch("create", json!({"kind":"observation", "key":"run/1/lease", "content":"same key, different kind", "embed":false})).await.unwrap();
    assert_eq!(
        details(
            registry
                .dispatch("get", json!({"key":"run/1/lease"}))
                .await
                .unwrap_err()
        )["reason"],
        "key_ambiguous"
    );
    for invalid in [
        json!({"kind":"head", "name":"forbidden", "content":"{}"}),
        json!({"kind":"head", "content":"not JSON"}),
        json!({"id":note["id"], "key":"changed"}),
    ] {
        let verb = if invalid.get("id").is_some() {
            "update"
        } else {
            "create"
        };
        assert!(registry.dispatch(verb, invalid).await.is_err());
    }
}

#[tokio::test]
async fn version_key_conflict_disclosure_obeys_second_list_gate() {
    for allow in [true, false] {
        let (_, _, registry) = fixture(allow);
        let args = json!({"kind":"head", "key":"private/key", "content":"{}"});
        let holder = registry.dispatch("create", args.clone()).await.unwrap();
        let mut differing = args.clone();
        differing["content"] = json!("{\"other\":true}");
        let conflict = details(registry.dispatch("create", differing).await.unwrap_err());
        assert_eq!(conflict["reason"], "key_conflict");
        assert_eq!(conflict["key"], "private/key");
        assert_eq!(conflict.get("existing_id"), allow.then_some(&holder["id"]));
    }
}

#[tokio::test]
async fn version_keyed_unicode_prefix_is_literal_and_complete() {
    let (runtime, token, registry) = fixture(true);
    for (prefix, successor) in [
        ("p", "q"),
        ("a\u{10ffff}", "b"),
        ("b\u{d7ff}", "b\u{e000}"),
        ("literal%_", "literal%`"),
    ] {
        let expected = [
            prefix.to_string(),
            format!("{prefix}a"),
            format!("{prefix}\u{10ffff}"),
            format!("{prefix}\u{10ffff}x"),
        ];
        for key in &expected {
            seed(&runtime, &token, "head", Some(key), 10, &[]).await;
        }
        seed(&runtime, &token, "head", Some(successor), 10, &[]).await;
        let rows = walk(&registry, json!({"kind":"note", "key_prefix":prefix})).await;
        let actual: HashSet<_> = rows
            .iter()
            .map(|row| row["key"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(rows.len(), expected.len());
        assert_eq!(actual, HashSet::from(expected));
        assert!(rows.iter().all(|row| row["version"] == 1));
    }
}

#[tokio::test]
async fn version_keyed_ties_after_key_and_filtered_anchor() {
    let (runtime, token, registry) = fixture(true);
    let mut tied = [
        seed(&runtime, &token, "head", Some("same"), 10, &["job", "blue"]).await,
        seed(&runtime, &token, "observation", Some("same"), 10, &["job"]).await,
    ];
    tied.sort_by_key(|note| note.id);
    seed(&runtime, &token, "head", Some("lower"), 9, &["job", "blue"]).await;
    let rows = walk(&registry, json!({"kind":"note", "key_prefix":""})).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["id"], tied[0].id.to_string());
    assert_eq!(rows[1]["id"], tied[1].id.to_string());
    assert_eq!(
        details(
            registry
                .dispatch(
                    "list",
                    json!({"kind":"note", "key_prefix":"", "after_key":"same"})
                )
                .await
                .unwrap_err()
        )["reason"],
        "key_ambiguous"
    );
    assert_eq!(
        details(
            registry
                .dispatch(
                    "list",
                    json!({"kind":"note", "key_prefix":"", "after_key":"missing"})
                )
                .await
                .unwrap_err()
        )["reason"],
        "after_key_missing"
    );
    let outside = seed(
        &runtime,
        &token,
        "head",
        Some("outside-prefix"),
        11,
        &["lease"],
    )
    .await;
    let page = registry.dispatch("list", json!({"kind":"note", "note_kind":"head", "key_prefix":"", "tags":["job"], "after_key":outside.key})).await.unwrap();
    assert_eq!(page["notes"].as_array().unwrap().len(), 2);
    let all = walk(
        &registry,
        json!({"kind":"note", "key_prefix":"", "tags":["JOB","blue"], "tag_mode":"all"}),
    )
    .await;
    let any = walk(
        &registry,
        json!({"kind":"note", "key_prefix":"", "tags":["JOB","blue"], "tag_mode":"any"}),
    )
    .await;
    assert_eq!(all.len(), 2);
    assert_eq!(any.len(), 3);
}

#[tokio::test]
async fn version_time_filters_include_boundary_and_preserve_unkeyed_insertion_walk() {
    let (runtime, token, registry) = fixture(true);
    let first = seed(&runtime, &token, "head", Some("old-updated"), 5, &[]).await;
    let unkeyed = seed(&runtime, &token, "observation", None, 10, &[]).await;
    let newer = seed(&runtime, &token, "head", Some("new"), 10, &[]).await;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(Note {
            updated_at: 11,
            ..first.clone()
        })
        .await
        .unwrap();
    let cutoff = "1970-01-01T00:00:00.000010Z";
    let plain = walk(&registry, json!({"kind":"note", "updated_after":cutoff})).await;
    assert_eq!(
        plain
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            first.id.to_string(),
            unkeyed.id.to_string(),
            newer.id.to_string()
        ]
    );
    assert_eq!(
        walk(&registry, json!({"kind":"note", "created_after":cutoff}))
            .await
            .len(),
        2
    );
    let keyed = walk(
        &registry,
        json!({"kind":"note", "key_prefix":"", "updated_after":cutoff}),
    )
    .await;
    assert_eq!(keyed.len(), 2);
    assert_eq!(keyed[0]["id"], first.id.to_string());
    assert_eq!(keyed[0]["version"], 2);
    assert!(registry
        .dispatch("list", json!({"kind":"note", "updated_after":"not-a-time"}))
        .await
        .is_err());
}

// ADR-172 Amendment 6: identical generic keyed-create replay (C1-C10).

#[derive(Debug)]
struct ListErrorPolicy;
impl Gate for ListErrorPolicy {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "list" {
            Err(GateError::Internal("gate backend unavailable".into()))
        } else {
            Ok(GateDecision::allow())
        }
    }
}

fn fixture_with_gate(gate: Arc<dyn Gate>) -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(gate);
    builder.register(KgPack::new(runtime.clone()));
    (runtime, token, builder.build().unwrap())
}

#[tokio::test]
async fn keyed_create_c1_equal_replay_is_a_minimal_receipt_unkeyed_creates_stay_distinct() {
    let (_, _, registry) = fixture(true);
    let a = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":"import:7", "content":"A", "properties":{"n":1,"x":2}, "embed":false}),
        )
        .await
        .unwrap();
    assert_eq!(a["created"], true);
    let before_version = a["version"].clone();
    let b = registry
        .dispatch(
            "create",
            json!({
                "kind":"observation", "key":"import:7", "content":"A", "properties":{"x":2,"n":1},
                "embed":false, "name":"ignored on replay", "salience":0.9
            }),
        )
        .await
        .unwrap();
    assert_eq!(b, json!({"id": a["id"], "created": false}));
    let after = registry
        .dispatch("get", json!({"id": a["id"]}))
        .await
        .unwrap();
    assert_eq!(after["version"], before_version);

    let u1 = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "content":"no key A", "embed":false}),
        )
        .await
        .unwrap();
    let u2 = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "content":"no key A", "embed":false}),
        )
        .await
        .unwrap();
    assert_ne!(u1["id"], u2["id"]);
}

#[tokio::test]
async fn keyed_create_c2_payload_mismatch_conflicts_and_leaves_holder_unchanged() {
    let base = json!({"kind":"observation", "key":"import:8", "content":"same text", "properties":{"a":1,"b":[1,2]}, "embed":false});
    let mismatches = [
        json!({"kind":"observation", "key":"import:8", "content":"same text ", "properties":{"a":1,"b":[1,2]}, "embed":false}),
        json!({"kind":"observation", "key":"import:8", "content":"same text", "properties":{"a":2,"b":[1,2]}, "embed":false}),
        json!({"kind":"observation", "key":"import:8", "content":"same text", "properties":{"a":"1","b":[1,2]}, "embed":false}),
        json!({"kind":"observation", "key":"import:8", "content":"same text", "properties":{"a":1,"b":[2,1]}, "embed":false}),
        json!({"kind":"observation", "key":"import:8", "content":"same text", "properties":{"a":1,"b":[1,2],"c":3}, "embed":false}),
    ];
    for mismatch in mismatches {
        let (_, _, registry) = fixture(true);
        let holder = registry.dispatch("create", base.clone()).await.unwrap();
        let error = details(
            registry
                .dispatch("create", mismatch.clone())
                .await
                .unwrap_err(),
        );
        assert_eq!(error["reason"], "key_conflict", "mismatch case: {mismatch}");
        assert_eq!(
            error["existing_id"], holder["id"],
            "mismatch case: {mismatch}"
        );
        let after = registry
            .dispatch("get", json!({"id": holder["id"]}))
            .await
            .unwrap();
        assert_eq!(
            after["version"], holder["version"],
            "mismatch case: {mismatch}"
        );
    }
}

#[tokio::test]
async fn keyed_create_c2_absent_properties_differs_from_empty_object() {
    let (_, _, registry) = fixture(true);
    let holder = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":"import:9", "content":"x", "embed":false}),
        )
        .await
        .unwrap();
    let error = details(
        registry
            .dispatch(
                "create",
                json!({"kind":"observation", "key":"import:9", "content":"x", "properties":{}, "embed":false}),
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(error["reason"], "key_conflict");
    assert_eq!(error["existing_id"], holder["id"]);
}

#[tokio::test]
async fn keyed_create_c2_generated_property_variance_is_never_dropped_for_equality() {
    let (runtime, _, registry) = fixture(true);
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter_for_hook = Arc::clone(&counter);
    runtime.install_note_write_validator(Arc::new(move |_kind, _actor, properties| {
        let n = counter_for_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut obj = properties
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        obj.insert("generated_stamp".to_string(), json!(n));
        Ok(Some(Value::Object(obj)))
    }));
    let holder = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":"import:10", "content":"x", "embed":false}),
        )
        .await
        .unwrap();
    let error = details(
        registry
            .dispatch(
                "create",
                json!({"kind":"observation", "key":"import:10", "content":"x", "embed":false}),
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(error["reason"], "key_conflict");
    assert_eq!(error["existing_id"], holder["id"]);
}

#[tokio::test]
async fn keyed_create_c3_disclosure_policies_gate_equal_and_different_conflicts_alike() {
    let policies: Vec<(&str, Arc<dyn Gate>)> = vec![
        ("allow", Arc::new(ListPolicy(true))),
        ("deny", Arc::new(ListPolicy(false))),
        ("error", Arc::new(ListErrorPolicy)),
    ];
    for (label, gate) in policies {
        let (_, _, registry) = fixture_with_gate(gate);
        let create_args = json!({"kind":"head", "key":"disclosure/key", "content":"{}"});
        let holder = registry
            .dispatch("create", create_args.clone())
            .await
            .unwrap();
        assert_eq!(
            holder["created"], true,
            "{label}: first insertion is always allowed by create policy"
        );

        let equal_result = registry.dispatch("create", create_args.clone()).await;
        let different = json!({"kind":"head", "key":"disclosure/key", "content":"{\"x\":1}"});
        let different_error = details(registry.dispatch("create", different).await.unwrap_err());

        if label == "allow" {
            let replay = equal_result.unwrap();
            assert_eq!(replay, json!({"id": holder["id"], "created": false}));
            assert_eq!(different_error["existing_id"], holder["id"]);
        } else {
            let equal_error = details(equal_result.unwrap_err());
            for body in [&equal_error, &different_error] {
                assert_eq!(body["reason"], "key_conflict", "{label}");
                assert!(
                    body.get("existing_id").is_none(),
                    "{label}: no holder id when disclosure is denied or the gate errors: {body:?}"
                );
                assert!(
                    body.get("equal").is_none(),
                    "{label}: the equality bit must never reach a client: {body:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn keyed_create_c4_concurrent_equal_and_differing_races_resolve_deterministically() {
    {
        let (runtime, token, registry) = fixture(true);
        let registry = Arc::new(registry);
        let args =
            json!({"kind":"observation", "key":"race/equal", "content":"same", "embed":false});
        let (r1, r2) = tokio::join!(
            registry.dispatch("create", args.clone()),
            registry.dispatch("create", args.clone())
        );
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();
        let ids: HashSet<_> = [r1["id"].clone(), r2["id"].clone()].into_iter().collect();
        assert_eq!(
            ids.len(),
            1,
            "both racers must agree on the surviving id: {r1:?} {r2:?}"
        );
        let created_true = [&r1, &r2].iter().filter(|r| r["created"] == true).count();
        assert_eq!(
            created_true, 1,
            "exactly one racer sees created:true: {r1:?} {r2:?}"
        );
        let live = runtime
            .notes(&token)
            .unwrap()
            .get_live_notes_by_key(
                token.namespace().as_str(),
                "race/equal",
                Some("observation"),
            )
            .await
            .unwrap();
        assert_eq!(live.len(), 1, "exactly one live holder for the key");
    }
    {
        let (_, _, registry) = fixture(true);
        let registry = Arc::new(registry);
        let a = json!({"kind":"observation", "key":"race/diff", "content":"A", "embed":false});
        let b = json!({"kind":"observation", "key":"race/diff", "content":"B", "embed":false});
        let (ra, rb) = tokio::join!(
            registry.dispatch("create", a),
            registry.dispatch("create", b)
        );
        let successes = [ra.is_ok(), rb.is_ok()]
            .into_iter()
            .filter(|ok| *ok)
            .count();
        assert_eq!(
            successes, 1,
            "exactly one differing-payload racer creates; the other conflicts: {ra:?} {rb:?}"
        );
    }
}

#[tokio::test]
async fn keyed_create_c8_lifetime_soft_and_hard_delete_release_key_restore_refuses() {
    let (_, _, registry) = fixture(true);
    let first = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":"lifetime/k", "content":"v1", "embed":false}),
        )
        .await
        .unwrap();
    registry
        .dispatch("delete", json!({"id": first["id"]}))
        .await
        .unwrap();
    let second = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":"lifetime/k", "content":"v2", "embed":false}),
        )
        .await
        .unwrap();
    assert_ne!(second["id"], first["id"]);
    assert_eq!(second["created"], true);

    let restore_error = details(
        registry
            .dispatch("restore", json!({"id": first["id"], "kind": "observation"}))
            .await
            .unwrap_err(),
    );
    assert_eq!(restore_error["reason"], "restore_key_conflict");
    let second_after = registry
        .dispatch("get", json!({"id": second["id"]}))
        .await
        .unwrap();
    assert_eq!(second_after["version"], second["version"]);

    registry
        .dispatch("delete", json!({"id": second["id"], "hard": true}))
        .await
        .unwrap();
    let third = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":"lifetime/k", "content":"v3", "embed":false}),
        )
        .await
        .unwrap();
    assert_ne!(third["id"], second["id"]);
    assert_eq!(third["created"], true);
}

#[tokio::test]
async fn keyed_create_c9_external_id_collision_under_a_different_key_is_not_a_replay() {
    let (_, _, registry) = fixture(true);
    registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":"dual/k1", "content":"v1", "properties":{"external_id":"E"}, "embed":false}),
        )
        .await
        .unwrap();
    let result = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":"dual/k2", "content":"v2", "properties":{"external_id":"E"}, "embed":false}),
        )
        .await;
    match result {
        Ok(ok) => panic!(
            "an external_id collision under a different key must not succeed as a replay: {ok:?}"
        ),
        Err(RuntimeError::Khive(error)) => {
            if let Some(found) = error.details() {
                let found = serde_json::to_value(found).unwrap();
                assert_ne!(
                    found.get("reason"),
                    Some(&json!("key_conflict")),
                    "a unique-constraint failure on a different property must not be reported as this key's replay"
                );
            }
        }
        Err(_) => {}
    }
}

#[tokio::test]
async fn keyed_create_c10_scope_is_kind_and_namespace_bounded_and_key_length_is_enforced() {
    let (_, _, registry) = fixture(true);

    let obs = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":"scope/k", "content":"o", "embed":false}),
        )
        .await
        .unwrap();
    let insight = registry
        .dispatch(
            "create",
            json!({"kind":"insight", "key":"scope/k", "content":"i", "embed":false}),
        )
        .await
        .unwrap();
    assert_ne!(obs["id"], insight["id"]);
    assert_eq!(insight["created"], true);

    // Writes pin to the default namespace unless the request names one
    // explicitly, so the second namespace is selected with `namespace`.
    let other_ns = registry
        .dispatch(
            "create",
            json!({"namespace":"other", "kind":"observation", "key":"scope/k", "content":"o2", "embed":false}),
        )
        .await
        .unwrap();
    assert_ne!(other_ns["id"], obs["id"]);
    assert_eq!(other_ns["created"], true);

    assert!(registry
        .dispatch(
            "create",
            json!({"kind":"concept", "name":"x", "key":"nope"}),
        )
        .await
        .is_err());

    assert!(registry
        .dispatch(
            "create",
            json!({"kind":"observation", "content":"x", "external_id":"nope"}),
        )
        .await
        .is_err());

    let ok_key = "k".repeat(512);
    let ok = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":ok_key, "content":"len-ok", "embed":false}),
        )
        .await
        .unwrap();
    assert_eq!(ok["created"], true);

    let too_long = "k".repeat(513);
    assert!(registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":too_long, "content":"len-bad", "embed":false}),
        )
        .await
        .is_err());

    assert!(registry
        .dispatch(
            "create",
            json!({"kind":"observation", "key":"has\u{0000}null", "content":"nul", "embed":false}),
        )
        .await
        .is_err());
}

// The `equal` replay-comparison signal (ADR-172 Amendment 6) is opt-in per
// caller: only the singleton `create` route asks for it. stream.batch's
// keyed write member reaches the same shared conflict-classification code
// with an identical payload and must never see it, in either run mode.

#[tokio::test]
async fn keyed_create_c11_stream_batch_per_member_equal_conflict_never_discloses_equal() {
    let (_, _, registry) = fixture(false);
    let ops = json!([{"op":"write","kind":"observation","key":"stream/keyed-a","doc":{"n":1}}]);
    let first = registry
        .dispatch("stream.batch", json!({"ops": ops}))
        .await
        .unwrap();
    assert!(
        first["results"][0]["id"].is_string(),
        "first write must create the holder: {first}"
    );

    let second = registry
        .dispatch("stream.batch", json!({"ops": ops}))
        .await
        .unwrap();
    let member = &second["results"][0];
    let details = &member["details"];
    assert_eq!(details["reason"], "key_conflict", "member: {member}");
    assert_eq!(details["key"], "stream/keyed-a", "member: {member}");
    assert!(
        details.get("equal").is_none(),
        "a per-member key_conflict must never carry equal: {member}"
    );
    assert!(
        details.get("existing_id").is_none(),
        "existing_id stays withheld when list is denied: {member}"
    );
}

#[tokio::test]
async fn keyed_create_c12_stream_batch_atomic_equal_conflict_matches_todays_key_conflict_shape() {
    let (_, _, registry) = fixture(true);
    let ops = json!([{"op":"write","kind":"observation","key":"stream/keyed-b","doc":{"n":1}}]);
    let first = registry
        .dispatch("stream.batch", json!({"ops": ops, "atomic": true}))
        .await
        .unwrap();
    assert!(
        first["results"][0]["id"].is_string(),
        "first write must create the holder: {first}"
    );

    let error = details(
        registry
            .dispatch("stream.batch", json!({"ops": ops, "atomic": true}))
            .await
            .unwrap_err(),
    );
    assert_eq!(error["reason"], "key_conflict");
    assert_eq!(error["key"], "stream/keyed-b");
    assert!(
        error.get("existing_id").is_some(),
        "list is allowed so the holder is disclosed: {error}"
    );
    assert!(
        error.get("member").is_some(),
        "atomic-mode key_conflict names its member index: {error}"
    );
    let keys: std::collections::BTreeSet<&str> = error
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["reason", "key", "existing_id", "member"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        "an allowed-disclosure atomic key_conflict carries exactly today's fields, no equal: {error}"
    );
}
