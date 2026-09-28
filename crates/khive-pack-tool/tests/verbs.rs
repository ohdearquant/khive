//! Acceptance arms of ADR-180, driven through the verb registry.

use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistry, VerbRegistryBuilder};
use khive_storage::Entity;
use serde_json::{json, Value};

struct Fixture {
    rt: KhiveRuntime,
    registry: VerbRegistry,
}

fn fixture() -> Fixture {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(ToolPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture { rt, registry }
}

impl Fixture {
    async fn call(&self, verb: &str, params: Value) -> Value {
        self.registry
            .dispatch(verb, params)
            .await
            .unwrap_or_else(|e| panic!("{verb} failed: {e}"))
    }

    async fn call_err(&self, verb: &str, params: Value) -> String {
        match self.registry.dispatch(verb, params).await {
            Ok(v) => panic!("{verb} unexpectedly succeeded: {v}"),
            Err(e) => e.to_string(),
        }
    }
}

fn s(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string {key} in {v}"))
        .to_string()
}

// Arm 1: register creates one entity with the properties and tags; a repeat returns the same id.
#[tokio::test]
async fn register_is_idempotent_by_name() {
    let f = fixture();
    let first = f
        .call(
            "tool.register",
            json!({
                "name": "fetch_url",
                "description": "Fetch a URL and return the body",
                "side_effect": "egress",
                "trust": "marketplace",
                "source": "mcp:web",
                "schema": {"type": "object", "properties": {"url": {"type": "string"}}},
            }),
        )
        .await;
    assert_eq!(first["created"], json!(true));
    let tool = &first["tool"];
    assert_eq!(s(tool, "kind"), "tool");
    assert_eq!(s(tool, "side_effect"), "egress");
    assert_eq!(s(tool, "trust"), "marketplace");
    assert_eq!(s(tool, "source"), "mcp:web");
    let tags = tool["tags"].as_array().expect("tags");
    assert!(tags.contains(&json!("tool-registry")) && tags.contains(&json!("tool")));

    let again = f
        .call(
            "tool.register",
            json!({"name": "fetch_url", "kind": "tool"}),
        )
        .await;
    assert_eq!(again["created"], json!(false));
    assert_eq!(again["tool"]["full_id"], tool["full_id"]);

    let listed = f.call("tool.list", json!({})).await;
    assert_eq!(listed["count"], json!(1));
}

// Arm 2: capabilities are created once and linked with implements edges.
#[tokio::test]
async fn capabilities_are_created_once_and_linked() {
    let f = fixture();
    let a = f
        .call(
            "tool.register",
            json!({"name": "fetch_url", "capabilities": ["web browsing", "http"]}),
        )
        .await;
    assert_eq!(a["capabilities"].as_array().unwrap().len(), 2);
    let b = f
        .call(
            "tool.register",
            json!({"name": "curl", "capabilities": ["http"]}),
        )
        .await;
    assert_eq!(
        b["capabilities"][0]["id"], a["capabilities"][1]["id"],
        "http concept reused"
    );

    let described = f.call("tool.describe", json!({"tool": "fetch_url"})).await;
    let caps: Vec<String> = described["tool"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| s(c, "name"))
        .collect();
    assert_eq!(caps.len(), 2);
    assert!(caps.contains(&"web browsing".to_string()) && caps.contains(&"http".to_string()));
    assert_eq!(described["tool"]["decision"]["decision"], json!("ask"));
}

#[tokio::test]
async fn registration_reuses_capability_after_more_than_five_thousand_newer_names() {
    let f = fixture();
    let original = f
        .call(
            "tool.register",
            json!({"name": "first", "capabilities": ["HTTP"]}),
        )
        .await;
    let original_id = original["capabilities"][0]["id"].clone();
    let token = f.rt.authorize(Namespace::local()).unwrap();
    let fillers: Vec<Entity> = (0..5000)
        .map(|index| {
            Entity::new("local", "concept", format!("filler-{index:05}"))
                .with_entity_type(Some("capability"))
                .with_tags(vec!["tool-capability".into()])
        })
        .collect();
    let seeded =
        f.rt.entities(&token)
            .unwrap()
            .upsert_entities(fillers)
            .await
            .unwrap();
    assert_eq!(seeded.affected, 5000, "{seeded:?}");
    assert_eq!(seeded.failed, 0, "{seeded:?}");

    let reused = f
        .call(
            "tool.register",
            json!({"name": "second", "capabilities": ["http"]}),
        )
        .await;
    assert_eq!(reused["capabilities"][0]["id"], original_id);
    assert_eq!(
        f.rt.count_entities_tagged(&token, Some("concept"), Some("tool-capability"))
            .await
            .unwrap(),
        5001,
        "the second registration must not create a duplicate capability"
    );
}

// Arm 3: a need phrased in capability words finds a tool registered under another name;
// the same tool without the capability is not found by that phrasing (mutation control).
#[tokio::test]
async fn suggest_reaches_tools_through_capabilities() {
    let with = fixture();
    with.call(
        "tool.register",
        json!({
            "name": "fetch_url",
            "description": "Fetch a URL and return the body",
            "capabilities": ["web browsing"],
        }),
    )
    .await;
    let hits = with
        .call("tool.suggest", json!({"query": "web browsing"}))
        .await;
    let results = hits["results"].as_array().expect("results");
    let hit = results
        .iter()
        .find(|r| r["name"] == json!("fetch_url"))
        .unwrap_or_else(|| panic!("fetch_url not suggested: {hits}"));
    assert!(
        hit["via"]
            .as_array()
            .unwrap()
            .contains(&json!("web browsing")),
        "hit must name the capability it was reached through: {hit}"
    );
    assert!(hit["decision"]["decision"].is_string());

    let without = fixture();
    without
        .call(
            "tool.register",
            json!({"name": "fetch_url", "description": "Fetch a URL and return the body"}),
        )
        .await;
    let hits = without
        .call("tool.suggest", json!({"query": "web browsing"}))
        .await;
    let found = hits["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == json!("fetch_url"));
    assert!(
        !found,
        "control: without the capability the phrasing must not reach it: {hits}"
    );
}

// Arm 4: default and policy resolution order.
#[tokio::test]
async fn check_resolves_default_then_policy_by_specificity() {
    let f = fixture();
    let unknown = f
        .call("tool.check", json!({"tool": "nope", "actor": "agent:a"}))
        .await;
    assert_eq!(unknown["decision"], json!("ask"));
    assert_eq!(unknown["source"], json!("default"));
    assert_eq!(unknown["registered"], json!(false));

    f.call(
        "tool.register",
        json!({"name": "read_file", "side_effect": "read"}),
    )
    .await;
    let read = f
        .call(
            "tool.check",
            json!({"tool": "read_file", "actor": "agent:a"}),
        )
        .await;
    assert_eq!(read["decision"], json!("allow"));
    assert_eq!(read["source"], json!("default"));
    assert_eq!(read["registered"], json!(true));

    let deny_all = f
        .call(
            "tool.policy",
            json!({"actor": "agent:*", "tool": "*", "decision": "deny"}),
        )
        .await;
    let denied = f
        .call(
            "tool.check",
            json!({"tool": "read_file", "actor": "agent:a"}),
        )
        .await;
    assert_eq!(denied["decision"], json!("deny"));
    assert_eq!(denied["source"], json!("policy"));
    assert_eq!(denied["policy_id"], deny_all["policy"]["id"]);

    f.call(
        "tool.policy",
        json!({"actor": "agent:a", "tool": "read_file", "decision": "allow"}),
    )
    .await;
    let allowed = f
        .call(
            "tool.check",
            json!({"tool": "read_file", "actor": "agent:a"}),
        )
        .await;
    assert_eq!(allowed["decision"], json!("allow"));
    assert_eq!(allowed["source"], json!("policy"));

    let other = f
        .call(
            "tool.check",
            json!({"tool": "read_file", "actor": "agent:b"}),
        )
        .await;
    assert_eq!(
        other["decision"],
        json!("deny"),
        "exact rule is scoped to its actor"
    );

    let policies = f.call("tool.policies", json!({})).await;
    assert_eq!(policies["count"], json!(2));
}

// Arm 5: request, grant, revoke.
#[tokio::test]
async fn request_grant_revoke_cycle() {
    let f = fixture();
    f.call(
        "tool.register",
        json!({"name": "send_mail", "side_effect": "egress"}),
    )
    .await;
    let req = f
        .call(
            "tool.request",
            json!({"tool": "send_mail", "actor": "agent:a", "reason": "notify the owner"}),
        )
        .await;
    assert_eq!(req["decision"], json!("ask"));
    assert_eq!(req["status"], json!("requested"));
    let id = s(&req, "request_id");

    let pending = f
        .call("tool.requests", json!({"status": "requested"}))
        .await;
    assert_eq!(pending["count"], json!(1));

    let granted = f.call("tool.grant", json!({"id": id})).await;
    assert_eq!(granted["grant"]["status"], json!("granted"));
    let check = f
        .call(
            "tool.check",
            json!({"tool": "send_mail", "actor": "agent:a"}),
        )
        .await;
    assert_eq!(check["decision"], json!("allow"));
    assert_eq!(check["source"], json!("grant"));
    assert_eq!(check["grant_id"], json!(id));

    let fast = f
        .call(
            "tool.request",
            json!({"tool": "send_mail", "actor": "agent:a"}),
        )
        .await;
    assert_eq!(fast["decision"], json!("allow"));
    assert!(fast["request_id"].is_null(), "fast path inserts no row");

    f.call("tool.revoke", json!({"id": id})).await;
    let after = f
        .call(
            "tool.check",
            json!({"tool": "send_mail", "actor": "agent:a"}),
        )
        .await;
    assert_eq!(after["decision"], json!("ask"));
    assert_eq!(after["source"], json!("default"));
}

// Arm 6: an expired grant is not active.
#[tokio::test]
async fn expired_grant_is_ignored() {
    let f = fixture();
    f.call("tool.register", json!({"name": "send_mail"})).await;
    let req = f
        .call(
            "tool.request",
            json!({"tool": "send_mail", "actor": "agent:a"}),
        )
        .await;
    let id = s(&req, "request_id");
    f.call("tool.grant", json!({"id": id, "expires_in_s": 0}))
        .await;
    let check = f
        .call(
            "tool.check",
            json!({"tool": "send_mail", "actor": "agent:a"}),
        )
        .await;
    assert_eq!(check["decision"], json!("ask"), "{check}");
    assert_eq!(check["source"], json!("default"));
}

// Arm 7: an illegal transition is refused with the current status in the message.
#[tokio::test]
async fn deny_on_revoked_is_refused() {
    let f = fixture();
    f.call("tool.register", json!({"name": "send_mail"})).await;
    let req = f
        .call(
            "tool.request",
            json!({"tool": "send_mail", "actor": "agent:a"}),
        )
        .await;
    let id = s(&req, "request_id");
    f.call("tool.grant", json!({"id": id})).await;
    f.call("tool.revoke", json!({"id": id})).await;
    let err = f.call_err("tool.deny", json!({"id": id})).await;
    assert!(
        err.contains("revoked"),
        "message names the current status: {err}"
    );
    let err = f.call_err("tool.revoke", json!({"id": id})).await;
    assert!(err.contains("revoked"), "{err}");
}

// Arm 8: ingest registers khive's loaded verbs, one capability per pack; a rerun is all existing.
#[tokio::test]
async fn ingest_khive_verbs_once() {
    let f = fixture();
    let first = f.call("tool.ingest", json!({"source": "khive"})).await;
    let registered = first["registered"].as_u64().expect("registered");
    assert!(registered > 0, "{first}");
    assert_eq!(first["existing"], json!(0));

    let again = f.call("tool.ingest", json!({"source": "khive"})).await;
    assert_eq!(again["registered"], json!(0));
    assert_eq!(again["existing"], json!(registered));

    let verbs = f
        .call("tool.list", json!({"kind": "verb", "limit": 1000}))
        .await;
    assert_eq!(verbs["count"], json!(registered));
    let described = f.call("tool.describe", json!({"tool": "tool.check"})).await;
    assert_eq!(described["tool"]["source"], json!("khive:tool"));
    assert_eq!(described["tool"]["side_effect"], json!("read"));
    let caps: Vec<String> = described["tool"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| s(c, "name"))
        .collect();
    assert_eq!(caps, vec!["tool".to_string()]);
}

// Arm 9: a requester cannot grant its own request; another actor's request can be granted.
#[tokio::test]
async fn self_grant_is_refused() {
    let f = fixture();
    f.call("tool.register", json!({"name": "send_mail"})).await;
    let own = f.call("tool.request", json!({"tool": "send_mail"})).await;
    let id = s(&own, "request_id");
    let err = f.call_err("tool.grant", json!({"id": id})).await;
    assert!(err.contains("own request"), "{err}");
    let still = f
        .call("tool.requests", json!({"status": "requested"}))
        .await;
    assert_eq!(still["count"], json!(1));

    let other = f
        .call(
            "tool.request",
            json!({"tool": "send_mail", "actor": "agent:z"}),
        )
        .await;
    let id2 = s(&other, "request_id");
    let granted = f.call("tool.grant", json!({"id": id2})).await;
    assert_eq!(granted["grant"]["status"], json!("granted"));
}

#[tokio::test]
async fn pattern_request_cannot_self_grant() {
    let f = fixture();
    f.call("tool.register", json!({"name": "send_mail"})).await;
    let own_actor = s(
        &f.call("tool.check", json!({"tool": "send_mail"})).await,
        "actor",
    );

    for actor in ["*".to_string(), format!("{own_actor}*")] {
        let request = f
            .call("tool.request", json!({"tool": "send_mail", "actor": actor}))
            .await;
        let id = s(&request, "request_id");
        let err = f.call_err("tool.grant", json!({"id": &id})).await;
        assert!(err.contains("own request"), "{err}");
        let still = f
            .call("tool.requests", json!({"status": "requested"}))
            .await;
        assert!(still["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"].as_str() == Some(id.as_str())));
        let decision = f.call("tool.check", json!({"tool": "send_mail"})).await;
        assert_eq!(decision["decision"], json!("ask"));
        assert_eq!(decision["source"], json!("default"));
    }
}

// Arm 10: a registry row is opaque to the generic entity verbs. `update` and
// `delete` refuse a row whose current tags carry the registry tag, and the
// refusal reads those tags before the write, so stripping the tag is itself
// refused. The pack's own verbs are unaffected.
#[tokio::test]
async fn registry_rows_are_opaque_to_the_generic_entity_verbs() {
    let f = fixture();
    let registered = f
        .call(
            "tool.register",
            json!({
                "name": "fetch_url",
                "side_effect": "read",
                "trust": "first_party",
                "source": "mcp:web@1",
            }),
        )
        .await;
    let id = s(&registered["tool"], "full_id");

    // The measured hole: moving `properties.source` under a registered name.
    let err = f
        .call_err(
            "update",
            json!({"id": id, "properties": {"source": "mcp:evil@1"}}),
        )
        .await;
    assert!(
        err.contains("tool-registry"),
        "the refusal names the tag it read: {err}"
    );
    assert!(
        err.contains("registering a new name"),
        "the refusal says what to do instead: {err}"
    );

    // Strip-then-edit: the strip is the same write, read against the same
    // pre-write tags, so it is refused by the same rule.
    let err = f.call_err("update", json!({"id": id, "tags": []})).await;
    assert!(err.contains("tool-registry"), "{err}");
    let after_strip = f.call("tool.describe", json!({"tool": "fetch_url"})).await;
    let tags = after_strip["tool"]["tags"].as_array().expect("tags");
    assert!(
        tags.contains(&json!("tool-registry")),
        "the refusal is read before the write, so the tag is still there: {after_strip}"
    );

    let err = f.call_err("delete", json!({"id": id})).await;
    assert!(err.contains("tool-registry"), "{err}");
    assert!(err.contains("delete refuses"), "{err}");

    // Nothing moved: the row still answers with its registered source, and the
    // pack's own verb still writes it.
    let described = f.call("tool.describe", json!({"tool": "fetch_url"})).await;
    assert_eq!(described["tool"]["source"], json!("mcp:web@1"));
    let again = f
        .call(
            "tool.register",
            json!({"name": "fetch_url", "capabilities": ["http"]}),
        )
        .await;
    assert_eq!(again["created"], json!(false));
    assert_eq!(again["tool"]["full_id"], json!(id));
}

// Arm 11 (control): the rule keys on the registry tag, not on the entity kind
// or the pack that is loaded. An untagged entity of the same kind still
// updates and still deletes.
#[tokio::test]
async fn an_untagged_entity_still_updates_and_deletes() {
    let f = fixture();
    let created = f
        .call(
            "create",
            json!({"kind": "entity", "entity_kind": "project", "name": "ordinary", "tags": ["ordinary-tag"], "properties": {"source": "before"}}),
        )
        .await;
    let id = created["id"].as_str().expect("created id").to_string();

    let updated = f
        .call(
            "update",
            json!({"id": id, "properties": {"source": "after"}}),
        )
        .await;
    assert_eq!(
        updated["properties"]["source"],
        json!("after"),
        "an untagged entity is still writable by the generic verb: {updated}"
    );
    assert_eq!(
        updated["tags"],
        json!(["ordinary-tag"]),
        "a patch that names no tags leaves the row's tags alone: {updated}"
    );
    let deleted = f.call("delete", json!({"id": id})).await;
    assert_eq!(deleted["deleted"], json!(true));
}

/// #2596: unrelated policy rows must not displace an older matching deny.
/// The decision used to read the newest 1,000 rows and rank them in Rust, which
/// made an older matching `deny` invisible once the table passed the cap --
/// fail-open on an authorization surface. The decision is resolved in SQL now,
/// so the row count does not bound what it can see.
#[tokio::test]
async fn a_matching_deny_still_decides_after_a_thousand_later_rows() {
    let f = fixture();
    f.call(
        "tool.register",
        json!({"name": "read_file", "source": "builtin", "side_effect": "read"}),
    )
    .await;
    let deny = f
        .call(
            "tool.policy",
            json!({"actor": "agent:a", "tool": "read_file", "decision": "deny"}),
        )
        .await;

    let before = f
        .call(
            "tool.check",
            json!({"tool": "read_file", "actor": "agent:a"}),
        )
        .await;
    assert_eq!(before["decision"], json!("deny"));
    assert_eq!(before["policy_id"], deny["policy"]["id"]);

    for i in 0..1_050 {
        f.call(
            "tool.policy",
            json!({"actor": format!("agent:filler{i}"), "tool": "other_tool", "decision": "allow"}),
        )
        .await;
    }

    let after = f
        .call(
            "tool.check",
            json!({"tool": "read_file", "actor": "agent:a"}),
        )
        .await;
    assert_eq!(
        after["decision"],
        json!("deny"),
        "the deny must still decide with 1,050 newer rows in the table: {after}"
    );
    assert_eq!(after["source"], json!("policy"));
    assert_eq!(after["policy_id"], deny["policy"]["id"]);
}

/// Two different patterns can sum to the same specificity, and `DECISIONS` is
/// closed and strictly ranked, so such a tie always carries the same decision:
/// only the reported `policy_id` can vary. It must not. Oldest-first is the
/// stated rule, and it is what the previous Rust path did by accident.
#[tokio::test]
async fn an_exact_specificity_tie_resolves_to_the_older_row() {
    let f = fixture();
    f.call(
        "tool.register",
        json!({"name": "t.x", "source": "builtin", "side_effect": "write"}),
    )
    .await;
    let first = f
        .call(
            "tool.policy",
            json!({"actor": "lambda:*", "tool": "t.x", "decision": "deny"}),
        )
        .await;
    let second = f
        .call(
            "tool.policy",
            json!({"actor": "lambda:a", "tool": "t.*", "decision": "deny"}),
        )
        .await;
    assert_ne!(first["policy"]["id"], second["policy"]["id"]);

    for _ in 0..5 {
        let checked = f
            .call("tool.check", json!({"tool": "t.x", "actor": "lambda:a"}))
            .await;
        assert_eq!(checked["decision"], json!("deny"));
        assert_eq!(
            checked["policy_id"], first["policy"]["id"],
            "an equal-specificity tie must cite the older row every time: {checked}"
        );
    }
}

/// #2596, grant side: `decide()` read only the newest 500 `granted` rows, so an
/// active grant with more than 500 later grants in front of it stopped being
/// honoured and the check fell through to the default. The grant is resolved
/// in SQL now, so the row count does not bound what it can see.
#[tokio::test]
async fn an_active_grant_still_allows_after_five_hundred_later_grants() {
    let f = fixture();
    f.call(
        "tool.register",
        json!({"name": "send_mail", "side_effect": "egress"}),
    )
    .await;
    let req = f
        .call(
            "tool.request",
            json!({"tool": "send_mail", "actor": "agent:a"}),
        )
        .await;
    let id = s(&req, "request_id");
    f.call("tool.grant", json!({"id": id})).await;
    let before = f
        .call(
            "tool.check",
            json!({"tool": "send_mail", "actor": "agent:a"}),
        )
        .await;
    assert_eq!(before["source"], json!("grant"), "{before}");
    assert_eq!(before["grant_id"], json!(id));

    for i in 0..520 {
        let filler = f
            .call(
                "tool.request",
                json!({"tool": "other_tool", "actor": format!("agent:filler{i}")}),
            )
            .await;
        f.call("tool.grant", json!({"id": s(&filler, "request_id")}))
            .await;
    }

    let after = f
        .call(
            "tool.check",
            json!({"tool": "send_mail", "actor": "agent:a"}),
        )
        .await;
    assert_eq!(
        after["decision"],
        json!("allow"),
        "the grant must still allow with 520 newer grants in the table: {after}"
    );
    assert_eq!(after["source"], json!("grant"));
    assert_eq!(after["grant_id"], json!(id));
}

/// Two active grants can allow the same pair. The capped read handed back the
/// newest request first, and the statement keeps that order, so the check
/// cites the newer grant on every call.
#[tokio::test]
async fn of_two_active_grants_the_newer_request_is_cited() {
    let f = fixture();
    f.call(
        "tool.register",
        json!({"name": "send_mail", "side_effect": "egress"}),
    )
    .await;
    // Both requests go in before either grant: once one grant is active, a
    // request for a pair it covers takes the fast path and inserts no row.
    let older = s(
        &f.call(
            "tool.request",
            json!({"tool": "send_mail", "actor": "agent:*"}),
        )
        .await,
        "request_id",
    );
    let newer = s(
        &f.call(
            "tool.request",
            json!({"tool": "send_mail", "actor": "agent:a"}),
        )
        .await,
        "request_id",
    );
    f.call("tool.grant", json!({"id": newer})).await;
    f.call("tool.grant", json!({"id": older})).await;

    for _ in 0..5 {
        let checked = f
            .call(
                "tool.check",
                json!({"tool": "send_mail", "actor": "agent:a"}),
            )
            .await;
        assert_eq!(checked["source"], json!("grant"), "{checked}");
        assert_eq!(
            checked["grant_id"],
            json!(newer),
            "the newer of two active grants must be cited every time: {checked}"
        );
    }
}

/// Patterns match by bytes. SQLite's `LIKE` and `length()` stop reading a TEXT
/// value at its first NUL, so a predicate written with them read `a\0b*` as
/// `a`: not a prefix pattern, and no match for `a\0bcd`. A `deny` written that
/// way stopped applying, and a grant would have stopped allowing.
#[tokio::test]
async fn a_nul_bearing_prefix_pattern_matches_by_bytes_for_policies_and_grants() {
    let f = fixture();
    let deny = f
        .call(
            "tool.policy",
            json!({"actor": "agent:a", "tool": "a\0b*", "decision": "deny"}),
        )
        .await;
    let denied = f
        .call("tool.check", json!({"tool": "a\0bcd", "actor": "agent:a"}))
        .await;
    assert_eq!(denied["decision"], json!("deny"), "{denied}");
    assert_eq!(denied["policy_id"], deny["policy"]["id"]);
    let unmatched = f
        .call("tool.check", json!({"tool": "a\0xcd", "actor": "agent:a"}))
        .await;
    assert_eq!(
        unmatched["source"],
        json!("default"),
        "a different byte after the NUL is outside the prefix: {unmatched}"
    );

    // The rank reads bytes too: the exact row is more specific than the
    // prefix row, so it decides even though a deny outranks an allow on a tie.
    let exact = f
        .call(
            "tool.policy",
            json!({"actor": "agent:a", "tool": "a\0bcd", "decision": "allow"}),
        )
        .await;
    let ranked = f
        .call("tool.check", json!({"tool": "a\0bcd", "actor": "agent:a"}))
        .await;
    assert_eq!(ranked["decision"], json!("allow"), "{ranked}");
    assert_eq!(ranked["policy_id"], exact["policy"]["id"]);

    let id = s(
        &f.call("tool.request", json!({"tool": "a\0b*", "actor": "agent:b"}))
            .await,
        "request_id",
    );
    f.call("tool.grant", json!({"id": id})).await;
    let allowed = f
        .call("tool.check", json!({"tool": "a\0bcd", "actor": "agent:b"}))
        .await;
    assert_eq!(allowed["source"], json!("grant"), "{allowed}");
    assert_eq!(allowed["grant_id"], json!(id));
    let not_allowed = f
        .call("tool.check", json!({"tool": "a\0xcd", "actor": "agent:b"}))
        .await;
    assert_eq!(not_allowed["source"], json!("default"), "{not_allowed}");
}

#[tokio::test]
async fn filtered_tool_lists_apply_predicates_before_the_limit() {
    let f = fixture();
    f.call(
        "tool.register",
        json!({"name": "older-tool", "kind": "tool"}),
    )
    .await;
    f.call(
        "tool.register",
        json!({"name": "newer-verb", "kind": "verb"}),
    )
    .await;
    let tools = f
        .call("tool.list", json!({"kind": "tool", "limit": 1}))
        .await;
    assert_eq!(tools["count"], json!(1), "{tools}");
    assert_eq!(tools["tools"][0]["name"], json!("older-tool"));

    f.call(
        "tool.policy",
        json!({"actor": "agent:*", "tool": "older-tool", "decision": "deny"}),
    )
    .await;
    f.call(
        "tool.policy",
        json!({"actor": "agent:unrelated", "tool": "older-tool", "decision": "allow"}),
    )
    .await;
    let policies = f
        .call(
            "tool.policies",
            json!({"actor": "agent:target", "limit": 1}),
        )
        .await;
    assert_eq!(policies["count"], json!(1), "{policies}");
    assert_eq!(policies["policies"][0]["actor"], json!("agent:*"));

    f.call(
        "tool.request",
        json!({"tool": "newer-verb", "actor": "agent:target"}),
    )
    .await;
    f.call(
        "tool.request",
        json!({"tool": "newer-verb", "actor": "agent:unrelated"}),
    )
    .await;
    let requests = f
        .call(
            "tool.requests",
            json!({"actor": "agent:target", "limit": 1}),
        )
        .await;
    assert_eq!(requests["count"], json!(1), "{requests}");
    assert_eq!(requests["requests"][0]["actor"], json!("agent:target"));
}

#[tokio::test]
async fn grant_id_wildcards_cannot_select_a_request() {
    let f = fixture();
    f.call("tool.register", json!({"name": "send_mail"})).await;
    let request = f
        .call(
            "tool.request",
            json!({"tool": "send_mail", "actor": "agent:recipient"}),
        )
        .await;
    let id = s(&request, "request_id");
    for (verb, wildcard) in [("tool.grant", "%"), ("tool.deny", "_")] {
        let error = f.call_err(verb, json!({"id": wildcard})).await;
        assert!(error.contains("grant id"), "{error}");
    }
    let pending = f
        .call("tool.requests", json!({"status": "requested"}))
        .await;
    assert_eq!(pending["requests"][0]["id"], json!(id));
    assert_eq!(pending["requests"][0]["status"], json!("requested"));

    f.call("tool.grant", json!({"id": &id[..8]})).await;
    let error = f.call_err("tool.revoke", json!({"id": "%"})).await;
    assert!(error.contains("grant id"), "{error}");
    let granted = f.call("tool.requests", json!({"status": "granted"})).await;
    assert_eq!(granted["requests"][0]["id"], json!(id));
    assert_eq!(granted["requests"][0]["status"], json!("granted"));
}
