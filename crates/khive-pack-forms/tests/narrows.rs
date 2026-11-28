//! A field that narrows another: every value of the narrower field is a value of the
//! broader one. A form document gives it with `narrows` on a field, and the store holds
//! it as one contains edge from the broader field to the narrower. Run against an
//! in-memory store with the kg and forms packs installed.

use khive_pack_forms::FormsPack;
use khive_runtime::{KhiveRuntime, PackRegistry, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;
use serde_json::{json, Value};

use khive_pack_kg as _;

const NAME: &str = "name";
const LEGAL: &str = "legal name";
const FULL: &str = "full name";
const MAIDEN: &str = "maiden name";
const GIVEN: &str = "given name";

struct Store {
    registry: VerbRegistry,
    _runtime: KhiveRuntime,
}

impl Store {
    fn new() -> Self {
        let packs = vec!["kg".to_owned(), FormsPack::NAME.to_owned()];
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: packs.clone(),
            actor_id: Some("forms-reader".to_owned()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("forms-reader".to_owned()));
        PackRegistry::register_packs(&packs, runtime.clone(), &mut builder)
            .expect("kg and forms are discovered through their factories");
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

    async fn call(&self, verb: &str, params: Value) -> Value {
        self.registry
            .dispatch(verb, params.clone())
            .await
            .unwrap_or_else(|e| panic!("{verb} {params}: {e}"))
    }

    async fn refused(&self, verb: &str, params: Value) -> String {
        match self.registry.dispatch(verb, params.clone()).await {
            Ok(reply) => panic!("{verb} {params} was accepted: {reply}"),
            Err(error) => error.to_string(),
        }
    }

    async fn load(&self, document: Value) -> Value {
        let reply = self.call("forms.load", json!({"document": document})).await;
        assert_eq!(reply["loaded"], true, "{reply}");
        reply
    }

    async fn refused_load(&self, document: Value) -> String {
        self.refused("forms.load", json!({"document": document}))
            .await
    }

    async fn export(&self) -> Value {
        self.call("forms.export", json!({})).await
    }

    async fn field(&self, name: &str) -> Value {
        self.call("forms.neighbourhood", json!({"node": name}))
            .await
    }

    /// A store holding name and full name, legal name narrowing name, and a
    /// reference number of another value type.
    async fn with_legal_name() -> Self {
        let store = Self::new();
        store.load(name_form()).await;
        store.load(legal_form("LEGAL-1", 2)).await;
        store
            .load(form(
                "REF-1",
                3,
                json!([field("reference number", "identifier")]),
                json!([asking("Reference", 1, "reference number")]),
            ))
            .await;
        store
    }
}

fn source(n: u8) -> Value {
    json!({"url": format!("https://forms.example/narrows-{n}.pdf"), "sha256": n.to_string().repeat(64)})
}

fn field(name: &str, value_type: &str) -> Value {
    json!({"name": name, "definition": format!("The {name} a party gives."), "value_type": value_type})
}

fn narrowing(name: &str, value_type: &str, broader: &str) -> Value {
    let mut spec = field(name, value_type);
    spec["narrows"] = json!(broader);
    spec
}

fn asking(label: &str, order: u32, field: &str) -> Value {
    json!({"label": label, "page": 1, "order": order, "item_kind": "text", "filled_by": "applicant", "field": field})
}

fn form(form_id: &str, n: u8, fields: Value, items: Value) -> Value {
    json!({
        "form_id": form_id,
        "revision": "2026-01",
        "title": format!("Form {form_id}"),
        "source": source(n),
        "fields": fields,
        "items": items
    })
}

/// A form asking for a name and a full name.
fn name_form() -> Value {
    form(
        "NAME-1",
        1,
        json!([field(NAME, "text"), field(FULL, "text")]),
        json!([asking("Name", 1, NAME), asking("Full name", 2, FULL)]),
    )
}

/// A form asking for a legal name, which narrows name.
fn legal_form(form_id: &str, n: u8) -> Value {
    form(
        form_id,
        n,
        json!([narrowing(LEGAL, "text", NAME)]),
        json!([asking("Legal name", 1, LEGAL)]),
    )
}

fn names(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap_or_else(|| panic!("a list: {list}"))
        .iter()
        .map(|name| name.as_str().expect("a name"))
        .collect()
}

/// The contains edges of an export from one field's key to another's.
fn narrows_edges(export: &Value, broader: &str, narrower: &str) -> usize {
    let key = |name: &str| {
        export["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .find(|n| n["type"] == "field" && n["name"] == name)
            .unwrap_or_else(|| panic!("no field named {name}: {export}"))["key"]
            .clone()
    };
    let (source, target) = (key(broader), key(narrower));
    export["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .filter(|e| e["relation"] == "contains" && e["source"] == source && e["target"] == target)
        .count()
}

fn missing(reply: &Value) -> Vec<Value> {
    reply["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter(|f| f["kind"] == "missing")
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_field_narrows_a_loaded_field_by_one_contains_edge() {
    let store = Store::new();
    store.load(name_form()).await;
    let reply = store.load(legal_form("LEGAL-1", 2)).await;
    assert_eq!(reply["warnings"], json!([]), "{reply}");

    let export = store.export().await;
    assert_eq!(narrows_edges(&export, NAME, LEGAL), 1, "{export}");

    let legal = store.field(LEGAL).await;
    assert_eq!(names(&legal["broader"]), [NAME], "{legal}");
    assert_eq!(legal["general"], json!([]), "{legal}");
    assert_eq!(legal["narrower"], json!([]), "{legal}");
    let name = store.field(NAME).await;
    assert_eq!(names(&name["narrower"]), [LEGAL], "{name}");
    assert_eq!(name["broader"], json!([]), "{name}");

    // Resolving the broader field's own name finds it alone.
    let resolved = store
        .call("forms.resolve", json!({"text": NAME, "min_rung": "exact"}))
        .await;
    let hits = resolved["hits"].as_array().expect("hits");
    assert_eq!(hits.len(), 1, "{resolved}");
    assert_eq!(hits[0]["name"], NAME, "{resolved}");

    // The same narrows given again, by another form, changes nothing.
    store.load(legal_form("LEGAL-2", 4)).await;
    let again = store.export().await;
    assert_eq!(narrows_edges(&again, NAME, LEGAL), 1, "{again}");
    let legal = store.field(LEGAL).await;
    assert_eq!(names(&legal["broader"]), [NAME], "{legal}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_to_the_narrower_field_does_not_answer_the_broader_one() {
    let store = Store::with_legal_name().await;
    let mut required = asking("Name", 1, NAME);
    required["necessity"] = json!("required");
    store
        .load(form(
            "ASK-1",
            6,
            json!([field(NAME, "text")]),
            json!([required]),
        ))
        .await;
    let reply = store
        .call(
            "forms.check",
            json!({"forms": ["ASK-1"], "answers": {LEGAL: "Kim Doe"}}),
        )
        .await;
    assert_eq!(missing(&reply).len(), 1, "{reply}");
    assert!(
        missing(&reply).iter().any(|f| f["field"] == NAME),
        "name stays unanswered: {reply}"
    );

    // The control: the name itself answers it.
    let reply = store
        .call(
            "forms.check",
            json!({"forms": ["ASK-1"], "answers": {NAME: "Kim Doe"}}),
        )
        .await;
    assert_eq!(missing(&reply), Vec::<Value>::new(), "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_narrows_that_cannot_hold_is_refused_and_writes_nothing() {
    let store = Store::with_legal_name().await;
    let before = store.export().await;
    let maiden = |n: u8, fields: Value| {
        form(
            "BAD-1",
            n,
            fields,
            json!([asking("Maiden name", 1, MAIDEN)]),
        )
    };
    let cases = [
        (
            maiden(10, json!([narrowing(MAIDEN, "text", "nickname")])),
            "which is neither loaded nor listed earlier in the document",
        ),
        (
            form(
                "BAD-1",
                11,
                json!([narrowing(MAIDEN, "text", GIVEN), field(GIVEN, "text")]),
                json!([
                    asking("Maiden name", 1, MAIDEN),
                    asking("Given name", 2, GIVEN)
                ]),
            ),
            "which is neither loaded nor listed earlier in the document",
        ),
        (
            maiden(12, json!([narrowing(MAIDEN, "text", MAIDEN)])),
            "which is the field itself",
        ),
        (
            maiden(13, json!([narrowing(MAIDEN, "text", "reference number")])),
            "whose value_type is identifier, not text",
        ),
        (
            form(
                "BAD-1",
                14,
                json!([narrowing(NAME, "text", LEGAL)]),
                json!([asking("Name", 1, NAME)]),
            ),
            "which would close a cycle of narrows",
        ),
        (
            form(
                "BAD-1",
                15,
                json!([narrowing(LEGAL, "text", FULL)]),
                json!([asking("Legal name", 1, LEGAL)]),
            ),
            "field \"legal name\" already narrows \"name\"",
        ),
    ];
    for (document, expected) in cases {
        let message = store.refused_load(document).await;
        assert!(message.contains(expected), "{expected}: {message}");
        assert_eq!(store.export().await, before, "{expected}: nothing written");
    }

    // A field listed earlier in the same document may be narrowed.
    let reply = store
        .load(form(
            "GIVEN-1",
            16,
            json!([field(GIVEN, "text"), narrowing(MAIDEN, "text", GIVEN)]),
            json!([
                asking("Given name", 1, GIVEN),
                asking("Maiden name", 2, MAIDEN)
            ]),
        ))
        .await;
    assert_eq!(reply["warnings"], json!([]), "{reply}");
    let maiden = store.field(MAIDEN).await;
    assert_eq!(names(&maiden["broader"]), [GIVEN], "{maiden}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_general_wording_is_not_narrowed_and_a_split_field_keeps_its_edge() {
    let store = Store::new();
    store
        .load(form(
            "ACCT-1",
            20,
            json!([
                field("number", "identifier"),
                narrowing("account number", "identifier", "number")
            ]),
            json!([
                asking("Number", 1, "number"),
                asking("Account number", 2, "account number")
            ]),
        ))
        .await;
    let added = store
        .call(
            "forms.add_item",
            json!({
                "form": "ACCT-1",
                "item": {"label": "Account no", "page": 1, "order": 3, "item_kind": "text", "filled_by": "applicant"},
                "field": field("account number", "identifier")
            }),
        )
        .await;
    store
        .call(
            "forms.split",
            json!({
                "field": "account number",
                "item": added["item"]["id"],
                "existing_name": "bank account number",
                "new_name": "brokerage account number",
                "new_definition": "The number of an account at a brokerage firm."
            }),
        )
        .await;

    let kept = store.field("bank account number").await;
    assert_eq!(names(&kept["broader"]), ["number"], "{kept}");
    assert_eq!(names(&kept["general"]), ["account number"], "{kept}");
    let new = store.field("brokerage account number").await;
    assert_eq!(new["broader"], json!([]), "{new}");
    assert_eq!(names(&new["general"]), ["account number"], "{new}");

    let before = store.export().await;
    let message = store
        .refused_load(form(
            "GEN-1",
            21,
            json!([narrowing(
                "savings account number",
                "identifier",
                "account number"
            )]),
            json!([asking("Savings account", 1, "savings account number")]),
        ))
        .await;
    assert!(message.contains("which is a general wording"), "{message}");
    assert_eq!(store.export().await, before, "nothing written");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_field_written_by_a_decision_has_no_broader_field() {
    let store = Store::new();
    store.load(name_form()).await;
    let mut pet = form(
        "PET-1",
        30,
        json!([{"name": NAME, "definition": "The word a pet answers to.", "value_type": "text", "narrows": FULL}]),
        json!([asking("Pet name", 1, NAME)]),
    );
    pet["title"] = json!("Pet licence");
    let loaded = store.load(pet).await;
    let warnings = loaded["warnings"].to_string();
    assert!(
        warnings.contains("so the field it narrows was not recorded"),
        "an undecided field records no narrows: {loaded}"
    );
    let item = loaded["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["label"] == "Pet name")
        .expect("the pet name line")["id"]
        .clone();

    let before = store.export().await;
    let message = store
        .refused(
            "forms.same",
            json!({"item": item, "new_field": narrowing("pet name", "text", NAME)}),
        )
        .await;
    assert!(message.contains("which forms.load takes"), "{message}");
    assert_eq!(store.export().await, before, "nothing written");

    store
        .call(
            "forms.same",
            json!({"item": item, "new_field": field("pet name", "text")}),
        )
        .await;
    let pet = store.field("pet name").await;
    assert_eq!(pet["broader"], json!([]), "{pet}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_edge_leaves_no_broader_field_and_the_narrows_may_be_given_again() {
    let store = Store::with_legal_name().await;
    let name_id = store.field(NAME).await["field"]["id"].clone();
    let legal_id = store.field(LEGAL).await["field"]["id"].clone();
    let out = store
        .call(
            "neighbors",
            json!({"id": name_id, "direction": "outgoing", "relations": ["contains"]}),
        )
        .await;
    let edge = out
        .as_array()
        .expect("neighbors")
        .iter()
        .find(|hit| hit["id"] == legal_id)
        .unwrap_or_else(|| panic!("the narrows edge: {out}"))["edge_id"]
        .clone();
    store
        .call("delete", json!({"id": edge, "kind": "edge", "hard": true}))
        .await;

    let legal = store.field(LEGAL).await;
    assert_eq!(legal["broader"], json!([]), "{legal}");
    let name = store.field(NAME).await;
    assert_eq!(name["narrower"], json!([]), "{name}");

    store.load(legal_form("LEGAL-3", 5)).await;
    let legal = store.field(LEGAL).await;
    assert_eq!(names(&legal["broader"]), [NAME], "{legal}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_field_with_a_narrower_field_is_sourced_and_replay_keeps_the_edge() {
    let document = |narrows: bool| {
        let legal = if narrows {
            narrowing(LEGAL, "text", NAME)
        } else {
            field(LEGAL, "text")
        };
        form(
            "LINT-1",
            40,
            json!([field(NAME, "text"), legal]),
            json!([asking("Legal name", 1, LEGAL)]),
        )
    };

    // The control: a field no line asks for and nothing narrows is unsourced.
    let bare = Store::new();
    bare.load(document(false)).await;
    let lint = bare.call("forms.lint", json!({})).await;
    assert!(
        lint["missing_source"]
            .as_array()
            .expect("missing_source")
            .iter()
            .any(|entry| entry["name"] == NAME),
        "{lint}"
    );

    let store = Store::new();
    store.load(document(true)).await;
    let lint = store.call("forms.lint", json!({})).await;
    assert_eq!(lint["missing_source"], json!([]), "{lint}");

    let exported = store.export().await;
    let empty = Store::new();
    let replayed = empty
        .call("forms.replay", json!({"document": exported.clone()}))
        .await;
    assert_eq!(replayed["replayed"], true, "{replayed}");
    let again = empty.export().await;
    assert_eq!(again["nodes"], exported["nodes"], "identical nodes");
    assert_eq!(again["edges"], exported["edges"], "identical edges");
    let legal = empty.field(LEGAL).await;
    assert_eq!(names(&legal["broader"]), [NAME], "{legal}");
}
