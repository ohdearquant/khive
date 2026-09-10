//! Acceptance arms of ADR-180, driven through the verb registry.

use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

struct Fixture {
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
    Fixture { registry }
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
