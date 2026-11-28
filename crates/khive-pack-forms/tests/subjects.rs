//! Subjects and lines: whose value a line asks for, how necessary it is, and
//! what a choice brings into play, run against an in-memory store with the kg
//! and forms packs installed.
//!
//! Form A declares a payer and a recipient and prints one identifier for each.
//! Form B declares an owner, three held items that belong to the owner, and a
//! postal address that the delivery choice "by post" brings into play.
//! Form C, further down, holds pairs of its lines to match or to differ.

use khive_pack_forms::FormsPack;
use khive_runtime::{KhiveRuntime, PackRegistry, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;
use serde_json::{json, Value};

use khive_pack_kg as _;

const A: &str = "SUBJ-A";
const B: &str = "SUBJ-B";
const G: &str = "SUBJ-G";
const ID: &str = "identification number";

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

    async fn check(&self, forms: &[&str], answers: Value) -> Value {
        self.call("forms.check", json!({"forms": forms, "answers": answers}))
            .await
    }

    /// A store with forms A and B loaded.
    async fn with_a_and_b() -> Self {
        let store = Self::new();
        store.load(form_a()).await;
        store.load(form_b()).await;
        store
    }
}

fn source(n: u8) -> Value {
    json!({"url": format!("https://forms.example/subjects-{n}.pdf"), "sha256": n.to_string().repeat(64)})
}

fn line(label: &str, order: u32, about: &str, field: &str) -> Value {
    json!({
        "label": label,
        "page": 1,
        "order": order,
        "item_kind": "text",
        "filled_by": about,
        "about": about,
        "field": field,
    })
}

fn id_field() -> Value {
    json!({"name": ID, "definition": "Number that identifies a party or a thing to the issuer.", "value_type": "identifier"})
}

fn form_a() -> Value {
    json!({
        "form_id": A,
        "revision": "2026-01",
        "title": "Payment between two parties",
        "source": source(1),
        "terms": {
            "kinds": ["party", "holding", {"name": "address", "definition": "A place mail is sent to."}],
            "roles": ["payer", "recipient", "owner", "holding", "delivery address"],
            "relations": ["belongs to"]
        },
        "subjects": [
            {"id": "payer", "kind": "party", "role": "payer"},
            {"id": "recipient", "kind": "party", "role": "recipient"}
        ],
        "fields": [id_field()],
        "items": [
            {"label": "PAYER'S ID", "page": 1, "order": 1, "item_kind": "text", "filled_by": "payer", "about": "payer", "field": ID},
            {"label": "RECIPIENT'S ID", "page": 1, "order": 2, "item_kind": "text", "filled_by": "payer", "about": "recipient", "field": ID}
        ]
    })
}

fn form_b() -> Value {
    let mut delivery = line("Delivery", 4, "owner", "delivery");
    delivery["item_kind"] = json!("choice");
    let mut former = line("Former name", 1, "owner", "legal name");
    former["state"] = json!("former");
    let mut new = line("New name", 2, "owner", "legal name");
    new["state"] = json!("new");
    let mut street = line("Street", 5, "postal address", "street");
    street["filled_by"] = json!("owner");
    let mut city = line("City", 6, "postal address", "city");
    city["filled_by"] = json!("owner");
    let mut identifier = line("Identifier", 3, "items", ID);
    identifier["filled_by"] = json!("owner");
    json!({
        "form_id": B,
        "revision": "2026-02",
        "title": "Change of name and holdings",
        "source": source(2),
        "subjects": [
            {"id": "owner", "kind": "party", "role": "owner"},
            {"id": "items", "kind": "holding", "role": "holding", "repeats": 3, "relations": [{"relation": "belongs to", "to": "owner"}]},
            {"id": "postal address", "kind": "address", "role": "delivery address"}
        ],
        "fields": [
            id_field(),
            {"name": "legal name", "definition": "The name a party is registered under.", "value_type": "text"},
            {
                "name": "delivery",
                "definition": "How the issuer delivers what it sends back.",
                "value_type": "choice",
                "exclusive": true,
                "values": [
                    {"name": "in person", "definition": "Collected at the issuer's office."},
                    {"name": "by post", "definition": "Mailed to a postal address."}
                ]
            },
            {"name": "street", "definition": "Street and number of an address.", "value_type": "text"},
            {"name": "city", "definition": "City of an address.", "value_type": "text"}
        ],
        "items": [former, new, identifier, delivery, street, city],
        "rules": [{
            "text": "If by post, give the postal address",
            "page": 1,
            "depends_on": [{"field": "delivery", "value": "by post"}],
            "enables": [{"subject": "postal address"}]
        }]
    })
}

fn kinds(findings: &Value, kind: &str) -> Vec<Value> {
    findings
        .as_array()
        .expect("findings")
        .iter()
        .filter(|f| f["kind"] == kind)
        .cloned()
        .collect()
}

fn missing(reply: &Value) -> Vec<Value> {
    kinds(&reply["findings"], "missing")
}

fn missing_of(reply: &Value, subject: &str, field: &str) -> Vec<Value> {
    missing(reply)
        .into_iter()
        .filter(|f| f["subject"] == subject && f["field"] == field)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_two_forms_share_one_field_per_meaning_without_warning() {
    let store = Store::new();
    let a = store.load(form_a()).await;
    let b = store.load(form_b()).await;
    assert_eq!(a["warnings"], json!([]), "{a}");
    assert_eq!(b["warnings"], json!([]), "{b}");
    assert_eq!(a["counts"]["subjects"], 2, "{a}");
    assert_eq!(a["counts"]["terms"], 9, "{a}");
    assert_eq!(b["counts"]["subjects"], 3, "{b}");
    assert_eq!(b["counts"]["terms"], 0, "B names terms A loaded: {b}");
    assert_eq!(b["counts"]["fields_reused"], 1, "{b}");

    let export = store.call("forms.export", json!({})).await;
    let fields: Vec<&Value> = export["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter(|n| n["type"] == "field")
        .collect();
    for name in [ID, "legal name"] {
        assert_eq!(
            fields.iter().filter(|f| f["name"] == name).count(),
            1,
            "{name}: {export}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn row_2_an_answer_fills_only_the_line_of_its_own_subject() {
    let store = Store::with_a_and_b().await;
    let reply = store
        .check(
            &[A],
            json!([{"subject": "payer", "field": ID, "value": "12-345"}]),
        )
        .await;
    assert!(missing_of(&reply, "payer", ID).is_empty(), "{reply}");
    let recipient = missing_of(&reply, "recipient", ID);
    assert_eq!(recipient.len(), 1, "{reply}");
    assert_eq!(recipient[0]["lines"][0]["label"], "RECIPIENT'S ID");
    assert_eq!(missing(&reply).len(), 1, "{reply}");
    assert_eq!(reply["consistent"], false);

    // An answer that names no subject fills no line about one.
    let reply = store
        .check(&[A], json!({"identification number": "12-345"}))
        .await;
    assert_eq!(missing(&reply).len(), 2, "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn row_3_an_answer_for_an_undeclared_subject_comes_back_unplaced() {
    let store = Store::with_a_and_b().await;
    let reply = store
        .check(
            &[A],
            json!([{"subject": "nobody", "field": ID, "value": "1"}]),
        )
        .await;
    let unplaced = reply["unplaced_answers"].as_array().expect("unplaced");
    assert_eq!(unplaced.len(), 1, "{reply}");
    assert_eq!(unplaced[0]["subject"], "nobody");
    assert!(
        unplaced[0]["reason"]
            .as_str()
            .is_some_and(|r| r.starts_with("no such subject")),
        "{reply}"
    );
    assert_eq!(reply["consistent"], false);

    // A declared subject with no line on that field is unplaced too.
    let reply = store
        .check(
            &[B],
            json!([{"subject": "owner", "field": ID, "value": "1"}]),
        )
        .await;
    assert!(
        reply["unplaced_answers"][0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("no line")),
        "{reply}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn row_4_rows_of_a_repeating_subject_are_answered_one_by_one() {
    let store = Store::with_a_and_b().await;
    let reply = store
        .check(
            &[B],
            json!([
                {"subject": "items", "field": ID, "row": 1, "value": "A-1"},
                {"subject": "items", "field": ID, "row": 3, "value": "A-3"}
            ]),
        )
        .await;
    let rows: Vec<Value> = missing_of(&reply, "items", ID)
        .iter()
        .map(|f| f["row"].clone())
        .collect();
    assert_eq!(rows, vec![json!(2)], "{reply}");
    assert_eq!(reply["unplaced_answers"], json!([]), "{reply}");

    let reply = store
        .check(
            &[B],
            json!([
                {"subject": "items", "field": ID, "row": 4, "value": "A-4"},
                {"subject": "items", "field": ID, "value": "A-?"}
            ]),
        )
        .await;
    let reasons: Vec<&str> = reply["unplaced_answers"]
        .as_array()
        .expect("unplaced")
        .iter()
        .filter_map(|u| u["reason"].as_str())
        .collect();
    assert_eq!(reasons.len(), 2, "{reply}");
    assert!(reasons[0].contains("has 3 row(s)"), "{reply}");
    assert!(reasons[1].contains("name a row"), "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn row_5_the_state_of_a_line_tells_former_from_new() {
    let store = Store::with_a_and_b().await;
    let reply = store
        .check(
            &[B],
            json!([{"subject": "owner", "field": "legal name", "state": "new", "value": "Kim Doe"}]),
        )
        .await;
    let names = missing_of(&reply, "owner", "legal name");
    assert_eq!(names.len(), 1, "{reply}");
    assert_eq!(names[0]["state"], "former");
    assert_eq!(names[0]["lines"][0]["label"], "Former name");

    let reply = store
        .check(
            &[B],
            json!([{"subject": "owner", "field": "legal name", "value": "Kim Doe"}]),
        )
        .await;
    assert!(
        reply["unplaced_answers"][0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("\"former\", \"new\"")),
        "{reply}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_6_and_7_a_choice_brings_a_subject_into_play() {
    let store = Store::with_a_and_b().await;
    let in_person = store
        .check(
            &[B],
            json!([{"subject": "owner", "field": "delivery", "value": "in person"}]),
        )
        .await;
    assert!(
        missing(&in_person)
            .iter()
            .all(|f| f["subject"] != "postal address"),
        "{in_person}"
    );
    assert_eq!(in_person["lines_not_asked"], 2, "{in_person}");
    assert!(kinds(&in_person["findings"], "rule_unmet").is_empty());

    let by_post = store
        .check(
            &[B],
            json!([{"subject": "owner", "field": "delivery", "value": "by post"}]),
        )
        .await;
    assert_eq!(by_post["lines_not_asked"], 0, "{by_post}");
    for field in ["street", "city"] {
        assert_eq!(
            missing_of(&by_post, "postal address", field).len(),
            1,
            "{field}: {by_post}"
        );
    }

    let filled = store
        .check(
            &[B],
            json!([
                {"subject": "owner", "field": "delivery", "value": "by post"},
                {"subject": "postal address", "field": "street", "value": "1 Main St"},
                {"subject": "postal address", "field": "city", "value": "Springfield"}
            ]),
        )
        .await;
    assert!(
        missing(&filled)
            .iter()
            .all(|f| f["subject"] != "postal address"),
        "{filled}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn row_9_an_empty_answer_set_leaves_every_required_line_missing() {
    let store = Store::with_a_and_b().await;
    let reply = store.check(&[A], json!([])).await;
    let subjects: Vec<Value> = missing(&reply)
        .iter()
        .map(|f| f["subject"].clone())
        .collect();
    assert_eq!(
        subjects,
        vec![json!("payer"), json!("recipient")],
        "{reply}"
    );
    assert_eq!(reply["consistent"], false);

    let reply = store
        .check(
            &[A],
            json!([
                {"subject": "payer", "field": ID, "value": "1"},
                {"subject": "recipient", "field": ID, "value": "2"}
            ]),
        )
        .await;
    assert_eq!(reply["consistent"], true, "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn row_10_an_item_about_an_undeclared_subject_refuses_the_form() {
    let store = Store::new();
    let mut document = form_a();
    document["items"][1]["about"] = json!("ghost");
    let message = store
        .refused("forms.load", json!({"document": document}))
        .await;
    assert!(message.contains("RECIPIENT'S ID"), "{message}");
    assert!(message.contains("ghost"), "{message}");

    let mut document = form_a();
    document["items"][0]
        .as_object_mut()
        .expect("item")
        .remove("about");
    let message = store
        .refused("forms.load", json!({"document": document}))
        .await;
    assert!(message.contains("names no subject"), "{message}");

    let mut document = form_a();
    document["subjects"][0]["role"] = json!("stranger");
    let message = store
        .refused("forms.load", json!({"document": document}))
        .await;
    assert!(message.contains("not a loaded role"), "{message}");

    let mut document = form_b();
    document["subjects"][1]["relations"][0]["to"] = json!("landlord");
    store.load(form_a()).await;
    let message = store
        .refused("forms.load", json!({"document": document}))
        .await;
    assert!(message.contains("landlord"), "{message}");

    // Nothing of a refused document was written.
    let export = store.call("forms.export", json!({})).await;
    assert!(
        export["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .all(|n| n["key"].as_str().is_none_or(|k| !k.contains(B))),
        "{export}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn row_11_export_replay_export_is_identical_with_subjects() {
    let store = Store::with_a_and_b().await;
    let exported = store.call("forms.export", json!({})).await;
    let subjects = exported["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter(|n| n["type"] == "form_subject")
        .count();
    assert_eq!(subjects, 5, "{exported}");
    assert!(
        exported["edges"]
            .as_array()
            .expect("edges")
            .iter()
            .any(|e| e["relation"] == "enables"
                && e["target"]
                    .as_str()
                    .is_some_and(|t| t.ends_with("/subject:postal address"))),
        "{exported}"
    );

    let empty = Store::new();
    empty
        .call("forms.replay", json!({"document": exported.clone()}))
        .await;
    let again = empty.call("forms.export", json!({})).await;
    assert_eq!(again["nodes"], exported["nodes"]);
    assert_eq!(again["edges"], exported["edges"]);
    assert_eq!(again["log"], exported["log"]);

    // The replayed graph checks the same.
    let reply = empty
        .check(
            &[B],
            json!([{"subject": "owner", "field": "delivery", "value": "by post"}]),
        )
        .await;
    assert_eq!(
        missing_of(&reply, "postal address", "city").len(),
        1,
        "{reply}"
    );
}

fn form_g() -> Value {
    let mut guardian_for = line("Guardian for", 1, "guardian", "person cared for");
    guardian_for["selects"] = json!(["minor one", "minor two"]);
    let mut phone = line("Phone", 2, "guardian", "phone number");
    phone["necessity"] = json!("one_of");
    phone["group"] = json!("contact");
    let mut email = line("Email", 3, "guardian", "email address");
    email["necessity"] = json!("one_of");
    email["group"] = json!("contact");
    let mut dependant = line("Dependant name", 4, "dependants", "legal name");
    dependant["necessity"] = json!("at_least_one_row");
    dependant["filled_by"] = json!("guardian");
    let mut note = line("Note", 5, "guardian", "note");
    note["necessity"] = json!("optional");
    json!({
        "form_id": G,
        "revision": "2026-04",
        "title": "Guardianship",
        "source": source(3),
        "terms": {"roles": ["minor", "guardian"]},
        "subjects": [
            {"id": "minor one", "kind": "party", "role": "minor", "qualifier": "first"},
            {"id": "minor two", "kind": "party", "role": "minor", "qualifier": "additional"},
            {"id": "guardian", "kind": "party", "role": "guardian"},
            {"id": "dependants", "kind": "party", "role": "minor", "repeats": 2}
        ],
        "fields": [
            {"name": "person cared for", "definition": "Which listed person a guardian acts for.", "value_type": "text"},
            {"name": "phone number", "definition": "A number a party is called on.", "value_type": "identifier"},
            {"name": "email address", "definition": "An address a party is mailed at.", "value_type": "text"},
            {"name": "note", "definition": "Free text added by the filer.", "value_type": "text"}
        ],
        "items": [guardian_for, phone, email, dependant, note]
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_line_selects_only_its_subjects_and_groups_and_rows_count_once() {
    let store = Store::with_a_and_b().await;
    store.load(form_g()).await;

    let reply = store.check(&[G], json!([])).await;
    let open = missing(&reply);
    let group: Vec<&Value> = open.iter().filter(|f| f["group"] == "contact").collect();
    assert_eq!(group.len(), 1, "{reply}");
    assert_eq!(group[0]["lines"].as_array().map(Vec::len), Some(2));
    let rows = missing_of(&reply, "dependants", "legal name");
    assert_eq!(rows.len(), 1, "{reply}");
    assert_eq!(rows[0]["row"], Value::Null);
    assert!(rows[0]["message"]
        .as_str()
        .is_some_and(|m| m.contains("at least one row")));
    assert!(missing_of(&reply, "guardian", "note").is_empty(), "{reply}");
    assert_eq!(
        open.len(),
        3,
        "guardian for, the contact group, a dependant: {reply}"
    );

    let reply = store
        .check(
            &[G],
            json!([
                {"subject": "guardian", "field": "person cared for", "value": "minor two"},
                {"subject": "guardian", "field": "email address", "value": "g@example.org"},
                {"subject": "dependants", "field": "legal name", "row": 2, "value": "Sam Doe"}
            ]),
        )
        .await;
    assert_eq!(reply["consistent"], true, "{reply}");

    let reply = store
        .check(
            &[G],
            json!([{"subject": "guardian", "field": "person cared for", "value": "someone else"}]),
        )
        .await;
    let outside = kinds(&reply["findings"], "subject_outside_selection");
    assert_eq!(outside.len(), 1, "{reply}");
    assert_eq!(outside[0]["allowed"], json!(["minor one", "minor two"]));
    assert_eq!(
        missing_of(&reply, "guardian", "person cared for").len(),
        1,
        "an answer outside the selection fills nothing: {reply}"
    );

    // A subject only one of the checked forms declares needs no form named.
    let reply = store
        .check(
            &[B, G],
            json!([{"subject": "dependants", "field": "legal name", "row": 1, "value": "Sam Doe"}]),
        )
        .await;
    assert_eq!(reply["unplaced_answers"], json!([]), "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_role_lists_every_form_and_field_asked_of_it() {
    let store = Store::with_a_and_b().await;
    let payer = store
        .call("forms.neighbourhood", json!({"node": "role:payer"}))
        .await;
    assert_eq!(payer["term"]["list"], "role", "{payer}");
    let subjects = payer["subjects"].as_array().expect("subjects");
    assert_eq!(subjects.len(), 1, "{payer}");
    assert_eq!(subjects[0]["form"], format!("form:{A}@2026-01"));
    assert_eq!(subjects[0]["fields"], json!([ID]));

    let field = store.call("forms.neighbourhood", json!({"node": ID})).await;
    let roles: Vec<Value> = field["asked_of"]
        .as_array()
        .expect("asked_of")
        .iter()
        .map(|a| a["role"].clone())
        .collect();
    assert_eq!(roles.len(), 3, "{field}");
    for role in ["payer", "recipient", "holding"] {
        assert!(roles.contains(&json!(role)), "{role}: {field}");
    }

    let form = store.call("forms.neighbourhood", json!({"node": B})).await;
    let items = form["subjects"]
        .as_array()
        .expect("subjects")
        .iter()
        .find(|s| s["id"] == "items")
        .cloned()
        .expect("items subject");
    assert_eq!(items["repeats"], 3);
    assert_eq!(
        items["relations"],
        json!([{"relation": "belongs to", "to": "owner"}])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn add_item_on_a_form_with_subjects_names_whom_it_is_about() {
    let store = Store::with_a_and_b().await;
    let item = json!({"label": "Payer's second ID", "page": 2, "order": 9, "item_kind": "text", "filled_by": "payer"});
    let message = store
        .refused(
            "forms.add_item",
            json!({"form": A, "item": item.clone(), "same_as": ID}),
        )
        .await;
    assert!(message.contains("names no subject"), "{message}");

    let mut about = item;
    about["about"] = json!("payer");
    about["state"] = json!("also known as");
    let added = store
        .call(
            "forms.add_item",
            json!({"form": A, "item": about, "same_as": ID}),
        )
        .await;
    assert_eq!(added["status"], "same", "{added}");
    let reply = store
        .check(
            &[A],
            json!([{"subject": "payer", "field": ID, "value": "1"}]),
        )
        .await;
    let payer = missing_of(&reply, "payer", ID);
    assert_eq!(payer.len(), 1, "{reply}");
    assert_eq!(payer[0]["state"], "also known as");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_naming_no_subject_is_unplaced_where_only_subjects_are_asked() {
    let store = Store::with_a_and_b().await;
    let reply = store.check(&[B], json!({"delivery": "by post"})).await;
    let unplaced = reply["unplaced_answers"].as_array().expect("unplaced");
    assert_eq!(unplaced.len(), 1, "{reply}");
    assert!(
        unplaced[0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("\"owner\"") && r.contains("names the subject")),
        "{reply}"
    );
    // It holds no premise: the postal address is not brought into play.
    assert!(
        missing_of(&reply, "postal address", "street").is_empty(),
        "{reply}"
    );
    assert_eq!(missing_of(&reply, "owner", "delivery").len(), 1, "{reply}");
}

const X: &str = "SUBJ-X";

fn form_x() -> Value {
    json!({
        "form_id": X,
        "revision": "2026-06",
        "title": "Supplement without subjects",
        "source": source(5),
        "fields": [
            id_field(),
            {"name": "trigger", "definition": "Whether the supplement applies.", "value_type": "text"}
        ],
        "items": [
            {"label": "Applies", "page": 1, "order": 1, "item_kind": "text", "filled_by": "filer", "field": "trigger"},
            {"label": "Number", "page": 1, "order": 2, "item_kind": "text", "filled_by": "filer", "field": ID}
        ],
        "rules": [{
            "text": "If it applies, give the number",
            "page": 1,
            "depends_on": [{"field": "trigger"}],
            "enables": [{"item": "Number"}]
        }]
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn on_a_form_without_subjects_an_answer_to_a_line_its_rule_does_not_bring_into_play_is_not_asked(
) {
    let store = Store::new();
    store.load(form_x()).await;

    // The number alone: the rule that asks for it does not hold.
    let reply = store.check(&[X], json!({ID: "7"})).await;
    assert_eq!(reply["findings"], json!([]), "{reply}");
    let not_asked = reply["answers_not_asked"]
        .as_array()
        .expect("answers_not_asked");
    assert_eq!(not_asked.len(), 1, "{reply}");
    assert_eq!(not_asked[0]["field"], ID);
    assert_eq!(reply["consistent"], false);

    // With the premise given, the same answer is asked and the check holds.
    let reply = store.check(&[X], json!({"trigger": "yes", ID: "7"})).await;
    assert_eq!(reply["answers_not_asked"], json!([]), "{reply}");
    assert_eq!(reply["findings"], json!([]), "{reply}");
    assert_eq!(reply["consistent"], true, "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subject_answer_fills_no_line_about_none_on_another_form() {
    let store = Store::with_a_and_b().await;
    store.load(form_x()).await;
    let reply = store
        .check(
            &[A, X],
            json!([
                {"field": "trigger", "value": "yes"},
                {"subject": "payer", "field": ID, "value": "1"}
            ]),
        )
        .await;
    let unmet = kinds(&reply["findings"], "rule_unmet");
    assert_eq!(unmet.len(), 1, "{reply}");
    assert_eq!(unmet[0]["unmet"][0]["name"], "Number");

    // The number given for no subject meets the rule.
    let reply = store
        .check(
            &[A, X],
            json!([
                {"field": "trigger", "value": "yes"},
                {"field": ID, "value": "1"},
                {"subject": "payer", "field": ID, "value": "1"}
            ]),
        )
        .await;
    assert!(
        kinds(&reply["findings"], "rule_unmet").is_empty(),
        "{reply}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_line_bound_to_no_field_stays_open() {
    let store = Store::new();
    let mut document = form_a();
    document["items"][1]
        .as_object_mut()
        .expect("item")
        .remove("field");
    let loaded = store.load(document).await;
    assert!(
        loaded["warnings"].to_string().contains("names no field"),
        "{loaded}"
    );
    let reply = store
        .check(
            &[A],
            json!([{"subject": "payer", "field": ID, "value": "1"}]),
        )
        .await;
    let open = kinds(&reply["findings"], "line_without_field");
    assert_eq!(open.len(), 1, "{reply}");
    assert_eq!(open[0]["line"]["label"], "RECIPIENT'S ID");
    assert_eq!(open[0]["subject"], "recipient");
    assert_eq!(reply["consistent"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_of_line_counts_for_its_group_when_it_shares_a_slot() {
    let store = Store::new();
    let required = line("First", 1, "s", "first");
    let mut either = line("First again", 2, "s", "first");
    either["necessity"] = json!("one_of");
    either["group"] = json!("g");
    let mut or = line("Second", 3, "s", "second");
    or["necessity"] = json!("one_of");
    or["group"] = json!("g");
    store
        .load(json!({
            "form_id": "SUBJ-S",
            "revision": "2026-07",
            "title": "Shared slot",
            "source": source(6),
            "terms": {"kinds": ["party"], "roles": ["payer"]},
            "subjects": [{"id": "s", "kind": "party", "role": "payer"}],
            "fields": [
                {"name": "first", "definition": "A first value.", "value_type": "text"},
                {"name": "second", "definition": "A second value.", "value_type": "text"}
            ],
            "items": [required, either, or]
        }))
        .await;
    let reply = store
        .check(
            &["SUBJ-S"],
            json!([{"subject": "s", "field": "first", "value": "x"}]),
        )
        .await;
    assert_eq!(missing(&reply), Vec::<Value>::new(), "{reply}");
    assert_eq!(reply["consistent"], true, "{reply}");

    // With nothing answered, the group is open once beside the required line.
    let reply = store.check(&["SUBJ-S"], json!([])).await;
    let groups: Vec<Value> = missing(&reply)
        .into_iter()
        .filter(|f| f["group"] == "g")
        .collect();
    assert_eq!(groups.len(), 1, "{reply}");
    assert_eq!(groups[0]["lines"].as_array().map(Vec::len), Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn repeats_is_bounded_at_load() {
    let store = Store::new();
    let mut document = form_a();
    document["subjects"][0]["repeats"] = json!(1_000_000_000u64);
    let message = store
        .refused("forms.load", json!({"document": document}))
        .await;
    assert!(message.contains("from 1 to 100"), "{message}");

    let mut document = form_a();
    document["items"][0]["repeats"] = json!(101);
    let message = store
        .refused("forms.load", json!({"document": document}))
        .await;
    assert!(message.contains("from 1 to 100"), "{message}");

    let mut document = form_a();
    document["subjects"][0]["repeats"] = json!(100);
    store.load(document).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_for_a_line_not_asked_holds_no_premise() {
    let store = Store::new();
    store.load(form_a()).await;
    let mut document = form_b();
    document["subjects"]
        .as_array_mut()
        .expect("subjects")
        .push(json!({"id": "other", "kind": "party", "role": "owner"}));
    document["items"].as_array_mut().expect("items").push(line(
        "Other's name",
        7,
        "other",
        "legal name",
    ));
    document["rules"]
        .as_array_mut()
        .expect("rules")
        .push(json!({
            "text": "If a street is given, name the other party",
            "page": 1,
            "depends_on": [{"field": "street"}],
            "enables": [{"subject": "other"}]
        }));
    store.load(document).await;

    let reply = store
        .check(
            &[B],
            json!([
                {"subject": "owner", "field": "delivery", "value": "in person"},
                {"subject": "postal address", "field": "street", "value": "1 Main St"}
            ]),
        )
        .await;
    let not_asked = reply["answers_not_asked"]
        .as_array()
        .expect("answers_not_asked");
    assert_eq!(not_asked.len(), 1, "{reply}");
    assert_eq!(not_asked[0]["subject"], "postal address");
    assert!(
        missing_of(&reply, "other", "legal name").is_empty(),
        "{reply}"
    );
    assert_eq!(reply["consistent"], false);

    // By post brings the address into play, and its street the other party.
    let reply = store
        .check(
            &[B],
            json!([
                {"subject": "owner", "field": "delivery", "value": "by post"},
                {"subject": "postal address", "field": "street", "value": "1 Main St"}
            ]),
        )
        .await;
    assert_eq!(reply["answers_not_asked"], json!([]), "{reply}");
    assert_eq!(
        missing_of(&reply, "other", "legal name").len(),
        1,
        "{reply}"
    );
}

const C: &str = "SUBJ-C";

/// Form C declares an owner, an account held at a receiver, and constraints
/// across its lines: the holder's name twice, the account number twice, two
/// tax numbers that must differ, and two banks compared by a caller's key.
fn form_c() -> Value {
    let by_owner = |label: &str, order: u32, about: &str, field: &str| {
        let mut item = line(label, order, about, field);
        item["filled_by"] = json!("owner");
        item
    };
    let mut on_account = by_owner("Name on the account", 2, "account", "holder name");
    on_account["known_to"] = json!("receiver");
    let mut number = by_owner("Account number", 3, "account", "account number");
    number["state"] = json!("given");
    number["necessity"] = json!("required");
    number["known_to"] = json!({"relation": "held at"});
    let mut again = by_owner("Account number again", 4, "account", "account number");
    again["state"] = json!("repeated");
    json!({
        "form_id": C,
        "revision": "2026-03",
        "title": "Account held at another party",
        "source": source(3),
        "terms": {
            "kinds": ["party", "holding"],
            "roles": ["owner", "holding", "receiver"],
            "relations": ["held at"],
            "purposes": [{"name": "account opening", "definition": "Opening an account for a party."}]
        },
        "purposes": ["account opening"],
        "subjects": [
            {"id": "owner", "kind": "party", "role": "owner"},
            {"id": "account", "kind": "holding", "role": "holding", "relations": [{"relation": "held at", "to": "receiver"}]},
            {"id": "receiver", "kind": "party", "role": "receiver"}
        ],
        "fields": [
            {"name": "holder name", "definition": "The name an account is held under.", "value_type": "text", "comparison": "folded"},
            {"name": "account number", "definition": "Number that identifies an account to the party holding it.", "value_type": "identifier", "comparison": "digits"},
            {"name": "tax number", "definition": "Number a tax authority gives a party.", "value_type": "identifier", "comparison": "digits"},
            {"name": "bank", "definition": "The institution an account is held at.", "value_type": "text", "comparison": "key"}
        ],
        "items": [
            line("Holder name", 1, "owner", "holder name"),
            on_account,
            number,
            again,
            line("Owner's tax number", 5, "owner", "tax number"),
            by_owner("Receiver's tax number", 6, "receiver", "tax number"),
            by_owner("Bank", 7, "account", "bank"),
            by_owner("Receiving bank", 8, "receiver", "bank")
        ],
        "constraints": [
            {"must": "match", "text": "The name on the account is the holder's name", "lines": [{"subject": "owner", "field": "holder name"}, {"subject": "account", "field": "holder name"}]},
            {"must": "match", "text": "Give the account number twice", "lines": [{"subject": "account", "field": "account number", "state": "given"}, {"subject": "account", "field": "account number", "state": "repeated"}]},
            {"must": "differ", "text": "The receiver is not the owner", "lines": [{"subject": "owner", "field": "tax number"}, {"subject": "receiver", "field": "tax number"}]},
            {"must": "match", "lines": [{"subject": "account", "field": "bank"}, {"subject": "receiver", "field": "bank"}]}
        ]
    })
}

/// Answers to every line of form C that meet its constraints, with the
/// repeated account number as given.
fn c_answers(repeated: &str) -> Value {
    json!([
        {"subject": "owner", "field": "holder name", "value": "Ada Lovelace"},
        {"subject": "account", "field": "holder name", "value": "  ada   LOVELACE "},
        {"subject": "account", "field": "account number", "state": "given", "value": "12-345"},
        {"subject": "account", "field": "account number", "state": "repeated", "value": repeated},
        {"subject": "owner", "field": "tax number", "value": "111-22"},
        {"subject": "receiver", "field": "tax number", "value": "333 44"},
        {"subject": "account", "field": "bank", "value": "First Bank", "key": "FB-1"},
        {"subject": "receiver", "field": "bank", "value": "First Bank plc", "key": "FB-1"}
    ])
}

fn item_labels(finding: &Value) -> Vec<Value> {
    finding["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|l| l["item"].clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn row_8_a_must_match_pair_answered_differently_is_one_finding_naming_both_lines() {
    let store = Store::new();
    store.load(form_c()).await;

    let agreed = store.check(&[C], c_answers("12 345")).await;
    assert_eq!(agreed["findings"], json!([]), "{agreed}");
    assert_eq!(agreed["constraints_unchecked"], json!([]), "{agreed}");
    assert_eq!(agreed["consistent"], true, "{agreed}");

    let reply = store.check(&[C], c_answers("12 346")).await;
    assert_eq!(
        reply["findings"].as_array().map(Vec::len),
        Some(1),
        "{reply}"
    );
    let unmet = kinds(&reply["findings"], "constraint_unmet");
    assert_eq!(unmet.len(), 1, "{reply}");
    assert_eq!(unmet[0]["must"], "match");
    assert_eq!(unmet[0]["text"], "Give the account number twice");
    assert_eq!(
        item_labels(&unmet[0]),
        [json!("Account number"), json!("Account number again")],
        "{reply}"
    );
    assert_eq!(reply["consistent"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn each_field_compares_its_answers_its_own_way() {
    let store = Store::new();
    store.load(form_c()).await;
    let with = |index: usize, answer: Value| {
        let mut answers = c_answers("12 345");
        answers[index] = answer;
        answers
    };
    let unmet = |reply: &Value| kinds(&reply["findings"], "constraint_unmet");

    // Folded: case and spacing do not count, a different name does.
    let reply = store
        .check(
            &[C],
            with(
                1,
                json!({"subject": "account", "field": "holder name", "value": "Ada King"}),
            ),
        )
        .await;
    assert_eq!(unmet(&reply).len(), 1, "{reply}");
    assert_eq!(
        unmet(&reply)[0]["text"],
        "The name on the account is the holder's name"
    );

    // Digits: the same digits under other punctuation are the same, so a
    // pair that must differ is a finding.
    let reply = store
        .check(
            &[C],
            with(
                5,
                json!({"subject": "receiver", "field": "tax number", "value": "(111) 22"}),
            ),
        )
        .await;
    assert_eq!(unmet(&reply).len(), 1, "{reply}");
    assert_eq!(unmet(&reply)[0]["must"], "differ");
    assert!(
        unmet(&reply)[0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("are the same")),
        "{reply}"
    );

    // Key: different wording under one key matches, a different key does not.
    let reply = store
        .check(
            &[C],
            with(
                7,
                json!({"subject": "receiver", "field": "bank", "value": "First Bank", "key": "FB-2"}),
            ),
        )
        .await;
    assert_eq!(unmet(&reply).len(), 1, "{reply}");
    assert_eq!(
        item_labels(&unmet(&reply)[0]),
        [json!("Bank"), json!("Receiving bank")]
    );

    // Key: an answer that gives no key leaves the constraint unchecked.
    let reply = store
        .check(
            &[C],
            with(
                7,
                json!({"subject": "receiver", "field": "bank", "value": "First Bank"}),
            ),
        )
        .await;
    assert_eq!(unmet(&reply), Vec::<Value>::new(), "{reply}");
    let unchecked = reply["constraints_unchecked"]
        .as_array()
        .expect("constraints_unchecked");
    assert_eq!(unchecked.len(), 1, "{reply}");
    assert!(
        unchecked[0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("gives no key")),
        "{reply}"
    );
    assert_eq!(reply["consistent"], false);

    // A line with no answer is reported missing, not compared.
    let mut answers = c_answers("12 346");
    answers.as_array_mut().expect("answers").remove(3);
    let reply = store.check(&[C], answers).await;
    assert_eq!(unmet(&reply), Vec::<Value>::new(), "{reply}");
    assert_eq!(reply["constraints_unchecked"], json!([]), "{reply}");
    let open = missing(&reply);
    assert_eq!(open.len(), 1, "{reply}");
    assert_eq!(open[0]["lines"][0]["label"], "Account number again");
}

/// The missing finding for the account number as first given.
fn given_number_missing(reply: &Value) -> Vec<Value> {
    missing_of(reply, "account", "account number")
        .into_iter()
        .filter(|f| f["state"] == "given")
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_constraint_names_two_printed_lines_that_compare_one_way() {
    let store = Store::new();
    let holder = json!({"subject": "owner", "field": "holder name"});
    let cases = [
        (
            json!([{"must": "match", "lines": [{"subject": "receiver", "field": "holder name"}, holder]}]),
            "which the form does not print",
        ),
        (
            json!([{"must": "match", "lines": [holder, {"subject": "owner", "field": "tax number"}]}]),
            "both lines of a constraint compare one way",
        ),
        (
            json!([{"must": "match", "lines": [holder]}]),
            "this one names 1",
        ),
        (
            json!([{"must": "match", "lines": [holder, {"subject": "Owner", "field": "Holder  Name"}]}]),
            "twice",
        ),
        (
            json!([{"must": "equal", "lines": [holder, {"subject": "account", "field": "holder name"}]}]),
            "must must be one of match, differ",
        ),
        (
            json!([{"must": "match", "lines": [{"field": "holder name"}, holder]}]),
            "without a subject",
        ),
        (
            json!([{"must": "match", "lines": [{"subject": "bank", "field": "holder name"}, holder]}]),
            "does not declare \"bank\"",
        ),
    ];
    for (constraints, expected) in cases {
        let mut document = form_c();
        document["constraints"] = constraints;
        let message = store
            .refused("forms.load", json!({"document": document}))
            .await;
        assert!(message.contains(expected), "{expected}: {message}");
    }
    // Nothing of the refused documents stayed behind.
    store.load(form_c()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn purposes_known_to_and_comparisons_are_checked_at_load() {
    let store = Store::new();
    let refused = |document: Value, expected: &'static str| {
        let store = &store;
        async move {
            let message = store
                .refused("forms.load", json!({"document": document}))
                .await;
            assert!(message.contains(expected), "{expected}: {message}");
        }
    };

    let mut document = form_c();
    document["purposes"] = json!(["account closing"]);
    refused(document, "which is not a loaded purpose").await;

    let mut document = form_c();
    document["items"][0]["known_to"] = json!({"relation": "held at"});
    refused(document, "names no subject under that relation").await;

    let mut document = form_c();
    document["items"][0]["known_to"] = json!("bank");
    refused(document, "(known_to)").await;

    let mut document = form_c();
    document["fields"][1]["comparison"] = json!("soundex");
    refused(
        document,
        "comparison must be one of exact, folded, digits, key",
    )
    .await;

    store.load(form_c()).await;
    // A field compares one way on every form that prints it.
    let mut revision = form_c();
    revision["revision"] = json!("2026-04");
    revision["source"] = source(4);
    revision.as_object_mut().expect("document").remove("terms");
    revision["fields"][1]["comparison"] = json!("exact");
    refused(
        revision,
        "field \"account number\" compares its answers by \"digits\"; the document says \"exact\"",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_purpose_lists_its_forms_and_a_line_names_who_already_holds_its_value() {
    let store = Store::new();
    store.load(form_c()).await;

    let purpose = store
        .call(
            "forms.neighbourhood",
            json!({"node": "purpose:account opening"}),
        )
        .await;
    assert_eq!(purpose["term"]["list"], "purpose", "{purpose}");
    let forms = purpose["forms"].as_array().expect("forms");
    assert_eq!(forms.len(), 1, "{purpose}");
    assert_eq!(forms[0]["form_id"], C);

    let form = store.call("forms.neighbourhood", json!({"node": C})).await;
    assert_eq!(form["purposes"], json!(["account opening"]), "{form}");
    assert_eq!(
        form["constraints"].as_array().map(Vec::len),
        Some(4),
        "{form}"
    );

    // Named directly, and derived through the account's relation.
    for (field, item) in [
        ("holder name", "Name on the account"),
        ("account number", "Account number"),
    ] {
        let reply = store
            .call("forms.neighbourhood", json!({"node": field}))
            .await;
        let asked = reply["asked_of"]
            .as_array()
            .expect("asked_of")
            .iter()
            .find(|a| a["item"] == item)
            .cloned()
            .expect("the line");
        assert_eq!(asked["known_to"], "receiver", "{reply}");
    }

    let reply = store.check(&[C], json!([])).await;
    let open = given_number_missing(&reply);
    assert_eq!(open.len(), 1, "{reply}");
    assert_eq!(open[0]["filled_by"], "owner");
    assert_eq!(open[0]["known_to"], "receiver");
}

#[tokio::test(flavor = "multi_thread")]
async fn constraints_purposes_and_known_to_survive_export_and_replay() {
    let store = Store::new();
    store.load(form_c()).await;
    let exported = store.call("forms.export", json!({})).await;

    let empty = Store::new();
    empty
        .call("forms.replay", json!({"document": exported.clone()}))
        .await;
    let again = empty.call("forms.export", json!({})).await;
    assert_eq!(again["nodes"], exported["nodes"]);
    assert_eq!(again["edges"], exported["edges"]);
    assert_eq!(again["log"], exported["log"]);

    let reply = empty.check(&[C], c_answers("12 346")).await;
    assert_eq!(
        kinds(&reply["findings"], "constraint_unmet").len(),
        1,
        "{reply}"
    );
    let purpose = empty
        .call(
            "forms.neighbourhood",
            json!({"node": "purpose:account opening"}),
        )
        .await;
    assert_eq!(
        purpose["forms"].as_array().map(Vec::len),
        Some(1),
        "{purpose}"
    );
    let open = given_number_missing(&empty.check(&[C], json!([])).await);
    assert_eq!(open.len(), 1);
    assert_eq!(open[0]["known_to"], "receiver");
}
