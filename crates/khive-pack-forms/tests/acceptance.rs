//! The forms pack's acceptance cases, run against an in-memory store with the
//! kg and forms packs installed.
//!
//! Two invented forms: A, an outgoing international wire, and B, a transfer
//! of securities. A third form, C, is filed instead of A, never with it.

use khive_pack_forms::FormsPack;
use khive_runtime::{KhiveRuntime, PackRegistry, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;
use serde_json::{json, Value};

use khive_pack_kg as _;

const ACTOR: &str = "forms-reader";
const A: &str = "WIRE-1";
const A_KEY: &str = "form:WIRE-1@2026-01";
const B: &str = "TOA-1";
const B_KEY: &str = "form:TOA-1@2026-03";
const C: &str = "WIRE-2";
const RULE: &str = "If some assets, complete Section 3";

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
            actor_id: Some(ACTOR.to_owned()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("in-memory runtime without embedding services");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(ACTOR.to_owned()));
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
}

fn item(label: &str, page: u32, order: u32, item_kind: &str, field: &str) -> Value {
    json!({
        "label": label,
        "page": page,
        "order": order,
        "item_kind": item_kind,
        "filled_by": "customer",
        "field": field,
    })
}

fn form_a() -> Value {
    json!({
        "form_id": A,
        "revision": "2026-01",
        "title": "Outgoing international wire",
        "source": {"url": "https://forms.example/wire-1.pdf", "sha256": "1".repeat(64)},
        "issuer": {"name": "Example Bank", "jurisdiction": "US"},
        "fields": [
            {"name": "sender name", "definition": "Full legal name of the person sending the money.", "value_type": "text"},
            {"name": "sender account number", "definition": "Number of the account the money is taken from.", "value_type": "identifier"},
            {"name": "recipient name", "definition": "Full legal name of the person receiving the money.", "value_type": "text"},
            {"name": "recipient account number", "definition": "Number of the recipient's account at the receiving bank.", "value_type": "identifier"},
            {"name": "recipient bank identifier", "definition": "Code that identifies the receiving bank.", "value_type": "identifier"},
            {"name": "amount", "definition": "Sum of money to send.", "value_type": "amount"},
            {
                "name": "fee paid by",
                "definition": "Who pays the transfer fees.",
                "value_type": "choice",
                "exclusive": true,
                "values": [
                    {"name": "sender", "definition": "The sender pays every fee."},
                    {"name": "recipient", "definition": "The recipient pays every fee."},
                    {"name": "shared", "definition": "Each side pays its own bank's fees."}
                ]
            }
        ],
        "items": [
            item("Sender name", 1, 1, "text", "sender name"),
            item("Sender account number", 1, 2, "text", "sender account number"),
            item("Recipient name", 1, 3, "text", "recipient name"),
            item("Recipient account number", 1, 4, "text", "recipient account number"),
            item("Recipient bank identifier", 1, 5, "text", "recipient bank identifier"),
            item("Amount", 1, 6, "text", "amount"),
            item("Fee paid by", 1, 7, "choice", "fee paid by")
        ]
    })
}

fn form_b() -> Value {
    json!({
        "form_id": B,
        "revision": "2026-03",
        "title": "Transfer of securities",
        "source": {"url": "https://forms.example/toa-1.pdf", "sha256": "2".repeat(64)},
        "fields": [
            {"name": "delivering account number", "definition": "Number of the account the securities leave.", "value_type": "identifier"},
            {
                "name": "transfer",
                "definition": "How much of the account is transferred.",
                "value_type": "choice",
                "exclusive": true,
                "values": [
                    {"name": "all assets", "definition": "Every position in the account."},
                    {"name": "some assets", "definition": "Only the positions listed in Section 3."}
                ]
            },
            {"name": "assets to transfer", "definition": "The positions to move, one per line.", "value_type": "text"}
        ],
        "sections": [
            {"label": "Section 1", "order": 1},
            {"label": "Section 3", "order": 3}
        ],
        "items": [
            {"label": "Delivering account number", "page": 1, "order": 1, "item_kind": "text", "filled_by": "customer", "section": "Section 1", "field": "delivering account number"},
            {"label": "Transfer", "page": 1, "order": 2, "item_kind": "choice", "filled_by": "customer", "section": "Section 1", "field": "transfer"},
            {"label": "Assets to transfer", "page": 2, "order": 10, "item_kind": "text", "filled_by": "customer", "section": "Section 3", "field": "assets to transfer"}
        ],
        "rules": [
            {
                "text": RULE,
                "page": 2,
                "depends_on": [{"field": "transfer", "value": "some assets"}],
                "enables": [{"section": "Section 3"}]
            }
        ]
    })
}

fn form_c() -> Value {
    json!({
        "form_id": C,
        "revision": "2026-02",
        "title": "Domestic wire",
        "source": {"url": "https://forms.example/wire-2.pdf", "sha256": "3".repeat(64)},
        "fields": [
            {"name": "amount", "definition": "Sum of money to send.", "value_type": "amount"}
        ],
        "items": [item("Amount", 1, 1, "text", "amount")],
        "relations": [{"relation": "competes_with", "form": A}]
    })
}

fn field_hits(reply: &Value) -> Vec<&Value> {
    reply["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .filter(|hit| hit["type"] == "field")
        .collect()
}

fn texts(list: &Value, key: &str) -> Vec<String> {
    list.as_array()
        .expect("a list")
        .iter()
        .map(|entry| entry[key].as_str().expect("a string").to_owned())
        .collect()
}

/// The store after each case, built up in order.
struct Scenario {
    store: Store,
    loaded_a: Value,
    loaded_b: Value,
    added_b: Option<Value>,
}

impl Scenario {
    /// Case 1, then B loaded (the setup case 2 needs).
    async fn loaded() -> Self {
        let store = Store::new();
        let loaded_a = store
            .call("forms.load", json!({"document": form_a()}))
            .await;
        let loaded_b = store
            .call("forms.load", json!({"document": form_b()}))
            .await;
        Self {
            store,
            loaded_a,
            loaded_b,
            added_b: None,
        }
    }

    /// Case 2: B's recipient account number.
    async fn add_recipient_account(&mut self) -> Value {
        let reply = self
            .store
            .call(
                "forms.add_item",
                json!({
                    "form": B,
                    "item": {"label": "Recipient account number", "page": 1, "order": 3, "item_kind": "text", "filled_by": "customer", "section": "Section 1"},
                    "field": {"name": "recipient account number", "definition": "Number of the account at the receiving firm.", "value_type": "identifier"}
                }),
            )
            .await;
        self.added_b = Some(reply.clone());
        reply
    }

    /// Case 3: the answer is "different".
    async fn split(&mut self) -> Value {
        let added = self.add_recipient_account().await;
        self.store
            .call(
                "forms.split",
                json!({
                    "field": "recipient account number",
                    "item": added["item"]["id"],
                    "existing_name": "recipient bank account number",
                    "new_name": "recipient brokerage account number",
                    "new_definition": "Number of the recipient's account at the receiving brokerage firm."
                }),
            )
            .await
    }

    /// Case 4: B's account holder name is A's sender name.
    async fn same_holder(&self) -> Value {
        self.store
            .call(
                "forms.add_item",
                json!({
                    "form": B,
                    "item": {"label": "Account holder name", "page": 1, "order": 4, "item_kind": "text", "filled_by": "customer", "section": "Section 1"},
                    "same_as": "sender name"
                }),
            )
            .await
    }

    /// Cases 1 to 4 done, C loaded and competing with A.
    async fn full() -> Self {
        let mut scenario = Self::loaded().await;
        scenario.split().await;
        scenario.same_holder().await;
        scenario
            .store
            .call("forms.load", json!({"document": form_c()}))
            .await;
        scenario
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn case_1_loading_a_into_an_empty_namespace() {
    let store = Store::new();
    let reply = store
        .call("forms.load", json!({"document": form_a()}))
        .await;
    assert_eq!(reply["loaded"], true, "{reply}");
    assert_eq!(reply["form"]["key"], A_KEY);
    let counts = &reply["counts"];
    assert_eq!(counts["forms"], 1, "{reply}");
    assert_eq!(counts["items"], 7, "{reply}");
    assert_eq!(counts["fields"], 7, "{reply}");
    assert_eq!(counts["values"], 3, "{reply}");
    assert_eq!(reply["warnings"], json!([]), "{reply}");

    let lint = store.call("forms.lint", json!({})).await;
    assert_eq!(lint["unbound_items"], json!([]), "{lint}");
    assert_eq!(lint["missing_required"], json!([]), "{lint}");
    assert_eq!(lint["missing_source"], json!([]), "{lint}");

    let again = store
        .call("forms.load", json!({"document": form_a()}))
        .await;
    assert_eq!(
        again["loaded"], false,
        "loading the same revision twice changes nothing: {again}"
    );
    assert_eq!(again["form"]["id"], reply["form"]["id"]);
    let export = store.call("forms.export", json!({})).await;
    assert_eq!(
        export["nodes"].as_array().expect("nodes").len(),
        1 + 1 + 7 + 7 + 3,
        "one issuer, one form, seven items, seven fields, three values: {export}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn case_2_adding_bs_recipient_account_number_returns_as_field() {
    let mut scenario = Scenario::loaded().await;
    assert_eq!(scenario.loaded_b["loaded"], true, "{}", scenario.loaded_b);
    let reply = scenario.add_recipient_account().await;
    assert_eq!(reply["status"], "match", "{reply}");
    let candidates = reply["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 1, "{reply}");
    assert_eq!(candidates[0]["name"], "recipient account number");
    assert_eq!(
        candidates[0]["definition"],
        "Number of the recipient's account at the receiving bank."
    );
    let a_item = scenario.loaded_a["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["label"] == "Recipient account number")
        .expect("A's item");
    assert_eq!(a_item["field"], "recipient account number");
}

#[tokio::test(flavor = "multi_thread")]
async fn case_3_different_splits_the_field_under_a_general_one() {
    let mut scenario = Scenario::loaded().await;
    let split = scenario.split().await;
    assert_eq!(split["status"], "split", "{split}");
    let store = &scenario.store;

    let general = store
        .call(
            "forms.neighbourhood",
            json!({"node": split["general"]["id"]}),
        )
        .await;
    assert_eq!(general["field"]["general"], true, "{general}");
    let mut narrower = general["narrower"]
        .as_array()
        .expect("narrower")
        .iter()
        .map(|n| n.as_str().expect("name").to_owned())
        .collect::<Vec<_>>();
    narrower.sort();
    assert_eq!(
        narrower,
        [
            "recipient bank account number",
            "recipient brokerage account number"
        ]
    );

    let resolved = store
        .call("forms.resolve", json!({"text": "recipient account number"}))
        .await;
    assert_eq!(resolved["ambiguous"], true, "{resolved}");

    for (name, form) in [
        ("recipient bank account number", A_KEY),
        ("recipient brokerage account number", B_KEY),
    ] {
        let field = store
            .call("forms.neighbourhood", json!({"node": name}))
            .await;
        let asked_by = field["asked_by"].as_array().expect("asked_by");
        assert_eq!(asked_by.len(), 1, "{name}: {field}");
        assert_eq!(asked_by[0]["form"], form, "{name}: {field}");
        assert_eq!(
            asked_by[0]["item"], "Recipient account number",
            "the item keeps the form's words: {field}"
        );
        assert_eq!(
            field["general"],
            json!(["recipient account number"]),
            "{field}"
        );
    }

    let a_item = scenario.loaded_a["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["label"] == "Recipient account number")
        .expect("A's item")["id"]
        .clone();
    let a_item = store
        .call("forms.neighbourhood", json!({"node": a_item}))
        .await;
    assert_eq!(
        a_item["node"]["name"], "Recipient account number",
        "{a_item}"
    );

    let renamed = store
        .call(
            "forms.neighbourhood",
            json!({"node": "recipient bank account number"}),
        )
        .await;
    let aliases = renamed["field"]["aliases"].as_array().expect("aliases");
    assert!(
        !aliases.contains(&json!("recipient account number")),
        "the shared wording moves to the general field: {renamed}"
    );
}

/// A field that already holds the split's existing name as an entity alias is
/// still split: the rename carries the field's alias set without that name, so
/// the alias never equals the name and the other aliases stay.
#[tokio::test(flavor = "multi_thread")]
async fn a_split_renames_a_field_holding_the_existing_name_as_an_alias() {
    let mut scenario = Scenario::loaded().await;
    let id = {
        let store = &scenario.store;
        let field = store
            .call(
                "forms.neighbourhood",
                json!({"node": "recipient account number"}),
            )
            .await;
        let id = field["node"]["id"].clone();
        assert!(id.is_string(), "{field}");
        store
            .call(
                "update",
                json!({"id": id, "add_aliases": ["recipient bank account number", "payee account"]}),
            )
            .await;
        id
    };
    let split = scenario.split().await;
    assert_eq!(split["status"], "split", "{split}");
    let got = scenario.store.call("get", json!({"id": id})).await;
    assert_eq!(got["name"], "recipient bank account number", "{got}");
    assert_eq!(
        got["aliases"],
        json!([{"alias": "payee account", "origin": "caller"}]),
        "{got}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn case_4_same_gives_one_field_two_aliases_two_items() {
    let scenario = Scenario::loaded().await;
    let reply = scenario.same_holder().await;
    assert_eq!(reply["status"], "same", "{reply}");
    assert_eq!(reply["alias_added"], "Account holder name", "{reply}");

    let field = scenario
        .store
        .call("forms.neighbourhood", json!({"node": "sender name"}))
        .await;
    assert_eq!(
        field["field"]["aliases"],
        json!(["sender name", "Account holder name"]),
        "{field}"
    );
    let mut forms = texts(&field["asked_by"], "form");
    forms.sort();
    assert_eq!(forms, [B_KEY, A_KEY], "{field}");
    let fields = scenario
        .store
        .call("forms.resolve", json!({"text": "account holder name"}))
        .await;
    let hits = field_hits(&fields);
    assert_eq!(hits.len(), 1, "{fields}");
    assert_eq!(hits[0]["name"], "sender name");
}

#[tokio::test(flavor = "multi_thread")]
async fn case_5_resolve_returns_both_fields_flagged_and_the_choice_with_its_values() {
    let scenario = Scenario::full().await;
    let store = &scenario.store;

    let reply = store
        .call("forms.resolve", json!({"text": "recipient account number"}))
        .await;
    assert_eq!(reply["ambiguous"], true, "{reply}");
    let hits = field_hits(&reply);
    assert_eq!(hits.len(), 2, "{reply}");
    for (hit, (name, form)) in hits.iter().zip([
        ("recipient bank account number", A_KEY),
        ("recipient brokerage account number", B_KEY),
    ]) {
        assert_eq!(hit["name"], name, "{reply}");
        assert_eq!(hit["ambiguous"], true, "{reply}");
        assert_eq!(hit["via"], "recipient account number", "{reply}");
        let forms = texts(&hit["forms"], "form");
        assert_eq!(
            forms,
            [form],
            "each field names the form that prints the wording: {reply}"
        );
        assert_eq!(hit["forms"][0]["item"], "Recipient account number");
    }

    let again = store
        .call("forms.resolve", json!({"text": "recipient account number"}))
        .await;
    assert_eq!(again, reply, "same graph, same answer");

    let fee = store
        .call("forms.resolve", json!({"text": "fee paid by"}))
        .await;
    assert_eq!(fee["ambiguous"], false, "{fee}");
    let hits = field_hits(&fee);
    assert_eq!(hits.len(), 1, "{fee}");
    assert_eq!(hits[0]["name"], "fee paid by");
    assert_eq!(hits[0]["value_type"], "choice");
    assert_eq!(
        texts(&hits[0]["values"], "name"),
        ["sender", "recipient", "shared"],
        "{fee}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn case_6_a_printed_rule_whose_premise_holds_and_whose_section_is_empty() {
    let scenario = Scenario::full().await;
    let store = &scenario.store;

    let reply = store
        .call(
            "forms.check",
            json!({"forms": [B], "answers": {"transfer": "some assets"}}),
        )
        .await;
    let findings = reply["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 1, "{reply}");
    assert_eq!(findings[0]["kind"], "rule_unmet", "{reply}");
    assert_eq!(findings[0]["rule"], RULE, "{reply}");
    assert_eq!(findings[0]["page"], 2, "{reply}");
    assert_eq!(findings[0]["form"]["form_id"], B, "{reply}");
    assert_eq!(findings[0]["unmet"][0]["name"], "Section 3", "{reply}");
    assert_eq!(reply["unresolved_answers"], json!([]), "{reply}");

    let filled = store
        .call(
            "forms.check",
            json!({"forms": [B], "answers": {"transfer": "some assets", "assets to transfer": "100 shares"}}),
        )
        .await;
    assert_eq!(filled["findings"], json!([]), "Section 3 filled: {filled}");
    let all = store
        .call(
            "forms.check",
            json!({"forms": [B], "answers": {"transfer": "all assets"}}),
        )
        .await;
    assert_eq!(all["findings"], json!([]), "premise does not hold: {all}");
    let outside = store
        .call(
            "forms.check",
            json!({"forms": [B], "answers": {"transfer": "half"}}),
        )
        .await;
    assert_eq!(
        outside["findings"][0]["kind"], "value_outside_domain",
        "{outside}"
    );
    let both = store
        .call(
            "forms.check",
            json!({"forms": [A], "answers": {"fee paid by": ["sender", "shared"]}}),
        )
        .await;
    assert_eq!(both["findings"][0]["kind"], "exclusive_values", "{both}");
}

#[tokio::test(flavor = "multi_thread")]
async fn case_7_two_competing_forms_in_one_set() {
    let scenario = Scenario::full().await;
    let store = &scenario.store;

    let reply = store.call("forms.check", json!({"forms": [A, C]})).await;
    let findings = reply["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 1, "{reply}");
    assert_eq!(findings[0]["kind"], "competing_forms", "{reply}");
    let mut ids = texts(&findings[0]["forms"], "form_id");
    ids.sort();
    assert_eq!(ids, [A, C], "{reply}");

    let alone = store.call("forms.check", json!({"forms": [A]})).await;
    assert_eq!(alone["findings"], json!([]), "{alone}");
}

#[tokio::test(flavor = "multi_thread")]
async fn case_8_export_then_replay_into_an_empty_namespace_is_identical() {
    let scenario = Scenario::full().await;
    let exported = scenario.store.call("forms.export", json!({})).await;
    assert_eq!(exported["warnings"], json!([]), "{exported}");
    assert_eq!(
        exported["log"].as_array().expect("log").len(),
        2,
        "the split and the same decision: {exported}"
    );

    let empty = Store::new();
    let replayed = empty
        .call("forms.replay", json!({"document": exported.clone()}))
        .await;
    assert_eq!(replayed["replayed"], true, "{replayed}");
    let again = empty.call("forms.export", json!({})).await;
    assert_eq!(
        again["nodes"], exported["nodes"],
        "identical nodes and aliases"
    );
    assert_eq!(again["edges"], exported["edges"], "identical edges");
    assert_eq!(again["log"], exported["log"], "identical decision log");

    let resolved = empty
        .call("forms.resolve", json!({"text": "recipient account number"}))
        .await;
    assert_eq!(resolved["ambiguous"], true, "{resolved}");

    let refused = scenario
        .store
        .refused("forms.replay", json!({"document": exported}))
        .await;
    assert!(refused.contains("already present"), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn case_9_depends_on_from_an_item_to_a_form_is_refused_with_the_allowed_pairs() {
    let scenario = Scenario::loaded().await;
    let store = &scenario.store;
    let item = scenario.loaded_b["items"][0]["id"].clone();
    let form = scenario.loaded_a["form"]["id"].clone();

    let message = store
        .refused(
            "link",
            json!({"source_id": item, "target_id": form, "relation": "depends_on"}),
        )
        .await;
    assert!(
        message.contains("allowed pairs for depends_on"),
        "{message}"
    );
    for pair in [
        "concept:form -> concept:form",
        "concept:form_rule -> concept:field",
        "concept:form_rule -> concept:field_value",
    ] {
        assert!(message.contains(pair), "missing {pair}: {message}");
    }
    assert!(
        !message.contains("concept:form_item -> concept:form"),
        "{message}"
    );

    let b_form = scenario.loaded_b["form"]["id"].clone();
    store
        .call(
            "link",
            json!({"source_id": b_form, "target_id": form, "relation": "depends_on"}),
        )
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_write_leaves_nothing_behind() {
    let scenario = Scenario::loaded().await;
    let store = &scenario.store;
    let before = store.call("forms.export", json!({})).await;

    let mut broken = form_c();
    broken["items"][0]["field"] = json!("no such field");
    let message = store
        .refused("forms.load", json!({"document": broken}))
        .await;
    assert!(message.contains("no such field"), "{message}");

    let refused = store
        .refused(
            "forms.split",
            json!({
                "field": "sender name",
                "item": scenario.loaded_a["items"][1]["id"],
                "existing_name": "one",
                "new_name": "two",
                "new_definition": "Two."
            }),
        )
        .await;
    assert!(refused.contains("already means"), "{refused}");

    let after = store.call("forms.export", json!({})).await;
    assert_eq!(after, before);
}

const ACCT: &str = "ACCT-1";
const JOINT_RULE: &str = "Give the account type when a joint holder is named";
const CERT: &str = "CERT-1";
const CERT_RULE: &str = "A certificate number is given for common shares";

/// A form whose two rules hang on a choice field: one takes the choice as its
/// premise, the other requires the choice's item.
fn form_d() -> Value {
    json!({
        "form_id": ACCT,
        "revision": "2026-04",
        "title": "Account opening",
        "source": {"url": "https://forms.example/acct-1.pdf", "sha256": "4".repeat(64)},
        "fields": [
            {
                "name": "account type",
                "definition": "Who owns the account.",
                "value_type": "choice",
                "exclusive": true,
                "values": [
                    {"name": "individual", "definition": "One owner."},
                    {"name": "joint", "definition": "Two owners."}
                ]
            },
            {"name": "joint holder name", "definition": "Full legal name of the second owner.", "value_type": "text"}
        ],
        "items": [
            item("Account type", 1, 1, "choice", "account type"),
            item("Joint holder name", 1, 2, "text", "joint holder name")
        ],
        "rules": [
            {
                "text": "Name the joint holder once the account type is given",
                "page": 1,
                "depends_on": [{"field": "account type"}],
                "enables": [{"item": "Joint holder name"}]
            },
            {
                "text": JOINT_RULE,
                "page": 1,
                "depends_on": [{"field": "joint holder name"}],
                "enables": [{"item": "Account type"}]
            }
        ]
    })
}

/// A form whose field "amount" means something other than A's "amount", with a
/// rule that takes it as a premise.
fn form_e() -> Value {
    json!({
        "form_id": CERT,
        "revision": "2026-05",
        "title": "Share certificate request",
        "source": {"url": "https://forms.example/cert-1.pdf", "sha256": "5".repeat(64)},
        "fields": [
            {"name": "amount", "definition": "Number of shares on the certificate.", "value_type": "amount"},
            {
                "name": "share class",
                "definition": "Class of the shares.",
                "value_type": "choice",
                "exclusive": true,
                "values": [
                    {"name": "common", "definition": "Common shares."},
                    {"name": "preferred", "definition": "Preferred shares."}
                ]
            },
            {"name": "certificate number", "definition": "Number printed on the certificate.", "value_type": "identifier"}
        ],
        "items": [
            item("Number of shares", 1, 1, "text", "amount"),
            item("Share class", 1, 2, "choice", "share class"),
            item("Certificate number", 1, 3, "text", "certificate number")
        ],
        "rules": [
            {
                "text": CERT_RULE,
                "page": 1,
                "depends_on": [{"field": "amount"}, {"field": "share class", "value": "common"}],
                "enables": [{"item": "Certificate number"}]
            }
        ]
    })
}

fn kinds(reply: &Value) -> Vec<String> {
    texts(&reply["findings"], "kind")
}

fn node_of<'a>(export: &'a Value, node_type: &str, name: &str) -> &'a Value {
    export["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n["type"] == node_type && n["name"] == name)
        .unwrap_or_else(|| panic!("no {node_type} named {name}: {export}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_choice_answer_outside_its_values_answers_nothing() {
    let store = Store::new();
    store
        .call("forms.load", json!({"document": form_d()}))
        .await;

    let outside = store
        .call(
            "forms.check",
            json!({"forms": [ACCT], "answers": {"account type": "corporate"}}),
        )
        .await;
    assert_eq!(
        kinds(&outside),
        ["value_outside_domain"],
        "an answer outside the values holds no premise: {outside}"
    );

    let required = store
        .call(
            "forms.check",
            json!({"forms": [ACCT], "answers": {"account type": "corporate", "joint holder name": "Pat Doe"}}),
        )
        .await;
    let mut found = kinds(&required);
    found.sort();
    assert_eq!(
        found,
        ["rule_unmet", "value_outside_domain"],
        "an answer outside the values fills no required item: {required}"
    );
    let unmet = required["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .find(|f| f["kind"] == "rule_unmet")
        .expect("rule_unmet");
    assert_eq!(unmet["rule"], JOINT_RULE, "{required}");
    assert_eq!(unmet["unmet"][0]["name"], "Account type", "{required}");

    let within = store
        .call(
            "forms.check",
            json!({"forms": [ACCT], "answers": {"account type": "joint", "joint holder name": "Pat Doe"}}),
        )
        .await;
    assert_eq!(within["findings"], json!([]), "{within}");
    assert_eq!(within["consistent"], true, "{within}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_names_no_field_leaves_the_set_unconfirmed() {
    let scenario = Scenario::loaded().await;
    let store = &scenario.store;
    let reply = store
        .call(
            "forms.check",
            json!({"forms": [B], "answers": {"no such field": "x"}}),
        )
        .await;
    assert_eq!(reply["findings"], json!([]), "{reply}");
    assert_eq!(
        reply["unresolved_answers"]
            .as_array()
            .expect("unresolved_answers")
            .len(),
        1,
        "{reply}"
    );
    assert_eq!(
        reply["consistent"], false,
        "an answer that was never checked confirms nothing: {reply}"
    );

    let checked = store
        .call(
            "forms.check",
            json!({"forms": [B], "answers": {"transfer": "all assets"}}),
        )
        .await;
    assert_eq!(checked["consistent"], true, "{checked}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rule_on_an_undecided_field_is_not_checked_until_the_field_is_decided() {
    let store = Store::new();
    store
        .call("forms.load", json!({"document": form_a()}))
        .await;
    let loaded = store
        .call("forms.load", json!({"document": form_e()}))
        .await;
    let shares = loaded["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["label"] == "Number of shares")
        .expect("the item asking for amount");
    assert_eq!(
        shares["field"],
        Value::Null,
        "amount names another meaning: {loaded}"
    );

    let common = json!({"forms": [CERT], "answers": {"share class": "common"}});
    let waiting = store.call("forms.check", common.clone()).await;
    assert_eq!(
        waiting["findings"],
        json!([]),
        "the rule's other premise alone is not the rule: {waiting}"
    );
    let unchecked = waiting["rules_unchecked"]
        .as_array()
        .expect("rules_unchecked");
    assert_eq!(unchecked.len(), 1, "{waiting}");
    assert_eq!(unchecked[0]["rule"], CERT_RULE, "{waiting}");
    assert_eq!(unchecked[0]["pending"], json!(["amount"]), "{waiting}");
    assert_eq!(waiting["consistent"], false, "{waiting}");

    let exported = store.call("forms.export", json!({})).await;
    let replayed = Store::new();
    replayed
        .call("forms.replay", json!({"document": exported}))
        .await;
    let after_replay = replayed.call("forms.check", common.clone()).await;
    let replayed_unchecked = after_replay["rules_unchecked"]
        .as_array()
        .expect("rules_unchecked");
    assert_eq!(
        replayed_unchecked.len(),
        1,
        "the mark survives export and replay: {after_replay}"
    );
    assert_eq!(replayed_unchecked[0]["rule"], CERT_RULE, "{after_replay}");
    assert_eq!(
        replayed_unchecked[0]["pending"],
        json!(["amount"]),
        "{after_replay}"
    );

    let decided = store
        .call(
            "forms.same",
            json!({
                "item": shares["id"],
                "new_field": {"name": "share count", "definition": "Number of shares on the certificate.", "value_type": "amount"}
            }),
        )
        .await;
    assert_eq!(decided["rules_settled"], json!([CERT_RULE]), "{decided}");

    let settled = store.call("forms.check", common).await;
    assert_eq!(settled["findings"], json!([]), "{settled}");
    assert_eq!(settled["rules_unchecked"], json!([]), "{settled}");
    assert_eq!(settled["consistent"], true, "{settled}");

    let holds = store
        .call(
            "forms.check",
            json!({"forms": [CERT], "answers": {"share class": "common", "share count": "10"}}),
        )
        .await;
    assert_eq!(kinds(&holds), ["rule_unmet"], "{holds}");
    assert_eq!(holds["findings"][0]["rule"], CERT_RULE, "{holds}");
    assert_eq!(
        holds["findings"][0]["unmet"][0]["name"], "Certificate number",
        "{holds}"
    );

    let export = store.call("forms.export", json!({})).await;
    let rule = node_of(&export, "form_rule", CERT_RULE);
    assert!(
        rule["properties"].get("premises_pending").is_none(),
        "{rule}"
    );
    let item = node_of(&export, "form_item", "Number of shares");
    assert!(item["properties"].get("field_pending").is_none(), "{item}");
}

fn branch_document(values: usize, exclusive: bool) -> Value {
    let values: Vec<Value> = (1..=values)
        .map(
            |n| json!({"name": format!("branch {n}"), "definition": format!("Branch number {n}.")}),
        )
        .collect();
    json!({
        "form_id": if exclusive { "HOME-1" } else { "VISIT-1" },
        "revision": format!("2026-{:02}", values.len()),
        "title": "Branch choice",
        "source": {"url": "https://forms.example/branch.pdf", "sha256": "6".repeat(64)},
        "fields": [{
            "name": if exclusive { "home branch" } else { "visited branches" },
            "definition": "The branch the account is held at.",
            "value_type": "choice",
            "exclusive": exclusive,
            "values": values
        }],
        "items": [item("Branch", 1, 1, "choice", if exclusive { "home branch" } else { "visited branches" })]
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exclusive_field_with_more_values_than_the_cap_is_refused() {
    let store = Store::new();
    let message = store
        .refused("forms.load", json!({"document": branch_document(49, true)}))
        .await;
    assert!(message.contains("at most 48"), "{message}");
    assert!(message.contains("home branch"), "{message}");

    let at_cap = store
        .call("forms.load", json!({"document": branch_document(48, true)}))
        .await;
    assert_eq!(at_cap["loaded"], true, "{at_cap}");
    let open = store
        .call(
            "forms.load",
            json!({"document": branch_document(49, false)}),
        )
        .await;
    assert_eq!(
        open["loaded"], true,
        "the cap binds exclusive fields only: {open}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reused_exclusive_field_links_its_new_values_to_the_old() {
    let store = Store::new();
    store
        .call("forms.load", json!({"document": form_a()}))
        .await;
    let recall = json!({
        "form_id": "WIRE-3",
        "revision": "2026-06",
        "title": "Wire recall",
        "source": {"url": "https://forms.example/wire-3.pdf", "sha256": "7".repeat(64)},
        "fields": [{
            "name": "fee paid by",
            "definition": "Who pays the transfer fees.",
            "value_type": "choice",
            "exclusive": true,
            "values": [
                {"name": "sender", "definition": "The sender pays every fee."},
                {"name": "waived", "definition": "No fee is charged."},
                {"name": "split evenly", "definition": "Each side pays half of every fee."}
            ]
        }],
        "items": [item("Fee paid by", 1, 1, "choice", "fee paid by")]
    });
    let loaded = store.call("forms.load", json!({"document": recall})).await;
    assert_eq!(loaded["counts"]["fields_reused"], 1, "{loaded}");
    assert_eq!(loaded["counts"]["values"], 2, "{loaded}");

    for pair in [
        ["sender", "waived"],
        ["waived", "split evenly"],
        ["shared", "split evenly"],
    ] {
        let reply = store
            .call(
                "forms.check",
                json!({"forms": ["WIRE-3"], "answers": {"fee paid by": pair}}),
            )
            .await;
        assert_eq!(kinds(&reply), ["exclusive_values"], "{pair:?}: {reply}");
    }
}

fn sectioned(label: &str, page: u32, order: u32, section: &str, field: &str) -> Value {
    let mut entry = item(label, page, order, "text", field);
    entry["section"] = json!(section);
    entry
}

#[tokio::test(flavor = "multi_thread")]
async fn a_section_is_filled_only_when_every_item_in_it_is_answered() {
    let store = Store::new();
    store
        .call(
            "forms.load",
            json!({"document": {
                "form_id": "PART-1",
                "revision": "2026-07",
                "title": "Applicant details",
                "source": {"url": "https://forms.example/part-1.pdf", "sha256": "8".repeat(64)},
                "fields": [
                    {
                        "name": "applicant type",
                        "definition": "Whether a person or a company applies.",
                        "value_type": "choice",
                        "exclusive": true,
                        "values": [
                            {"name": "person", "definition": "A natural person."},
                            {"name": "company", "definition": "A registered company."}
                        ]
                    },
                    {"name": "company name", "definition": "Registered name of the company.", "value_type": "text"},
                    {"name": "company number", "definition": "Registration number of the company.", "value_type": "identifier"}
                ],
                "sections": [{"label": "Company details", "order": 2}],
                "items": [
                    item("Applicant type", 1, 1, "choice", "applicant type"),
                    sectioned("Company name", 1, 2, "Company details", "company name"),
                    sectioned("Company number", 1, 3, "Company details", "company number")
                ],
                "rules": [{
                    "text": "If a company applies, complete Company details",
                    "page": 1,
                    "depends_on": [{"field": "applicant type", "value": "company"}],
                    "enables": [{"section": "Company details"}]
                }]
            }}),
        )
        .await;

    let partial = store
        .call(
            "forms.check",
            json!({"forms": ["PART-1"], "answers": {"applicant type": "company", "company name": "Acme"}}),
        )
        .await;
    assert_eq!(
        kinds(&partial),
        ["rule_unmet"],
        "one answered item of two does not fill the section: {partial}"
    );
    assert_eq!(partial["consistent"], false, "{partial}");

    let full = store
        .call(
            "forms.check",
            json!({"forms": ["PART-1"], "answers": {"applicant type": "company", "company name": "Acme", "company number": "12345"}}),
        )
        .await;
    assert_eq!(full["findings"], json!([]), "{full}");
    assert_eq!(full["consistent"], true, "{full}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_section_sorts_at_the_first_page_of_what_it_contains() {
    let store = Store::new();
    store
        .call(
            "forms.load",
            json!({"document": {
                "form_id": "MIX-1",
                "revision": "2026-07",
                "title": "Contact sheet",
                "source": {"url": "https://forms.example/mix-1.pdf", "sha256": "9".repeat(64)},
                "fields": [
                    {"name": "contact name", "definition": "Name of the person to contact.", "value_type": "text"},
                    {"name": "reference", "definition": "Reference printed on the letter.", "value_type": "identifier"}
                ],
                "sections": [{"label": "Contact", "order": 1}],
                "items": [
                    sectioned("Contact name", 1, 1, "Contact", "contact name"),
                    item("Reference", 1, 2, "text", "reference")
                ]
            }}),
        )
        .await;
    let around = store
        .call("forms.neighbourhood", json!({"node": "MIX-1"}))
        .await;
    let outline = around["outline"].as_array().expect("outline");
    assert_eq!(outline.len(), 2, "{around}");
    assert_eq!(outline[0]["section"], "Contact", "{around}");
    assert_eq!(outline[1]["label"], "Reference", "{around}");
}

fn payment_form() -> Value {
    let mut by_cheque = item("By cheque", 1, 2, "choice", "payment method");
    by_cheque["value"] = json!("cheque");
    json!({
        "form_id": "PAY-1",
        "revision": "2026-08",
        "title": "Payment instruction",
        "source": {"url": "https://forms.example/pay-1.pdf", "sha256": "a".repeat(64)},
        "fields": [
            {
                "name": "payment method",
                "definition": "How the payment is made.",
                "value_type": "choice",
                "exclusive": true,
                "values": [
                    {"name": "cheque", "definition": "A paper cheque."},
                    {"name": "card", "definition": "A payment card."}
                ]
            },
            {"name": "card number", "definition": "Number printed on the payment card.", "value_type": "identifier", "aliases": ["card no"]}
        ],
        "items": [
            item("Card", 1, 1, "text", "card number"),
            by_cheque,
            item("Payment method", 1, 3, "choice", "payment method")
        ]
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_orders_every_hit_by_rank_and_names_the_wording_that_matched() {
    let store = Store::new();
    store
        .call("forms.load", json!({"document": payment_form()}))
        .await;

    let card = store.call("forms.resolve", json!({"text": "card"})).await;
    let hits = card["hits"].as_array().expect("hits");
    assert_eq!(hits.len(), 2, "{card}");
    assert_eq!(
        (&hits[0]["type"], &hits[0]["name"]),
        (&json!("field_value"), &json!("card")),
        "a value named by the text outranks a field reached through a label: {card}"
    );
    assert_eq!(
        (&hits[1]["type"], &hits[1]["name"]),
        (&json!("field"), &json!("card number")),
        "{card}"
    );
    assert_eq!(hits[1]["matched_by"], "label", "{card}");
    assert_eq!(hits[1]["matched_wording"], "Card", "{card}");

    let alias = store
        .call("forms.resolve", json!({"text": "Card No"}))
        .await;
    let fields = field_hits(&alias);
    assert_eq!(fields.len(), 1, "{alias}");
    assert_eq!(fields[0]["matched_by"], "alias", "{alias}");
    assert_eq!(fields[0]["matched_wording"], "card no", "{alias}");

    let option = store
        .call("forms.resolve", json!({"text": "by cheque"}))
        .await;
    let value = option["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .find(|hit| hit["type"] == "field_value")
        .unwrap_or_else(|| panic!("the option's value: {option}"));
    assert_eq!(value["name"], "cheque", "{option}");
    assert_eq!(value["matched_by"], "label", "{option}");
    assert_eq!(value["forms"][0]["item"], "By cheque", "{option}");
    assert_eq!(value["forms"][0]["form"], "form:PAY-1@2026-08", "{option}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proposed_field_sharing_no_word_still_gets_the_nearest_fields_of_its_type() {
    let store = Store::new();
    store
        .call(
            "forms.load",
            json!({"document": {
                "form_id": "ADDR-1",
                "revision": "2026-09",
                "title": "Change of address",
                "source": {"url": "https://forms.example/addr-1.pdf", "sha256": "b".repeat(64)},
                "fields": [{"name": "home location", "definition": "Postal location of the household.", "value_type": "text"}],
                "items": [item("Home location", 1, 1, "text", "home location")]
            }}),
        )
        .await;
    let reply = store
        .call(
            "forms.add_item",
            json!({
                "form": "ADDR-1",
                "item": {"label": "Residence", "page": 1, "order": 2, "item_kind": "text", "filled_by": "customer"},
                "field": {"name": "residence", "definition": "Dwelling address.", "value_type": "text"}
            }),
        )
        .await;
    assert_eq!(reply["status"], "no_match", "{reply}");
    assert_eq!(reply["nearest"][0]["name"], "home location", "{reply}");
    assert_eq!(
        store
            .call("forms.resolve", json!({"text": "residence"}))
            .await["hits"],
        json!([]),
        "no field was created without a decision"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn export_refuses_rather_than_drop_a_node_that_shares_its_key() {
    let store = Store::new();
    let rule = json!({
        "text": "Sign and date the form",
        "page": 1,
        "depends_on": [{"field": "signature date"}],
        "enables": [{"form": "SIGN-1"}]
    });
    store
        .call(
            "forms.load",
            json!({"document": {
                "form_id": "SIGN-1",
                "revision": "2026-09",
                "title": "Signature page",
                "source": {"url": "https://forms.example/sign-1.pdf", "sha256": "c".repeat(64)},
                "fields": [{"name": "signature date", "definition": "Date the form is signed.", "value_type": "date"}],
                "items": [item("Signature date", 1, 1, "text", "signature date")],
                "rules": [rule.clone(), rule]
            }}),
        )
        .await;
    let message = store.refused("forms.export", json!({})).await;
    assert!(message.contains("share the key"), "{message}");
    assert!(message.contains("without loss"), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn lint_lists_fields_reached_only_through_values_and_items_bound_twice() {
    let store = Store::new();
    let mut red = item("Red", 1, 1, "choice", "colour");
    red["value"] = json!("red");
    let mut blue = item("Blue", 1, 2, "choice", "colour");
    blue["value"] = json!("blue");
    let loaded = store
        .call(
            "forms.load",
            json!({"document": {
                "form_id": "COLR-1",
                "revision": "2026-09",
                "title": "Colour order",
                "source": {"url": "https://forms.example/colr-1.pdf", "sha256": "d".repeat(64)},
                "fields": [
                    {
                        "name": "colour",
                        "definition": "Colour of the ordered item.",
                        "value_type": "choice",
                        "values": [
                            {"name": "red", "definition": "Red."},
                            {"name": "blue", "definition": "Blue."}
                        ]
                    },
                    {"name": "order note", "definition": "Free text about the order.", "value_type": "text"}
                ],
                "items": [red, blue, item("Order note", 1, 3, "text", "order note")]
            }}),
        )
        .await;

    let lint = store.call("forms.lint", json!({})).await;
    let without = lint["fields_without_items"]
        .as_array()
        .expect("fields_without_items");
    assert_eq!(without.len(), 1, "{lint}");
    assert_eq!(without[0]["name"], "colour", "{lint}");
    assert_eq!(without[0]["through_values"], true, "{lint}");
    assert_eq!(lint["malformed_bindings"], json!([]), "{lint}");

    let note = loaded["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["label"] == "Order note")
        .expect("the note item");
    let colour = store.call("forms.resolve", json!({"text": "colour"})).await;
    let colour_id = field_hits(&colour)[0]["id"].clone();
    store
        .call(
            "link",
            json!({"source_id": note["id"], "target_id": colour_id, "relation": "variant_of"}),
        )
        .await;
    let lint = store.call("forms.lint", json!({})).await;
    let malformed = lint["malformed_bindings"]
        .as_array()
        .expect("malformed_bindings");
    assert_eq!(malformed.len(), 1, "{lint}");
    assert_eq!(malformed[0]["label"], "Order note", "{lint}");
    assert_eq!(
        malformed[0]["reason"], "means more than one field or value",
        "{lint}"
    );
    assert_eq!(lint["clean"], false, "{lint}");
}
