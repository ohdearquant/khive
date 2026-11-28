//! Document types: what kind of document a form revision is, the lines that
//! state a value every document of a type carries, and the lines that require
//! a document of another type, run against an in-memory store with the kg and
//! forms packs installed.
//!
//! The identity document is a type issued by a third party. Its form states
//! the holder's legal name and date of birth and asks for nothing. The
//! application form asks for the applicant's legal name and requires an
//! identity document attached.

use khive_pack_forms::FormsPack;
use khive_runtime::{KhiveRuntime, PackRegistry, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;
use serde_json::{json, Value};

use khive_pack_kg as _;

const IDENTITY: &str = "IDENT-1";
const APPLICATION: &str = "APPLY-1";
const IDENTITY_TYPE: &str = "identity-document";
const NAME: &str = "legal name";
const BIRTH: &str = "date of birth";

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

    /// A store with the identity document and the application loaded.
    async fn with_both() -> Self {
        let store = Self::new();
        store.load(identity_form()).await;
        store.load(application_form()).await;
        store
    }
}

fn source(n: u8) -> Value {
    json!({"url": format!("https://forms.example/types-{n}.pdf"), "sha256": n.to_string().repeat(64)})
}

fn fields() -> Value {
    json!([
        {"name": NAME, "definition": "The name a party is registered under.", "value_type": "text"},
        {"name": BIRTH, "definition": "The day a person was born.", "value_type": "text"}
    ])
}

fn stating(label: &str, order: u32, field: &str) -> Value {
    json!({
        "label": label,
        "page": 1,
        "order": order,
        "item_kind": "text",
        "relation": "states",
        "about": "holder",
        "field": field,
    })
}

fn identity_form() -> Value {
    json!({
        "form_id": IDENTITY,
        "revision": "2026-01",
        "title": "Identity card",
        "source": source(1),
        "document_type": {"id": IDENTITY_TYPE, "name": "Identity document", "origin": "third_party", "issuer_class": "government"},
        "subjects": [{"id": "holder", "kind": "party", "role": "holder"}],
        "terms": {"kinds": ["party"], "roles": ["holder", "applicant"]},
        "fields": fields(),
        "items": [stating("Name", 1, NAME), stating("Born", 2, BIRTH)]
    })
}

fn requiring(label: &str, document_type: &str) -> Value {
    json!({
        "label": label,
        "page": 1,
        "order": 2,
        "item_kind": "attachment",
        "relation": "requires",
        "filled_by": "applicant",
        "document_type": document_type,
    })
}

fn application_form() -> Value {
    json!({
        "form_id": APPLICATION,
        "revision": "2026-01",
        "title": "Application",
        "source": source(2),
        "document_type": {"id": "application", "name": "Application", "origin": "applicant"},
        "subjects": [{"id": "applicant", "kind": "party", "role": "applicant"}],
        "fields": fields(),
        "items": [
            {"label": "Your legal name", "page": 1, "order": 1, "item_kind": "text", "filled_by": "applicant", "about": "applicant", "field": NAME},
            requiring("Copy of your identity document", IDENTITY_TYPE)
        ]
    })
}

/// The application as the first document of a store, naming the terms the
/// identity document would otherwise have loaded.
fn application_alone() -> Value {
    let mut document = application_form();
    document["terms"] = json!({"kinds": ["party"], "roles": ["holder", "applicant"]});
    document
}

fn node<'a>(export: &'a Value, kind: &str, name: &str) -> &'a Value {
    export["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n["type"] == kind && n["name"] == name)
        .unwrap_or_else(|| panic!("no {kind} named {name}: {export}"))
}

fn contains(export: &Value, source: &str, target_prefix: &str) -> bool {
    export["edges"].as_array().expect("edges").iter().any(|e| {
        e["relation"] == "contains"
            && e["source"] == source
            && e["target"]
                .as_str()
                .is_some_and(|t| t.starts_with(target_prefix))
    })
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
async fn a_declared_type_holds_its_form_by_contains_and_keeps_what_it_declares() {
    let store = Store::new();
    let reply = store.load(identity_form()).await;
    assert_eq!(reply["document_type"]["minted"], false, "{reply}");
    assert_eq!(
        reply["document_type"]["key"],
        format!("document_type:{IDENTITY_TYPE}"),
        "{reply}"
    );
    assert_eq!(reply["counts"]["document_types"], 1, "{reply}");

    let export = store.export().await;
    let kind = node(&export, "document_type", "Identity document");
    assert_eq!(kind["key"], format!("document_type:{IDENTITY_TYPE}"));
    assert_eq!(kind["properties"]["origin"], "third_party", "{kind}");
    assert_eq!(kind["properties"]["issuer_class"], "government", "{kind}");
    assert!(kind["properties"].get("minted").is_none(), "{kind}");
    assert!(
        contains(
            &export,
            &format!("document_type:{IDENTITY_TYPE}"),
            &format!("form:{IDENTITY}@")
        ),
        "the type holds its revision by contains: {export}"
    );

    let lint = store.call("forms.lint", json!({})).await;
    assert_eq!(lint["minted_document_types"], json!([]), "{lint}");
    assert_eq!(lint["forms_without_document_type"], json!([]), "{lint}");
    assert_eq!(lint["missing_source"], json!([]), "{lint}");
    assert_eq!(
        lint["missing_required"],
        json!([]),
        "a stating line has nobody who fills it in and is not short of one: {lint}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stating_line_is_never_asked_and_an_answer_to_it_comes_back_unplaced() {
    let store = Store::with_both().await;
    let reply = store
        .call("forms.check", json!({"forms": [IDENTITY], "answers": []}))
        .await;
    assert!(
        missing(&reply).is_empty(),
        "the identity document states its values and asks for none: {reply}"
    );

    let reply = store
        .call(
            "forms.check",
            json!({"forms": [IDENTITY], "answers": [{"subject": "holder", "field": NAME, "value": "Kim Doe"}]}),
        )
        .await;
    let unplaced = reply["unplaced_answers"].as_array().expect("unplaced");
    assert_eq!(unplaced.len(), 1, "{reply}");
    let reason = unplaced[0]["reason"].as_str().expect("reason");
    assert!(
        reason.contains("the line \"Name\"") && reason.contains("a stating line is never asked"),
        "{reply}"
    );

    // The same field asked of the applicant is placed as before.
    let reply = store
        .call(
            "forms.check",
            json!({"forms": [APPLICATION], "answers": [{"subject": "applicant", "field": NAME, "value": "Kim Doe"}]}),
        )
        .await;
    assert_eq!(reply["unplaced_answers"], json!([]), "{reply}");
    assert!(
        missing(&reply).iter().all(|f| f["field"] != NAME),
        "{reply}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_requires_line_keeps_its_writer_and_necessity_and_names_a_loaded_type() {
    let store = Store::with_both().await;
    let export = store.export().await;
    let line = node(&export, "form_item", "Copy of your identity document");
    assert_eq!(line["properties"]["relation"], "requires", "{line}");
    assert_eq!(line["properties"]["document_type"], IDENTITY_TYPE, "{line}");
    assert_eq!(line["properties"]["filled_by"], "applicant", "{line}");
    assert_eq!(line["properties"]["necessity"], "required", "{line}");
    for label in ["Name", "Born"] {
        let stated = node(&export, "form_item", label);
        assert_eq!(stated["properties"]["relation"], "states", "{stated}");
        assert!(stated["properties"].get("filled_by").is_none(), "{stated}");
        assert!(stated["properties"].get("necessity").is_none(), "{stated}");
    }

    // No answer attaches a document yet, so check reports nothing for the
    // requires line; only the legal name is missing.
    let reply = store
        .call(
            "forms.check",
            json!({"forms": [APPLICATION], "answers": []}),
        )
        .await;
    let missing = missing(&reply);
    assert_eq!(missing.len(), 1, "{reply}");
    assert_eq!(missing[0]["field"], NAME, "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_requires_line_naming_a_type_not_loaded_is_refused() {
    let store = Store::new();
    let message = store.refused_load(application_alone()).await;
    assert!(
        message.contains("which is not loaded; load a document of that type first"),
        "{message}"
    );
    let export = store.export().await;
    assert_eq!(
        export["nodes"],
        json!([]),
        "a refused load writes nothing: {export}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_requires_line_naming_its_own_forms_type_is_refused() {
    let store = Store::new();
    let mut document = identity_form();
    document["subjects"] = json!([
        {"id": "holder", "kind": "party", "role": "holder"},
        {"id": "applicant", "kind": "party", "role": "applicant"}
    ]);
    document["items"]
        .as_array_mut()
        .expect("items")
        .push(requiring("Another identity document", IDENTITY_TYPE));
    let message = store.refused_load(document).await;
    assert!(
        message.contains("a document is not attached to itself"),
        "{message}"
    );

    // A form that declares no type is its own type by its form id.
    let mut document = application_alone();
    document
        .as_object_mut()
        .expect("document")
        .remove("document_type");
    document["items"][1]["document_type"] = json!(APPLICATION);
    let message = store.refused_load(document).await;
    assert!(
        message.contains("a document is not attached to itself"),
        "{message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stating_line_that_says_who_fills_it_or_how_necessary_it_is_is_refused() {
    let store = Store::new();
    for (key, value, why) in [
        ("filled_by", json!("holder"), "no subject fills it in"),
        ("necessity", json!("required"), "it has no necessity"),
        (
            "document_type",
            json!(IDENTITY_TYPE),
            "it takes no attached document",
        ),
    ] {
        let mut document = identity_form();
        document["items"][0][key] = value;
        let message = store.refused_load(document).await;
        assert!(
            message.contains(&format!(
                "item \"Name\" states its field, so {why}; drop {key}"
            )),
            "{key}: {message}"
        );
    }

    let mut document = application_alone();
    document["items"][0]["document_type"] = json!(IDENTITY_TYPE);
    let message = store.refused_load(document).await;
    assert!(
        message.contains("which only a requires line takes"),
        "{message}"
    );

    let mut document = application_alone();
    document["items"][1]["item_kind"] = json!("text");
    let message = store.refused_load(document).await;
    assert!(message.contains("its item_kind is attachment"), "{message}");

    let mut document = identity_form();
    document["document_type"]["origin"] = json!("nobody");
    let message = store.refused_load(document).await;
    assert!(message.contains("origin"), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_form_that_names_no_type_gets_one_minted_and_a_later_declaration_replaces_it() {
    let store = Store::new();
    let mut first = identity_form();
    first
        .as_object_mut()
        .expect("document")
        .remove("document_type");
    let reply = store.load(first).await;
    assert_eq!(reply["document_type"]["minted"], true, "{reply}");
    assert_eq!(
        reply["document_type"]["key"],
        format!("document_type:{IDENTITY}"),
        "{reply}"
    );

    let lint = store.call("forms.lint", json!({})).await;
    let minted = lint["minted_document_types"].as_array().expect("minted");
    assert_eq!(minted.len(), 1, "{lint}");
    assert_eq!(minted[0]["document_type_id"], IDENTITY, "{lint}");
    assert_eq!(
        lint["clean"], true,
        "a guessed type is listed, not broken: {lint}"
    );

    // The next revision declares the type under the minted id.
    let mut second = identity_form();
    second["revision"] = json!("2026-02");
    second["source"] = source(3);
    second["document_type"]["id"] = json!(IDENTITY);
    let reply = store.load(second).await;
    assert_eq!(reply["document_type"]["minted"], false, "{reply}");
    assert_eq!(
        reply["counts"]["document_types"], 0,
        "the minted type is replaced, not doubled: {reply}"
    );

    let export = store.export().await;
    let types: Vec<&Value> = export["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter(|n| n["type"] == "document_type")
        .collect();
    assert_eq!(types.len(), 1, "{export}");
    assert_eq!(types[0]["name"], "Identity document");
    assert_eq!(types[0]["properties"]["origin"], "third_party");
    assert!(types[0]["properties"].get("minted").is_none(), "{export}");
    for revision in ["2026-01", "2026-02"] {
        assert!(
            contains(
                &export,
                &format!("document_type:{IDENTITY}"),
                &format!("form:{IDENTITY}@{revision}")
            ),
            "{revision}: {export}"
        );
    }
    let lint = store.call("forms.lint", json!({})).await;
    assert_eq!(lint["minted_document_types"], json!([]), "{lint}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_declaration_that_differs_from_the_loaded_type_is_refused() {
    let store = Store::new();
    store.load(identity_form()).await;
    let before = store.export().await;
    let mut other = application_form();
    other["items"] = json!([other["items"][0].clone()]);
    other["document_type"] = json!({"id": IDENTITY_TYPE, "name": "Travel document", "origin": "third_party", "issuer_class": "government"});
    let message = store.refused_load(other.clone()).await;
    assert!(
        message.contains(&format!(
            "document type \"{IDENTITY_TYPE}\" is loaded as \"Identity document\""
        )) && message.contains("declares \"Travel document\""),
        "{message}"
    );
    assert_eq!(
        store.export().await,
        before,
        "a refused load writes nothing"
    );

    // The same declaration as the loaded one is accepted.
    other["document_type"]["name"] = json!("Identity document");
    let reply = store.load(other).await;
    assert_eq!(reply["counts"]["document_types"], 0, "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn export_and_replay_keep_types_their_edges_and_the_minted_mark() {
    let store = Store::with_both().await;
    let mut guessed = application_form();
    guessed["form_id"] = json!("GUESS-1");
    guessed["source"] = source(4);
    guessed
        .as_object_mut()
        .expect("document")
        .remove("document_type");
    store.load(guessed).await;
    let exported = store.export().await;
    assert_eq!(exported["warnings"], json!([]), "{exported}");

    let empty = Store::new();
    let replayed = empty
        .call("forms.replay", json!({"document": exported.clone()}))
        .await;
    assert_eq!(replayed["replayed"], true, "{replayed}");
    let again = empty.export().await;
    assert_eq!(again["nodes"], exported["nodes"], "identical nodes");
    assert_eq!(again["edges"], exported["edges"], "identical edges");
    let lint = empty.call("forms.lint", json!({})).await;
    let minted = lint["minted_document_types"].as_array().expect("minted");
    assert_eq!(minted.len(), 1, "the minted mark survives replay: {lint}");
    assert_eq!(minted[0]["document_type_id"], "GUESS-1", "{lint}");

    // A store written before document types existed: the same export with
    // the types taken out replays, and lint lists its forms.
    let mut older = exported;
    older["nodes"]
        .as_array_mut()
        .expect("nodes")
        .retain(|n| n["type"] != "document_type");
    older["edges"].as_array_mut().expect("edges").retain(|e| {
        !e["source"]
            .as_str()
            .is_some_and(|s| s.starts_with("document_type:"))
    });
    // The requires line names a type the older store never had.
    let fresh = Store::new();
    let replayed = fresh.call("forms.replay", json!({"document": older})).await;
    assert_eq!(replayed["replayed"], true, "{replayed}");
    let lint = fresh.call("forms.lint", json!({})).await;
    assert_eq!(
        lint["forms_without_document_type"]
            .as_array()
            .expect("forms without a type")
            .len(),
        3,
        "{lint}"
    );
    assert_eq!(lint["minted_document_types"], json!([]), "{lint}");
}

const PORTAL: &str = "PORTAL-1";
const PORTAL_TYPE: &str = "permit-portal";

/// One record of an outside system, declared the way a document type is.
fn portal_record(form_id: &str, n: u8) -> Value {
    json!({
        "form_id": form_id,
        "revision": "2026-01",
        "title": "Permit portal record",
        "source": source(n),
        "document_type": {"id": PORTAL_TYPE, "name": "Permit portal", "system": true},
        "subjects": [{"id": "holder", "kind": "party", "role": "holder"}],
        "terms": {"kinds": ["party"], "roles": ["holder", "applicant"]},
        "fields": fields(),
        "items": [stating("Name", 1, NAME)]
    })
}

fn asking(label: &str, order: u32) -> Value {
    json!({"label": label, "page": 1, "order": order, "item_kind": "text", "filled_by": "holder", "about": "holder", "field": NAME})
}

#[tokio::test(flavor = "multi_thread")]
async fn a_system_is_declared_as_a_document_type_whose_lines_only_state() {
    let store = Store::new();
    store.load(identity_form()).await;
    let reply = store.load(portal_record(PORTAL, 3)).await;
    assert_eq!(reply["document_type"]["minted"], false, "{reply}");
    let export = store.export().await;
    let portal = node(&export, "document_type", "Permit portal");
    assert_eq!(portal["properties"]["system"], true, "{portal}");
    assert!(portal["properties"].get("origin").is_none(), "{portal}");
    assert!(
        contains(&export, portal["key"].as_str().expect("key"), "form:"),
        "{export}"
    );
    // A second record of the same system joins the type it declared.
    let second = store.load(portal_record("PORTAL-2", 4)).await;
    assert_eq!(second["document_type"]["minted"], false, "{second}");
    // A system has no origin, and lint does not count that as missing.
    let lint = store.call("forms.lint", json!({})).await;
    assert_eq!(lint["missing_required"], json!([]), "{lint}");
    assert_eq!(lint["clean"], true, "{lint}");

    for (n, line) in [
        (5, asking("Your legal name", 1)),
        (
            6,
            requiring("Copy of your identity document", IDENTITY_TYPE),
        ),
    ] {
        let mut record = portal_record("PORTAL-3", n);
        record["items"] = json!([line]);
        let message = store.refused_load(record).await;
        assert!(
            message.contains("is a system, whose lines only state"),
            "{message}"
        );
    }

    let message = store
        .refused(
            "forms.add_item",
            json!({"form": PORTAL, "item": asking("Legal name again", 9), "same_as": NAME}),
        )
        .await;
    assert!(
        message.contains("is a system, whose lines only state"),
        "{message}"
    );
    // A form of a type that is not a system still takes the same line.
    let added = store
        .call(
            "forms.add_item",
            json!({"form": IDENTITY, "item": asking("Legal name again", 9), "same_as": NAME}),
        )
        .await;
    assert_eq!(added["status"], "same", "{added}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_system_declaration_is_refused_with_an_origin_without_one_or_against_what_is_loaded() {
    let store = Store::new();
    for (declared, wanted) in [
        (
            json!({"id": PORTAL_TYPE, "name": "Permit portal", "system": true, "origin": "work"}),
            "is a system, so it has no origin",
        ),
        (
            json!({"id": PORTAL_TYPE, "name": "Permit portal"}),
            "needs an origin",
        ),
        (
            json!({"id": PORTAL_TYPE, "name": "Permit portal", "system": "yes"}),
            "must be true or false",
        ),
    ] {
        let mut record = portal_record(PORTAL, 3);
        record["document_type"] = declared;
        let message = store.refused_load(record).await;
        assert!(message.contains(wanted), "{wanted}: {message}");
    }

    store.load(portal_record(PORTAL, 3)).await;
    let mut not_a_system = portal_record("PORTAL-2", 4);
    not_a_system["document_type"] =
        json!({"id": PORTAL_TYPE, "name": "Permit portal", "origin": "work"});
    let message = store.refused_load(not_a_system).await;
    assert!(
        message.contains("(system, issuer class none)")
            && message.contains("(origin work, issuer class none)"),
        "{message}"
    );

    // A type minted for a form to fill in never becomes a system.
    store.load(application_untyped()).await;
    let mut over_minted = portal_record("PORTAL-4", 7);
    over_minted["document_type"] =
        json!({"id": APPLICATION, "name": "Application", "system": true});
    let message = store.refused_load(over_minted).await;
    assert!(message.contains("cannot be declared a system"), "{message}");
}

/// The application with no document type, so that one is minted for it, and
/// no requires line, since its target is not loaded here.
fn application_untyped() -> Value {
    let mut document = application_alone();
    document
        .as_object_mut()
        .expect("document")
        .remove("document_type");
    document["items"] = json!([{"label": "Your legal name", "page": 1, "order": 1, "item_kind": "text", "filled_by": "applicant", "about": "applicant", "field": NAME}]);
    document
}

#[tokio::test(flavor = "multi_thread")]
async fn export_and_replay_keep_the_system_mark() {
    let store = Store::new();
    store.load(portal_record(PORTAL, 3)).await;
    let exported = store.export().await;
    let empty = Store::new();
    let replayed = empty
        .call("forms.replay", json!({"document": exported.clone()}))
        .await;
    assert_eq!(replayed["replayed"], true, "{replayed}");
    assert_eq!(empty.export().await, exported);
    let message = empty
        .refused(
            "forms.add_item",
            json!({"form": PORTAL, "item": asking("Legal name again", 9), "same_as": NAME}),
        )
        .await;
    assert!(message.contains("is a system"), "{message}");
}

fn known(vocabulary: &str, name: &str) -> Value {
    json!({"vocabulary": vocabulary, "name": name})
}

/// A form that asks for one field, given names in vocabularies.
fn naming_form(form_id: &str, n: u8, field: &str, known_as: Value) -> Value {
    json!({
        "form_id": form_id,
        "revision": "2026-01",
        "title": "Naming form",
        "source": source(n),
        "document_type": {"id": "naming", "name": "Naming", "origin": "applicant"},
        "subjects": [{"id": "applicant", "kind": "party", "role": "applicant"}],
        "terms": {"kinds": ["party"], "roles": ["applicant"]},
        "fields": [{"name": field, "definition": format!("The {field} the applicant gives."), "value_type": "text", "known_as": known_as}],
        "items": [{"label": field, "page": 1, "order": 1, "item_kind": "text", "filled_by": "applicant", "about": "applicant", "field": field}]
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_field_name_in_a_vocabulary_names_one_field() {
    let store = Store::new();
    store
        .load(naming_form(
            "NAME-A",
            11,
            "full name",
            json!([known("permits", "applicant name")]),
        ))
        .await;
    let field = store
        .call("forms.neighbourhood", json!({"node": "full name"}))
        .await;
    assert_eq!(
        field["field"]["known_as"],
        json!([known("permits", "applicant name")]),
        "{field}"
    );

    // Another field taking the name in the same vocabulary is refused and writes nothing.
    let message = store
        .refused_load(naming_form(
            "NAME-B",
            12,
            "given name",
            json!([known("Permits", "Applicant Name")]),
        ))
        .await;
    assert!(
        message.contains("already names field \"full name\""),
        "{message}"
    );
    let missing = store
        .refused("forms.neighbourhood", json!({"node": "given name"}))
        .await;
    assert!(missing.contains("given name"), "{missing}");

    // The same name in another vocabulary is accepted.
    store
        .load(naming_form(
            "NAME-C",
            13,
            "given name",
            json!([known("licensing", "applicant name")]),
        ))
        .await;

    // A reused field keeps its names and takes a further one.
    store
        .load(naming_form(
            "NAME-F",
            16,
            "full name",
            json!([known("tax", "legal name")]),
        ))
        .await;
    let field = store
        .call("forms.neighbourhood", json!({"node": "full name"}))
        .await;
    assert_eq!(
        field["field"]["known_as"],
        json!([
            known("permits", "applicant name"),
            known("tax", "legal name")
        ]),
        "{field}"
    );

    // Two fields of one document given one name are refused.
    let mut two = naming_form(
        "NAME-D",
        14,
        "family name",
        json!([known("census", "surname")]),
    );
    two["fields"]
        .as_array_mut()
        .expect("fields")
        .push(json!({"name": "last name", "definition": "The last name the applicant gives.", "value_type": "text", "known_as": [known("census", "surname")]}));
    let message = store.refused_load(two).await;
    assert!(
        message.contains("to fields \"family name\" and \"last name\""),
        "{message}"
    );

    // A vocabulary named after an operation node type is refused.
    let message = store
        .refused_load(naming_form(
            "NAME-E",
            15,
            "middle name",
            json!([known("step_type", "middle")]),
        ))
        .await;
    assert!(message.contains("reserved_vocabulary"), "{message}");
    // A '/' in a vocabulary is refused: vocabulary "x/field/y" with name "z" would
    // otherwise share a key with vocabulary "x" and name "y/field/z".
    let message = store
        .refused_load(naming_form(
            "NAME-E",
            16,
            "middle name",
            json!([known("x/field/y", "z")]),
        ))
        .await;
    assert!(message.contains("which contains '/'"), "{message}");

    // Replay writes the names as keys again: the replayed store refuses the same clash,
    // here on a field it reuses.
    let exported = store.export().await;
    let fresh = Store::new();
    let replayed = fresh
        .call("forms.replay", json!({"document": exported}))
        .await;
    assert_eq!(replayed["replayed"], true, "{replayed}");
    let message = fresh
        .refused_load(naming_form(
            "NAME-B",
            12,
            "given name",
            json!([known("permits", "applicant name")]),
        ))
        .await;
    assert!(
        message.contains("already names field \"full name\""),
        "{message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_field_lists_the_lines_that_state_it_and_a_document_type_is_a_node() {
    let store = Store::with_both().await;
    store.load(portal_record(PORTAL, 3)).await;

    // The application asks for the legal name; only the two stating lines are listed.
    let name = store
        .call("forms.neighbourhood", json!({"node": NAME}))
        .await;
    assert_eq!(
        name["stated_by"],
        json!([
            {"document_type": IDENTITY_TYPE, "system": false, "form": IDENTITY, "revision": "2026-01", "subject": "holder", "role": "holder"},
            {"document_type": PORTAL_TYPE, "system": true, "form": PORTAL, "revision": "2026-01", "subject": "holder", "role": "holder"}
        ]),
        "{name}"
    );
    let birth = store
        .call("forms.neighbourhood", json!({"node": BIRTH}))
        .await;
    assert_eq!(
        birth["stated_by"].as_array().map(Vec::len),
        Some(1),
        "{birth}"
    );
    assert_eq!(birth["stated_by"][0]["form"], IDENTITY, "{birth}");

    let portal = store
        .call(
            "forms.neighbourhood",
            json!({"node": format!("document_type:{PORTAL_TYPE}")}),
        )
        .await;
    assert_eq!(portal["node"]["type"], "document_type", "{portal}");
    assert_eq!(portal["node"]["system"], true, "{portal}");
    assert_eq!(portal["node"]["forms"][0]["form_id"], PORTAL, "{portal}");
    assert_eq!(
        portal["node"]["forms"].as_array().map(Vec::len),
        Some(1),
        "{portal}"
    );

    let identity = store
        .call(
            "forms.neighbourhood",
            json!({"node": format!("document_type:{IDENTITY_TYPE}")}),
        )
        .await;
    assert_eq!(identity["node"]["system"], false, "{identity}");
    assert_eq!(identity["node"]["origin"], "third_party", "{identity}");
    assert_eq!(
        identity["node"]["forms"][0]["form_id"], IDENTITY,
        "{identity}"
    );

    let message = store
        .refused(
            "forms.neighbourhood",
            json!({"node": "document_type:no-such-type"}),
        )
        .await;
    assert!(
        message.contains("no document type \"no-such-type\" is loaded"),
        "{message}"
    );
}
