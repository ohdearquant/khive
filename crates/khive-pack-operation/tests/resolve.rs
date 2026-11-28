//! operation.resolve and operation.decide (ADR-222 Amendment 1, A1.3): which slot of a type each
//! subject of a form fills, read from the forms pack's subjects and terms and from the decisions
//! kept beside the type, run against an in-memory store with kg, forms and operation installed.
//!
//! Form A is a municipality's permit application; form B is another municipality's, with other
//! subject names and the same roles.

use khive_pack_operation::OperationPack;
use khive_runtime::{
    KhiveRuntime, PackRegistry, RequestIdentity, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::Pack;
use serde_json::{json, Value};

use khive_pack_forms as _;
use khive_pack_kg as _;

const NS: &str = "permits";
const TYPE: &str = "permit-application";
const A: &str = "PERMIT-A";
const B: &str = "PERMIT-B";
const ID: &str = "identification number";

struct Store {
    registry: VerbRegistry,
    _runtime: KhiveRuntime,
}

fn caller() -> RequestIdentity {
    RequestIdentity {
        namespace: NS.to_owned(),
        actor_id: Some("permit-desk".to_owned()),
        ..Default::default()
    }
}

impl Store {
    fn new() -> Self {
        let packs = vec![
            "kg".to_owned(),
            "forms".to_owned(),
            OperationPack::NAME.to_owned(),
        ];
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: packs.clone(),
            actor_id: Some("permit-desk".to_owned()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("permit-desk".to_owned()));
        PackRegistry::register_packs(&packs, runtime.clone(), &mut builder)
            .expect("kg, forms and operation are discovered through their factories");
        let registry = builder.build().expect("registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("pack schema installation");
        runtime.install_edge_rules(registry.all_edge_rules());
        Self {
            registry,
            _runtime: runtime,
        }
    }

    async fn dispatch(&self, verb: &str, mut params: Value) -> Result<Value, String> {
        params["namespace"] = json!(NS);
        self.registry
            .dispatch_with_identity(verb, params, Some(caller()))
            .await
            .map_err(|e| e.to_string())
    }

    async fn call(&self, verb: &str, params: Value) -> Value {
        self.dispatch(verb, params.clone())
            .await
            .unwrap_or_else(|e| panic!("{verb} {params}: {e}"))
    }

    async fn refused(&self, verb: &str, params: Value) -> String {
        match self.dispatch(verb, params.clone()).await {
            Ok(reply) => panic!("{verb} {params} was accepted: {reply}"),
            Err(error) => error,
        }
    }

    async fn load(&self, document: Value) {
        let reply = self
            .call("forms.load", json!({ "document": document }))
            .await;
        assert_eq!(reply["loaded"], true, "{reply}");
    }

    async fn define(&self, version: i64, co_applicant: bool) -> Value {
        self.call(
            "operation.define",
            json!({ "document": type_document(version, co_applicant) }),
        )
        .await
    }

    async fn resolve(&self, form: &str, extra: Value) -> Value {
        let mut params = json!({ "type": TYPE, "form": form });
        if let Value::Object(extra) = extra {
            params.as_object_mut().expect("object").extend(extra);
        }
        self.call("operation.resolve", params).await
    }

    async fn decide(&self, subjects: &[&str], slot: Option<&str>, reason: &str) -> Value {
        let mut params = json!({ "type": TYPE, "form": A, "subjects": subjects, "reason": reason });
        match slot {
            Some(slot) => params["slot"] = json!(slot),
            None => params["not_this_type"] = json!(true),
        }
        self.call("operation.decide", params).await["decision"].clone()
    }

    /// A store with form A loaded and the type published at version 1.
    async fn with_a() -> Self {
        let store = Self::new();
        store.load(form_a("2026-01", 1, true)).await;
        store.define(1, true).await;
        store
    }
}

fn type_document(version: i64, co_applicant: bool) -> Value {
    let mut slots = vec![json!({"name": "applicant", "kind": "person", "role": "applicant"})];
    if co_applicant {
        slots.push(json!({"name": "co_applicant", "kind": "Person", "role": "applicant", "qualifier": "additional"}));
    }
    slots.push(json!({"name": "parcel", "kind": "parcel", "role": "parcel"}));
    slots.push(
        json!({"name": "parcel_owner", "defined_by": {"relation": "owns", "slot": "parcel"}}),
    );
    json!({"operation_types": [{
        "id": TYPE,
        "version": version,
        "slots": slots,
        "steps": [
            {"name": "apply", "by": "applicant"},
            {"name": "consent", "by": "parcel_owner", "after": ["apply"], "outcome": true}
        ]
    }]})
}

fn form(
    form_id: &str,
    revision: &str,
    n: u8,
    terms: Value,
    subjects: Value,
    filler: &str,
) -> Value {
    let items: Vec<Value> = subjects
        .as_array()
        .expect("subjects")
        .iter()
        .enumerate()
        .map(|(i, subject)| {
            json!({
                "label": format!("{} number", subject["id"].as_str().expect("id")),
                "page": 1,
                "order": i + 1,
                "item_kind": "text",
                "filled_by": filler,
                "about": subject["id"],
                "field": ID,
            })
        })
        .collect();
    json!({
        "form_id": form_id,
        "revision": revision,
        "title": format!("Permit application {form_id}"),
        "source": {"url": format!("https://forms.example/permit-{n}.pdf"), "sha256": n.to_string().repeat(64)},
        "terms": terms,
        "subjects": subjects,
        "fields": [{"name": ID, "definition": "Number that identifies a party or a thing to the issuer.", "value_type": "identifier"}],
        "items": items,
    })
}

/// Form A: applicant, additional applicant, parcel, parcel owner (owns: parcel), the
/// applicant's mailing address (belongs to: applicant) and, unless dropped, the permit being
/// revoked.
fn form_a(revision: &str, n: u8, with_permit: bool) -> Value {
    let mut subjects = vec![
        json!({"id": "applicant", "kind": "person", "role": "applicant"}),
        json!({"id": "additional applicant", "kind": "person", "role": "applicant", "qualifier": "additional"}),
        json!({"id": "parcel", "kind": "parcel", "role": "parcel"}),
        json!({"id": "parcel owner", "kind": "person", "role": "owner", "relations": [{"relation": "owns", "to": "parcel"}]}),
        json!({"id": "mailing address", "kind": "address", "role": "mailing address", "relations": [{"relation": "belongs to", "to": "applicant"}]}),
    ];
    if with_permit {
        subjects.push(
            json!({"id": "permit being revoked", "kind": "permit", "role": "revoked permit"}),
        );
    }
    form(
        A,
        revision,
        n,
        json!({
            "kinds": ["person", "parcel", "address", "permit"],
            "roles": ["applicant", "parcel", "owner", "mailing address", "revoked permit"],
            "relations": ["owns", "belongs to"],
            "counting_qualifiers": ["first", "second", "additional"],
            "carrying_relations": ["belongs to"]
        }),
        Value::Array(subjects),
        "applicant",
    )
}

/// Form B: the same roles under other subject names, one of them named by an alias.
fn form_b() -> Value {
    form(
        B,
        "2026-01",
        5,
        json!({"roles": [{"name": "applicant", "aliases": ["petitioner"]}]}),
        json!([
            {"id": "petitioner", "kind": "person", "role": "petitioner"},
            {"id": "joint petitioner", "kind": "person", "role": "Petitioner", "qualifier": "Additional"},
            {"id": "lot", "kind": "parcel", "role": "parcel"},
            {"id": "lot owner", "kind": "person", "role": "owner", "relations": [{"relation": "owns", "to": "lot"}]},
            {"id": "petitioner address", "kind": "address", "role": "mailing address", "relations": [{"relation": "belongs to", "to": "petitioner"}]},
            {"id": "permit to revoke", "kind": "permit", "role": "revoked permit"}
        ]),
        "petitioner",
    )
}

fn entry<'a>(reply: &'a Value, subject: &str) -> &'a Value {
    reply["subjects"]
        .as_array()
        .expect("subjects")
        .iter()
        .find(|s| s["subject"] == subject)
        .unwrap_or_else(|| panic!("no entry for {subject}: {reply}"))
}

fn found(reply: &Value, subject: &str) -> (Value, Value) {
    let entry = entry(reply, subject);
    (entry["resolved_by"].clone(), entry["slot"].clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn a1_3_a_each_subject_of_form_a_resolves_by_its_rung_and_a_decision_settles_the_last() {
    let store = Store::with_a().await;
    let reply = store.resolve(A, json!({})).await;
    assert_eq!(reply["type"]["version"], 1, "{reply}");
    assert_eq!(reply["form"]["revision"], "2026-01", "{reply}");
    for (subject, by, slot) in [
        ("applicant", "role", json!("applicant")),
        ("additional applicant", "role", json!("co_applicant")),
        ("parcel", "role", json!("parcel")),
        ("parcel owner", "relation", json!("parcel_owner")),
        ("mailing address", "carried", json!("applicant")),
        ("permit being revoked", "no_slot", Value::Null),
    ] {
        assert_eq!(
            found(&reply, subject),
            (json!(by), slot),
            "{subject}: {reply}"
        );
    }
    assert_eq!(entry(&reply, "mailing address")["carried_by"], "applicant");
    assert_eq!(reply["lapsed"], json!([]));

    let decision = store
        .decide(
            &["Permit Being Revoked"],
            None,
            "The permit being revoked is the object of the application, not a party to it.",
        )
        .await;
    assert_eq!(decision["subjects"], json!(["permit being revoked"]));
    assert_eq!(decision["supersedes"], json!([]));
    let reply = store.resolve(A, json!({})).await;
    let permit = entry(&reply, "permit being revoked");
    assert_eq!(permit["resolved_by"], "decision", "{reply}");
    assert_eq!(permit["not_this_type"], true);
    assert_eq!(permit["decision"], decision["id"]);
    assert_eq!(reply["decisions_read"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a1_3_b_form_b_resolves_the_same_with_no_decision_written() {
    let store = Store::with_a().await;
    store
        .decide(&["permit being revoked"], None, "Not a party.")
        .await;
    store.load(form_b()).await;
    let reply = store.resolve(B, json!({})).await;
    for (subject, by, slot) in [
        ("petitioner", "role", json!("applicant")),
        ("joint petitioner", "role", json!("co_applicant")),
        ("lot", "role", json!("parcel")),
        ("lot owner", "relation", json!("parcel_owner")),
        ("petitioner address", "carried", json!("applicant")),
        ("permit to revoke", "no_slot", Value::Null),
    ] {
        assert_eq!(
            found(&reply, subject),
            (json!(by), slot),
            "{subject}: {reply}"
        );
    }
    assert_eq!(
        reply["decisions_read"], 0,
        "form A's decision is not form B's"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a1_3_c_a_decision_applies_while_its_subject_and_slot_exist_and_then_lapses() {
    let store = Store::with_a().await;
    let not_party = store
        .decide(&["permit being revoked"], None, "Not a party.")
        .await;
    let co = store
        .decide(
            &["additional applicant"],
            Some("co_applicant"),
            "The additional applicant signs as co-applicant.",
        )
        .await;

    // Type version 2 and form revision 2, the same slots and subjects: both still apply.
    store.define(2, true).await;
    store.load(form_a("2026-02", 2, true)).await;
    let reply = store.resolve(A, json!({})).await;
    assert_eq!(reply["type"]["version"], 2, "{reply}");
    assert_eq!(reply["form"]["revision"], "2026-02", "{reply}");
    assert_eq!(
        entry(&reply, "permit being revoked")["decision"],
        not_party["id"]
    );
    assert_eq!(
        found(&reply, "additional applicant"),
        (json!("decision"), json!("co_applicant"))
    );
    assert_eq!(entry(&reply, "additional applicant")["decision"], co["id"]);

    // A revision that drops the permit lists its decision as lapsed.
    store.load(form_a("2026-03", 3, false)).await;
    let reply = store.resolve(A, json!({})).await;
    assert_eq!(
        reply["lapsed"],
        json!([{"decision": not_party["id"], "subject": "permit being revoked"}]),
        "{reply}"
    );
    assert_eq!(reply["subjects"].as_array().expect("subjects").len(), 5);

    // A version that drops co_applicant: the subject is offered what the rungs would offer,
    // and its decision is named as lapsed; nothing binds it in the decision's place.
    store.define(3, false).await;
    let reply = store.resolve(A, json!({})).await;
    let extra = entry(&reply, "additional applicant");
    assert_eq!(extra["resolved_by"], "undecided", "{reply}");
    assert_eq!(extra["candidates"], json!(["applicant"]));
    assert_eq!(extra["lapsed"], co["id"]);
    assert_eq!(
        found(&reply, "applicant"),
        (json!("role"), json!("applicant"))
    );

    // Version 2, read by name, still has co_applicant, so the decision still applies there.
    let reply = store.resolve(A, json!({"version": 2})).await;
    assert_eq!(
        found(&reply, "additional applicant"),
        (json!("decision"), json!("co_applicant"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_decision_supersedes_the_earlier_one_for_each_subject_it_names() {
    let store = Store::with_a().await;
    let first = store
        .decide(
            &["permit being revoked", "parcel"],
            None,
            "Neither is a party.",
        )
        .await;
    let second = store
        .decide(
            &["parcel"],
            Some("parcel"),
            "The parcel is the parcel slot after all.",
        )
        .await;
    assert_eq!(second["supersedes"], json!([first["id"]]));
    let reply = store.resolve(A, json!({})).await;
    assert_eq!(entry(&reply, "parcel")["decision"], second["id"]);
    assert_eq!(
        found(&reply, "parcel"),
        (json!("decision"), json!("parcel"))
    );
    // The first still decides the subject the second does not name.
    assert_eq!(
        entry(&reply, "permit being revoked")["decision"],
        first["id"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn define_refuses_a_slot_term_the_namespace_does_not_hold() {
    let store = Store::new();
    let message = store
        .refused(
            "operation.define",
            json!({"document": type_document(1, true)}),
        )
        .await;
    assert!(message.contains("unknown_term"), "{message}");

    store.load(form_a("2026-01", 1, true)).await;
    let mut document = type_document(1, true);
    document["operation_types"][0]["slots"][0]["role"] = json!("stranger");
    let message = store
        .refused("operation.define", json!({"document": document}))
        .await;
    assert!(
        message.contains("unknown_term") && message.contains("stranger"),
        "{message}"
    );
    let mut document = type_document(1, true);
    document["operation_types"][0]["slots"][3]["defined_by"]["relation"] = json!("leases");
    let message = store
        .refused("operation.define", json!({"document": document}))
        .await;
    assert!(message.contains("leases"), "{message}");

    // A type whose slots name no term needs none.
    let mut document = type_document(1, true);
    document["operation_types"][0]["id"] = json!("plain");
    document["operation_types"][0]["slots"] =
        json!([{"name": "applicant"}, {"name": "parcel_owner"}]);
    let fresh = Store::new();
    fresh
        .call("operation.define", json!({"document": document}))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn decide_refuses_what_the_form_or_the_type_does_not_have_and_writes_nothing() {
    let store = Store::with_a().await;
    let base = json!({"type": TYPE, "form": A, "reason": "A reason."});
    let with = |extra: Value| {
        let mut params = base.clone();
        params
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        params
    };
    for (extra, expected) in [
        (
            json!({"subjects": ["ghost"], "not_this_type": true}),
            "unknown_subject",
        ),
        (
            json!({"subjects": ["parcel"], "slot": "nowhere"}),
            "unknown_slot",
        ),
        (
            json!({"subjects": ["parcel"], "slot": "parcel", "not_this_type": true}),
            "exactly one",
        ),
        (json!({"subjects": ["parcel"]}), "exactly one"),
        (json!({"subjects": [], "not_this_type": true}), "subjects"),
        (
            json!({"subjects": ["parcel"], "not_this_type": true, "type": "elsewhere"}),
            "unknown_type",
        ),
    ] {
        let message = store.refused("operation.decide", with(extra.clone())).await;
        assert!(message.contains(expected), "{extra}: {message}");
    }
    let reply = store.resolve(A, json!({})).await;
    assert_eq!(reply["decisions_read"], 0, "{reply}");
}
