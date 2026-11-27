//! The operation pack's acceptance cases, run against an in-memory store.
//!
//! The fixture is a joint filing: a preparer requests it, every applicant signs, the preparer
//! submits it unless it goes through the online channel, and the office issues it. Types are
//! published with an explicit namespace; operations are opened in the default write namespace by
//! a caller who can read the types.

use khive_pack_operation::OperationPack;
use khive_runtime::{
    KhiveRuntime, PackRegistry, RequestIdentity, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::Pack;
use serde_json::{json, Value};

const TYPES: &str = "filing-types";
const PREPARER: &str = "preparer-1";

struct Store {
    registry: VerbRegistry,
    _runtime: KhiveRuntime,
}

/// A caller: who it is and, when it names one, the single namespace it reads and writes.
#[derive(Clone)]
struct Caller {
    identity: RequestIdentity,
    explicit: Option<&'static str>,
}

fn who(actor: &str, explicit: Option<&'static str>, visible: &[&str]) -> Caller {
    Caller {
        identity: RequestIdentity {
            namespace: explicit.unwrap_or("local").to_owned(),
            actor_id: Some(actor.to_owned()),
            visible_namespaces: visible.iter().map(|ns| (*ns).to_owned()).collect(),
            ..Default::default()
        },
        explicit,
    }
}

/// Publishes and reads the types, and sees nothing else.
fn author() -> Caller {
    who("type-author", Some(TYPES), &[])
}

/// Writes operations in the default namespace and reads the types.
fn office() -> Caller {
    who(PREPARER, None, &[TYPES])
}

fn with_namespace(caller: &Caller, mut params: Value) -> Value {
    if let (Some(namespace), Value::Object(map)) = (caller.explicit, &mut params) {
        map.insert("namespace".to_owned(), json!(namespace));
    }
    params
}

impl Store {
    fn new() -> Self {
        let packs = vec![OperationPack::NAME.to_owned()];
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: packs.clone(),
            actor_id: Some(PREPARER.to_owned()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("in-memory runtime without embedding services");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(PREPARER.to_owned()));
        PackRegistry::register_packs(&packs, runtime.clone(), &mut builder)
            .expect("the operation pack is discovered through its factory");
        let registry = builder.build().expect("registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("pack schema installation");
        Self {
            registry,
            _runtime: runtime,
        }
    }

    async fn call(&self, caller: Caller, verb: &str, params: Value) -> Value {
        let params = with_namespace(&caller, params);
        self.registry
            .dispatch_with_identity(verb, params.clone(), Some(caller.identity))
            .await
            .unwrap_or_else(|e| panic!("{verb} {params}: {e}"))
    }

    async fn refused(&self, caller: Caller, verb: &str, params: Value) -> String {
        let params = with_namespace(&caller, params);
        match self
            .registry
            .dispatch_with_identity(verb, params.clone(), Some(caller.identity))
            .await
        {
            Ok(reply) => panic!("{verb} {params} was accepted: {reply}"),
            Err(error) => error.to_string(),
        }
    }

    async fn defined(&self) -> Value {
        let document: Value =
            serde_json::from_str(include_str!("fixtures/joint-filing.json")).expect("fixture");
        self.call(author(), "operation.define", json!({"document": document}))
            .await
    }

    /// A defined type and an operation opened with two applicants, the office and the preparer.
    async fn opened(&self) -> String {
        self.defined().await;
        let opened = self
            .call(
                office(),
                "operation.open",
                json!({
                    "type": "joint-filing",
                    "bindings": {
                        "applicant": [person("a1"), person("a2")],
                        "office": {"kind": "office", "key": "registry"},
                        "preparer": person(PREPARER),
                    }
                }),
            )
            .await;
        opened["operation_id"]
            .as_str()
            .expect("operation id")
            .to_owned()
    }

    async fn record(&self, operation: &str, items: Value) -> Value {
        self.call(
            office(),
            "operation.record",
            json!({"operation": operation, "items": items}),
        )
        .await
    }

    async fn frontier(&self, operation: &str, extra: Value) -> Value {
        let mut params = json!({"operation": operation});
        if let Value::Object(extra) = extra {
            params.as_object_mut().expect("object").extend(extra);
        }
        self.call(office(), "operation.frontier", params).await
    }
}

fn person(key: &str) -> Value {
    json!({"kind": "person", "key": key})
}

fn act(key: &str, step: &str, identity: Option<&str>) -> Value {
    let mut item = json!({"kind": "act", "key": key, "step": step});
    if let Some(identity) = identity {
        item["identity"] = person(identity);
    }
    item
}

fn items_of<'a>(frontier: &'a Value, kind: &str) -> Vec<&'a Value> {
    frontier["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|item| item["kind"] == kind)
        .collect()
}

fn status<'a>(frontier: &'a Value, step: &str) -> &'a Value {
    &frontier["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .find(|row| row["step"] == step)
        .unwrap_or_else(|| panic!("step {step} in {frontier}"))["status"]
}

#[tokio::test]
async fn one_signature_of_two_leaves_the_other_owed_and_the_submission_blocked() {
    let store = Store::new();
    let operation = store.opened().await;
    store
        .record(
            &operation,
            json!([
                act("request-1", "request", None),
                act("sign-a1", "sign", Some("a1"))
            ]),
        )
        .await;
    let frontier = store.frontier(&operation, json!({})).await;

    let owed = items_of(&frontier, "act_owed");
    assert_eq!(owed.len(), 1, "{frontier}");
    assert_eq!(owed[0]["step"], "sign");
    assert_eq!(owed[0]["identity"]["key"], "a2");
    assert_eq!(owed[0]["quantifier"], "all_of");
    assert_eq!(
        owed[0]["supplier"],
        json!({"slot": "applicant", "identity": person("a2")})
    );

    let blocked = items_of(&frontier, "act_blocked");
    let submit = blocked
        .iter()
        .find(|item| item["step"] == "submit")
        .unwrap_or_else(|| panic!("submit is blocked: {frontier}"));
    assert_eq!(
        submit["waits_on"],
        json!([{"step": "sign", "suppliers": [{"slot": "applicant", "identity": person("a2")}]}])
    );
    assert_eq!(status(&frontier, "request"), "done");
    assert_eq!(status(&frontier, "sign"), "owed");
    assert_eq!(status(&frontier, "submit"), "blocked");
    assert_eq!(frontier["complete"], false);

    // The second applicant's group holds what is owed of them and what waits on them.
    let group = frontier["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|group| group["supplier"]["identity"]["key"] == "a2")
        .unwrap_or_else(|| panic!("a group for a2: {frontier}"));
    let steps: Vec<&Value> = group["items"]
        .as_array()
        .expect("group items")
        .iter()
        .map(|item| &item["step"])
        .collect();
    assert_eq!(steps, vec!["sign", "submit"]);
    assert!(
        !frontier["groups"]
            .as_array()
            .expect("groups")
            .iter()
            .any(|group| group["supplier"]["identity"]["key"] == "a1"),
        "the applicant who signed owes nothing: {frontier}"
    );

    // Grouped by slot, the applicants share one group.
    let by_slot = store.frontier(&operation, json!({"group_by": "by"})).await;
    assert!(by_slot["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .any(|group| group["supplier"] == json!({"slot": "applicant"})));
}

#[tokio::test]
async fn the_online_channel_skips_the_submission_and_the_office_owes_the_issue() {
    let store = Store::new();
    let operation = store.opened().await;
    store
        .record(
            &operation,
            json!([
                {"kind": "fact", "key": "channel", "field": "channel", "value": "online"},
                act("request-1", "request", None),
                act("sign-a1", "sign", Some("a1")),
                act("sign-a2", "sign", Some("a2")),
            ]),
        )
        .await;
    let frontier = store.frontier(&operation, json!({})).await;

    assert!(
        !frontier["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|item| item["step"] == "submit"),
        "submit is not asked: {frontier}"
    );
    assert_eq!(status(&frontier, "submit"), "bypassed");
    let owed = items_of(&frontier, "act_owed");
    assert_eq!(owed.len(), 1, "{frontier}");
    assert_eq!(owed[0]["step"], "issue");
    assert_eq!(owed[0]["by"], "office");
    assert_eq!(owed[0]["identity"]["key"], "registry");
    assert_eq!(
        frontier["not_asked"],
        json!({"count": 1, "steps": [{"step": "submit", "reason": "skipped", "rule": 0, "alternative": 0, "identity": person(PREPARER)}]})
    );

    store
        .record(
            &operation,
            json!([{"kind": "act", "key": "issue-1", "step": "issue", "identity": {"kind": "office", "key": "registry"}}]),
        )
        .await;
    let done = store.frontier(&operation, json!({})).await;
    assert_eq!(done["complete"], true, "{done}");
    assert_eq!(done["items"], json!([]));
}

#[tokio::test]
async fn without_the_online_channel_the_issue_waits_on_the_submission() {
    let store = Store::new();
    let operation = store.opened().await;
    store
        .record(
            &operation,
            json!([
                {"kind": "fact", "key": "channel", "field": "channel", "value": "paper"},
                act("request-1", "request", None),
                act("sign-a1", "sign", Some("a1")),
                act("sign-a2", "sign", Some("a2")),
            ]),
        )
        .await;
    let frontier = store.frontier(&operation, json!({})).await;
    let owed = items_of(&frontier, "act_owed");
    assert_eq!(owed.len(), 1, "{frontier}");
    assert_eq!(owed[0]["step"], "submit");
    let blocked = items_of(&frontier, "act_blocked");
    assert_eq!(blocked.len(), 1, "{frontier}");
    assert_eq!(blocked[0]["step"], "issue");
    assert_eq!(blocked[0]["waits_on"][0]["step"], "submit");
}

#[tokio::test]
async fn the_frontier_reads_as_of_the_observed_or_the_recorded_time() {
    let store = Store::new();
    store.defined().await;
    let opened = store
        .call(office(), "operation.open", json!({"type": "joint-filing"}))
        .await;
    let operation = opened["operation_id"].as_str().expect("id").to_owned();
    let early = "2026-01-01T08:00:00Z";
    store
        .record(
            &operation,
            json!([
                {"kind": "binding", "key": "b-a1", "slot": "applicant", "identity": person("a1"), "observed_at": early},
                {"kind": "binding", "key": "b-a2", "slot": "applicant", "identity": person("a2"), "observed_at": early},
                {"kind": "binding", "key": "b-office", "slot": "office", "identity": {"kind": "office", "key": "registry"}, "observed_at": early},
                {"kind": "binding", "key": "b-preparer", "slot": "preparer", "identity": person(PREPARER), "observed_at": early},
                {"kind": "act", "key": "request-1", "step": "request", "observed_at": "2026-01-01T09:00:00Z"},
                {"kind": "act", "key": "sign-a1", "step": "sign", "identity": person("a1"), "observed_at": "2026-01-02T09:00:00Z"},
                {"kind": "act", "key": "sign-a2", "step": "sign", "identity": person("a2"), "observed_at": "2026-01-03T09:00:00Z"},
            ]),
        )
        .await;
    let before = store
        .frontier(&operation, json!({"as_of": "2026-01-02T12:00:00Z"}))
        .await;
    assert_eq!(
        before["records_applied"], 6,
        "four bindings and two acts: {before}"
    );
    let owed = items_of(&before, "act_owed");
    assert_eq!(owed.len(), 1, "{before}");
    assert_eq!(owed[0]["identity"]["key"], "a2");

    let after = store
        .frontier(&operation, json!({"as_of": "2026-01-03T12:00:00Z"}))
        .await;
    assert_eq!(items_of(&after, "act_owed")[0]["step"], "submit");

    // Every record was stored after these observed times; on the recorded basis, an as-of time
    // before the store saw them reads none of them.
    let recorded = store
        .frontier(
            &operation,
            json!({"as_of": "2026-01-03T12:00:00Z", "basis": "recorded"}),
        )
        .await;
    assert_eq!(recorded["records_applied"], 0, "{recorded}");
    let slots: Vec<&Value> = items_of(&recorded, "slot_unbound")
        .iter()
        .map(|item| &item["slot"])
        .collect();
    assert_eq!(slots, vec!["preparer", "applicant", "office"], "{recorded}");
}

#[tokio::test]
async fn a_required_slot_nobody_bound_holds_its_steps_for_the_opener() {
    let store = Store::new();
    store.defined().await;
    let opened = store
        .call(
            office(),
            "operation.open",
            json!({"type": "joint-filing", "bindings": {"preparer": person(PREPARER)}}),
        )
        .await;
    let operation = opened["operation_id"].as_str().expect("id").to_owned();
    let frontier = store.frontier(&operation, json!({})).await;
    let unbound = items_of(&frontier, "slot_unbound");
    let applicant = unbound
        .iter()
        .find(|item| item["slot"] == "applicant")
        .unwrap_or_else(|| panic!("applicant unbound: {frontier}"));
    assert_eq!(applicant["holds_steps"], json!(["sign"]));
    assert_eq!(applicant["supplier"], json!({"opener": PREPARER}));
    assert_eq!(items_of(&frontier, "act_owed")[0]["step"], "request");

    let bound = store
        .call(
            office(),
            "operation.bind",
            json!({"operation": operation, "slot": "applicant", "identity": person("a1")}),
        )
        .await;
    assert_eq!(bound["replayed"], false);
    let again = store
        .call(
            office(),
            "operation.bind",
            json!({"operation": operation, "slot": "applicant", "identity": person("a1")}),
        )
        .await;
    assert_eq!(again["replayed"], true, "{again}");
    assert_eq!(again["sequence"], bound["sequence"]);
    let refused = store
        .refused(
            office(),
            "operation.bind",
            json!({"operation": operation, "slot": "preparer", "identity": person("someone-else")}),
        )
        .await;
    assert!(refused.contains("slot_holds_one"), "{refused}");
}

#[tokio::test]
async fn one_caller_key_records_one_row() {
    let store = Store::new();
    let operation = store.opened().await;
    let first = store
        .record(&operation, json!([act("request-1", "request", None)]))
        .await;
    let second = store
        .record(&operation, json!([act("request-1", "request", None)]))
        .await;
    assert_eq!(first["recorded"][0]["replayed"], false);
    assert_eq!(second["recorded"][0]["replayed"], true);
    assert_eq!(
        second["recorded"][0]["sequence"],
        first["recorded"][0]["sequence"]
    );
    let frontier = store.frontier(&operation, json!({})).await;
    assert_eq!(
        frontier["records_applied"], 5,
        "four bindings and one act: {frontier}"
    );

    let refused = store
        .refused(
            office(),
            "operation.record",
            json!({"operation": operation, "items": [{"kind": "fact", "key": "request-1", "field": "channel", "value": "paper"}]}),
        )
        .await;
    assert!(refused.contains("caller_key_conflict"), "{refused}");
    let unchanged = store.frontier(&operation, json!({})).await;
    assert_eq!(unchanged["records_applied"], 5);
}

#[tokio::test]
async fn a_token_for_the_type_namespace_alone_reads_no_operation() {
    let store = Store::new();
    let operation = store.opened().await;
    let type_only = author();
    for (verb, params) in [
        ("operation.frontier", json!({"operation": operation})),
        (
            "operation.record",
            json!({"operation": operation, "items": [act("x", "request", None)]}),
        ),
        (
            "operation.bind",
            json!({"operation": operation, "slot": "applicant", "identity": person("a3")}),
        ),
    ] {
        let refused = store.refused(type_only.clone(), verb, params).await;
        assert!(refused.contains("not found"), "{verb}: {refused}");
        assert!(
            !refused.contains("joint-filing"),
            "{verb} names nothing of the operation: {refused}"
        );
    }
    // The control: the office's own token reads it.
    store.frontier(&operation, json!({})).await;
}

#[tokio::test]
async fn the_namespace_holding_the_type_opens_and_records_nothing() {
    let store = Store::new();
    let operation = store.opened().await;
    let refused = store
        .refused(author(), "operation.open", json!({"type": "joint-filing"}))
        .await;
    assert!(refused.contains("instance_in_type_namespace"), "{refused}");

    let refused = store
        .refused(
            author(),
            "operation.record",
            json!({"operation": operation, "items": [act("x", "request", None)]}),
        )
        .await;
    assert!(refused.contains("not found"), "{refused}");
    let frontier = store.frontier(&operation, json!({})).await;
    assert_eq!(
        frontier["records_applied"], 4,
        "nothing was recorded: {frontier}"
    );
}

#[tokio::test]
async fn publishing_is_immutable_per_version() {
    let store = Store::new();
    let first = store.defined().await;
    assert_eq!(first["published"].as_array().expect("published").len(), 1);
    assert_eq!(
        first["published"][0]["steps"]
            .as_array()
            .expect("steps")
            .len(),
        4
    );
    let again = store.defined().await;
    assert_eq!(again["published"], json!([]));
    assert_eq!(
        again["already_published"][0]["digest"],
        first["published"][0]["digest"]
    );

    let mut changed: Value =
        serde_json::from_str(include_str!("fixtures/joint-filing.json")).expect("fixture");
    changed["operation_types"][0]["steps"][1]["quantifier"] = json!("at_least_one");
    let refused = store
        .refused(author(), "operation.define", json!({"document": changed}))
        .await;
    assert!(refused.contains("version_conflict"), "{refused}");

    changed["operation_types"][0]["version"] = json!(2);
    let second = store
        .call(author(), "operation.define", json!({"document": changed}))
        .await;
    assert_eq!(second["published"][0]["version"], 2);
}

#[tokio::test]
async fn acts_name_an_identity_bound_to_their_slot() {
    let store = Store::new();
    let operation = store.opened().await;
    let refused = store
        .refused(
            office(),
            "operation.record",
            json!({"operation": operation, "items": [act("sign-x", "sign", None)]}),
        )
        .await;
    assert!(refused.contains("act_identity_required"), "{refused}");
    let refused = store
        .refused(
            office(),
            "operation.record",
            json!({"operation": operation, "items": [act("sign-x", "sign", Some("stranger"))]}),
        )
        .await;
    assert!(refused.contains("act_identity_unbound"), "{refused}");
    // The preparer slot holds one identity: an act naming anyone else is refused, and it
    // would otherwise have turned the step done.
    let refused = store
        .refused(
            office(),
            "operation.record",
            json!({"operation": operation, "items": [act("request-x", "request", Some("stranger"))]}),
        )
        .await;
    assert!(refused.contains("act_identity_unbound"), "{refused}");
    let frontier = store.frontier(&operation, json!({})).await;
    assert_eq!(status(&frontier, "request"), "owed");
    store
        .record(
            &operation,
            json!([act("request-p", "request", Some(PREPARER))]),
        )
        .await;
    let frontier = store.frontier(&operation, json!({})).await;
    assert_eq!(status(&frontier, "request"), "done");
    let refused = store
        .refused(
            office(),
            "operation.record",
            json!({"operation": operation, "items": [act("x", "approve", None)]}),
        )
        .await;
    assert!(refused.contains("unknown_step"), "{refused}");
}

fn chain(id: &str, steps: usize) -> Value {
    let steps: Vec<Value> = (0..steps)
        .map(|i| {
            let mut step = json!({"name": format!("s{i}"), "by": "office"});
            if i > 0 {
                step["after"] = json!([format!("s{}", i - 1)]);
            }
            if i + 1 == steps {
                step["outcome"] = json!(true);
            }
            step
        })
        .collect();
    json!({"operation_types": [{"id": id, "version": 1, "slots": [{"name": "office"}], "steps": steps}]})
}

#[tokio::test]
async fn a_type_declares_at_most_256_steps() {
    let store = Store::new();
    let published = store
        .call(
            author(),
            "operation.define",
            json!({"document": chain("long", 256)}),
        )
        .await;
    assert_eq!(published["published"][0]["version"], 1, "{published}");
    let refused = store
        .refused(
            author(),
            "operation.define",
            json!({"document": chain("longer", 257)}),
        )
        .await;
    assert!(refused.contains("too_many_steps"), "{refused}");
}

#[tokio::test]
async fn at_least_one_is_done_when_one_identity_acts() {
    let store = Store::new();
    let mut document: Value =
        serde_json::from_str(include_str!("fixtures/joint-filing.json")).expect("fixture");
    document["operation_types"][0]["id"] = json!("single-signature-filing");
    document["operation_types"][0]["steps"][1]["quantifier"] = json!("at_least_one");
    store
        .call(author(), "operation.define", json!({"document": document}))
        .await;
    let opened = store
        .call(
            office(),
            "operation.open",
            json!({
                "type": "single-signature-filing",
                "bindings": {"applicant": [person("a1"), person("a2")], "office": {"kind": "office", "key": "registry"}, "preparer": person(PREPARER)}
            }),
        )
        .await;
    let operation = opened["operation_id"].as_str().expect("id").to_owned();
    let before = {
        store
            .record(&operation, json!([act("request-1", "request", None)]))
            .await;
        store.frontier(&operation, json!({})).await
    };
    assert_eq!(
        items_of(&before, "act_owed").len(),
        2,
        "either applicant may sign: {before}"
    );
    store
        .record(&operation, json!([act("sign-a2", "sign", Some("a2"))]))
        .await;
    let after = store.frontier(&operation, json!({})).await;
    assert_eq!(status(&after, "sign"), "done");
    assert_eq!(items_of(&after, "act_owed")[0]["step"], "submit");
}
