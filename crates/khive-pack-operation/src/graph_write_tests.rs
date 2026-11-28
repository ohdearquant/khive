//! The working type graph a define writes (ADR-222 Amendment 2 A2.10), against an in-memory
//! store with the kg, forms and operation packs installed.

use std::collections::BTreeMap;

use khive_runtime::{
    EdgeListFilter, KhiveRuntime, NamespaceToken, PackRegistry, RequestIdentity, RuntimeConfig,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{AtomicUnitOp, Entity};
use khive_types::{Namespace, Pack};
use serde_json::{json, Map, Value};

use khive_pack_forms as _;
use khive_pack_kg as _;

use super::{commit, live, plan, write_graph};
use crate::graph::{
    definition_key, name_key, requirement_key, slot_key, EdgeKey, Target, GRAPH_REVISION,
    OPERATION_TYPE, SLOT, STEP_TYPE,
};
use crate::pack::{stmt, OperationPack};

const TYPES: &str = "filing-types";

/// Fields the fixtures name, with their value types.
const FIELDS: [(&str, &str); 6] = [
    ("case number", "identifier"),
    ("signed date", "date"),
    ("holder name", "text"),
    ("party label", "text"),
    ("file/folio number", "identifier"),
    ("filing fee", "amount"),
];

type Snapshot = (
    BTreeMap<String, Map<String, Value>>,
    BTreeMap<EdgeKey, Map<String, Value>>,
);

struct Store {
    registry: VerbRegistry,
    runtime: KhiveRuntime,
}

fn author() -> RequestIdentity {
    RequestIdentity {
        namespace: TYPES.to_owned(),
        actor_id: Some("type-author".to_owned()),
        visible_namespaces: Vec::new(),
        ..Default::default()
    }
}

fn office() -> RequestIdentity {
    RequestIdentity {
        namespace: "local".to_owned(),
        actor_id: Some("preparer-1".to_owned()),
        visible_namespaces: vec![TYPES.to_owned()],
        ..Default::default()
    }
}

impl Store {
    async fn new() -> Self {
        let packs = vec![
            "kg".to_owned(),
            "forms".to_owned(),
            OperationPack::NAME.to_owned(),
        ];
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: packs.clone(),
            actor_id: Some("preparer-1".to_owned()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("in-memory runtime without embedding services");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("preparer-1".to_owned()));
        PackRegistry::register_packs(&packs, runtime.clone(), &mut builder)
            .expect("the operation pack and the packs it requires are discovered");
        let registry = builder.build().expect("registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("pack schema installation");
        runtime.install_edge_rules(registry.all_edge_rules());
        let store = Self { registry, runtime };
        store.load_fields().await;
        store
    }

    fn token(&self) -> NamespaceToken {
        self.runtime
            .authorize(Namespace::parse(TYPES).expect("namespace"))
            .expect("token for the type namespace")
    }

    async fn dispatch(
        &self,
        identity: RequestIdentity,
        verb: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.registry
            .dispatch_with_identity(verb, params, Some(identity))
            .await
            .map_err(|e| e.to_string())
    }

    async fn call(&self, verb: &str, mut params: Value) -> Value {
        params["namespace"] = json!(TYPES);
        self.dispatch(author(), verb, params.clone())
            .await
            .unwrap_or_else(|e| panic!("{verb} {params}: {e}"))
    }

    async fn define(&self, document: Value) -> Value {
        self.call("operation.define", json!({ "document": document }))
            .await
    }

    async fn refused(&self, document: Value) -> String {
        let params = json!({"document": document, "namespace": TYPES});
        match self.dispatch(author(), "operation.define", params).await {
            Ok(reply) => panic!("the define was accepted: {reply}"),
            Err(error) => error,
        }
    }

    async fn load_fields(&self) {
        let items: Vec<Value> = FIELDS
            .iter()
            .enumerate()
            .map(|(i, (name, _))| {
                json!({"label": name, "page": 1, "order": i + 1, "item_kind": "text",
                    "filled_by": "clerk", "about": "clerk", "field": name})
            })
            .collect();
        let definitions: Vec<Value> = FIELDS
            .iter()
            .map(|(name, value_type)| {
                json!({"name": name, "definition": format!("The {name} of a filing."), "value_type": value_type})
            })
            .collect();
        let loaded = self
            .call(
                "forms.load",
                json!({"document": {
                    "form_id": "CASE-1",
                    "revision": "2026-01",
                    "title": "Form CASE-1",
                    "source": {"url": "https://forms.example/CASE-1.pdf", "sha256": "7".repeat(64)},
                    "terms": {"kinds": ["person"], "roles": ["clerk"]},
                    "subjects": [{"id": "clerk", "kind": "person", "role": "clerk"}],
                    "fields": definitions,
                    "items": items,
                }}),
            )
            .await;
        assert_eq!(loaded["loaded"], true, "{loaded}");
    }

    async fn node(&self, key: &str) -> Option<Entity> {
        self.runtime
            .get_entity_by_key(&self.token(), "concept", key)
            .await
            .expect("key read")
    }

    /// One definition's nodes by key, without `graph_revision`, and its edges by key: what a
    /// fresh build must reproduce. A node any published version holds is read by its key, so
    /// a node left behind shows here.
    async fn graph(&self, kind: &str, id: &str) -> Snapshot {
        let namespace = self.token().namespace().clone();
        let known = super::desired(&self.runtime, namespace.as_str(), kind, id)
            .await
            .expect("published versions")
            .known;
        let live = live(
            &self.runtime,
            &self.token(),
            kind,
            id,
            &definition_key(kind, id),
            &known,
        )
        .await
        .expect("graph read");
        let nodes = live
            .nodes
            .iter()
            .map(|(key, entity)| {
                let mut properties = super::stored_properties(entity);
                properties.remove(GRAPH_REVISION);
                (key.clone(), properties)
            })
            .collect();
        let edges = live
            .edges
            .into_iter()
            .map(|(key, (_, properties))| (key, properties))
            .collect();
        (nodes, edges)
    }

    async fn exec(&self, sql: &'static str) {
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                writer.execute(stmt(sql, Vec::new())).await?;
                Ok(Box::new(Value::Null) as Box<dyn std::any::Any + Send>)
            })
        });
        self.runtime.sql().atomic_unit(op).await.expect(sql);
    }

    /// Refuse every entity insert, so a graph write that creates a node fails.
    async fn refuse_inserts(&self) {
        self.exec(
            "CREATE TRIGGER refuse_entity_inserts BEFORE INSERT ON entities \
             BEGIN SELECT RAISE(ABORT, 'entity inserts are refused'); END",
        )
        .await;
    }

    async fn allow_inserts(&self) {
        self.exec("DROP TRIGGER refuse_entity_inserts").await;
    }

    /// Remove the one edge from `source` to `target`, as a lost graph write would leave it.
    async fn remove_edge(&self, source: &str, target: &str) {
        let token = self.token();
        let source = self.node(source).await.expect("source node");
        let target = self.node(target).await.expect("target node");
        let filter = EdgeListFilter {
            source_id: Some(source.id),
            target_id: Some(target.id),
            ..Default::default()
        };
        let edges = self
            .runtime
            .list_edges(&token, filter, 10, 0)
            .await
            .expect("edges");
        assert_eq!(
            edges.len(),
            1,
            "one edge from {} to {}",
            source.name,
            target.name
        );
        let delete = khive_runtime::atomic_prepare::prepare_delete(
            &self.runtime,
            &token,
            &json!({"id": edges[0].id.0.to_string(), "hard": true}),
            None,
        )
        .await
        .expect("delete plan");
        commit(&self.runtime, &token, vec![delete])
            .await
            .expect("edge removed");
    }

    async fn has_edge(&self, source: &str, target: &str) -> bool {
        let (Some(source), Some(target)) = (self.node(source).await, self.node(target).await)
        else {
            return false;
        };
        let filter = EdgeListFilter {
            source_id: Some(source.id),
            target_id: Some(target.id),
            ..Default::default()
        };
        !self
            .runtime
            .list_edges(&self.token(), filter, 10, 0)
            .await
            .expect("edges")
            .is_empty()
    }

    async fn frontier_owes(&self, type_id: &str) -> Vec<Value> {
        let opened = self
            .dispatch(
                office(),
                "operation.open",
                json!({"type": type_id, "bindings": {"clerk": {"kind": "person", "key": "c1"}}}),
            )
            .await
            .expect("open");
        let frontier = self
            .dispatch(
                office(),
                "operation.frontier",
                json!({"operation": opened["operation_id"]}),
            )
            .await
            .expect("frontier");
        frontier["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter(|item| item["kind"] == "act_owed")
            .map(|item| item["step"].clone())
            .collect()
    }
}

fn value_type(field: &str) -> &'static str {
    FIELDS
        .iter()
        .find(|(name, _)| *name == field)
        .map(|(_, value_type)| *value_type)
        .expect("a fixture field")
}

fn requires(fields: &[&str]) -> Value {
    Value::Array(
        fields
            .iter()
            .map(|field| json!({"field": field, "value_type": value_type(field)}))
            .collect(),
    )
}

fn statuses(reply: &Value) -> BTreeMap<String, (String, Value)> {
    reply["graph"]
        .as_array()
        .unwrap_or_else(|| panic!("graph statuses in {reply}"))
        .iter()
        .map(|row| {
            (
                row["id"].as_str().expect("id").to_owned(),
                (
                    row["status"].as_str().expect("status").to_owned(),
                    row["shown_version"].clone(),
                ),
            )
        })
        .collect()
}

fn status_of(reply: &Value, id: &str) -> (String, Value) {
    statuses(reply)
        .remove(id)
        .unwrap_or_else(|| panic!("no graph status for {id} in {reply}"))
}

fn written(version: i64) -> (String, Value) {
    ("written".to_owned(), json!(version))
}

fn unchanged(version: i64) -> (String, Value) {
    ("unchanged".to_owned(), json!(version))
}

fn props(entity: &Entity) -> Map<String, Value> {
    super::stored_properties(entity)
}

fn filing(version: i64, slots: &[&str]) -> Value {
    json!({"operation_types": [{
        "id": "filing",
        "version": version,
        "slots": slots.iter().map(|slot| json!({"name": slot})).collect::<Vec<_>>(),
        "steps": [{"name": "assign", "by": "clerk", "requires": requires(&["case number"]), "outcome": true}]
    }]})
}

#[tokio::test]
async fn a_define_writes_the_graph_with_its_names_and_a_repeat_changes_nothing() {
    let store = Store::new().await;
    let document = json!({"operation_types": [{
        "id": "filing",
        "version": 1,
        "known_as": [{"vocabulary": "court", "name": "Petition"}],
        "slots": [{"name": "clerk", "known_as": [{"vocabulary": "court", "name": "Registrar"}]}],
        "steps": [{
            "name": "assign",
            "by": "clerk",
            "known_as": [{"vocabulary": "court", "name": "Docketing / intake"}],
            "requires": requires(&["case number"]),
            "outcome": true
        }]
    }]});
    let first = store.define(document.clone()).await;
    assert_eq!(status_of(&first, "filing"), written(1), "{first}");

    let type_key = definition_key(OPERATION_TYPE, "filing");
    let node = store.node(&type_key).await.expect("the type's node");
    assert_eq!(props(&node)["version"], 1);
    assert_eq!(props(&node)[GRAPH_REVISION], 1);
    assert_eq!(
        props(&node)["known_as"],
        json!([{"vocabulary": "court", "name": "Petition"}])
    );
    // A name is resolved by its key, which is held on the node it names.
    for (name, named) in [
        (
            name_key("court", OPERATION_TYPE, None, "Petition"),
            type_key.clone(),
        ),
        (
            name_key("court", SLOT, Some("filing"), "Registrar"),
            slot_key("filing", "clerk"),
        ),
        (
            name_key("court", STEP_TYPE, Some("filing"), "Docketing / intake"),
            definition_key(STEP_TYPE, "filing/assign"),
        ),
    ] {
        let holder = store.node(&name).await.unwrap_or_else(|| panic!("{name}"));
        let node = store
            .node(&named)
            .await
            .unwrap_or_else(|| panic!("{named}"));
        assert_eq!(holder.id, node.id, "{name} names {named}");
    }

    let (nodes, edges) = store.graph(OPERATION_TYPE, "filing").await;
    let requirement = requirement_key("filing/assign", None, "case number");
    assert!(nodes.contains_key(&requirement), "{nodes:?}");
    assert_eq!(
        edges.get(&EdgeKey {
            source: requirement,
            relation: "links_to",
            target: Target::Field("case number".to_owned()),
        }),
        Some(&Map::from_iter([("role".to_owned(), json!("requires"))]))
    );

    // The same document again publishes nothing and writes nothing: the node is as it was.
    let again = store.define(document).await;
    assert_eq!(again["already_published"][0]["id"], "filing", "{again}");
    assert_eq!(status_of(&again, "filing"), unchanged(1), "{again}");
    let after = store.node(&type_key).await.expect("the type's node");
    assert_eq!(after.updated_at, node.updated_at);
    assert_eq!(after.version, node.version);
    assert_eq!(after.properties, node.properties);
}

#[tokio::test]
async fn versions_two_and_three_in_either_order_leave_three_with_its_requirements() {
    let sign = |version: i64, fields: &[&str]| json!({"step_types": [{"id": "sign", "version": version, "by": "clerk", "requires": requires(fields)}]});
    let dropped = requirement_key("sign", None, "signed date");
    let kept = requirement_key("sign", None, "case number");

    let upward = Store::new().await;
    let two = upward
        .define(sign(2, &["case number", "signed date"]))
        .await;
    assert_eq!(status_of(&two, "sign"), written(2), "{two}");
    assert!(upward.node(&dropped).await.is_some());
    let three = upward.define(sign(3, &["case number"])).await;
    assert_eq!(status_of(&three, "sign"), written(3), "{three}");

    let downward = Store::new().await;
    let three = downward.define(sign(3, &["case number"])).await;
    assert_eq!(status_of(&three, "sign"), written(3), "{three}");
    // A publish below the shown version changes no node and no edge.
    let two = downward
        .define(sign(2, &["case number", "signed date"]))
        .await;
    assert_eq!(two["step_types_published"][0]["version"], 2, "{two}");
    assert_eq!(status_of(&two, "sign"), unchanged(3), "{two}");

    for store in [&upward, &downward] {
        let node = store
            .node(&definition_key(STEP_TYPE, "sign"))
            .await
            .expect("the step type's node");
        assert_eq!(props(&node)["version"], 3);
        assert!(store.node(&dropped).await.is_none(), "version 3 dropped it");
        assert!(store.node(&kept).await.is_some());
    }
    // The two orders give the same graph, graph_revision aside.
    assert_eq!(
        upward.graph(STEP_TYPE, "sign").await,
        downward.graph(STEP_TYPE, "sign").await
    );
}

#[tokio::test]
async fn pins_of_two_versions_carry_their_own_version_and_a_dropped_slot_is_removed() {
    let store = Store::new().await;
    store
        .define(
            json!({"step_types": [{"id": "sign", "version": 1, "by": "clerk",
            "requires": requires(&["case number", "signed date"])}]}),
        )
        .await;
    let pinning = |id: &str, version: i64| {
        json!({"id": id, "version": 1, "slots": [{"name": "clerk"}],
            "steps": [{"name": "sign", "step_type": {"id": "sign", "version": version}, "outcome": true}]})
    };
    let reply = store
        .define(json!({
            "step_types": [{"id": "sign", "version": 2, "by": "clerk", "requires": requires(&["case number"])}],
            "operation_types": [pinning("old", 1), pinning("new", 2)]
        }))
        .await;
    assert_eq!(status_of(&reply, "sign"), written(2), "{reply}");
    assert_eq!(status_of(&reply, "old"), written(1), "{reply}");
    assert_eq!(status_of(&reply, "new"), written(1), "{reply}");

    let sign = definition_key(STEP_TYPE, "sign");
    let node = store.node(&sign).await.expect("the step type's node");
    assert_eq!(props(&node)["version"], 2);
    assert!(store
        .node(&requirement_key("sign", None, "signed date"))
        .await
        .is_none());
    for (type_id, pinned) in [("old", 1), ("new", 2)] {
        let (_, edges) = store.graph(OPERATION_TYPE, type_id).await;
        let pins = edges
            .get(&EdgeKey {
                source: definition_key(OPERATION_TYPE, type_id),
                relation: "contains",
                target: Target::Node(sign.clone()),
            })
            .unwrap_or_else(|| panic!("{type_id} pins sign: {edges:?}"));
        assert_eq!(pins["role"], "pins");
        assert_eq!(pins["version"], pinned, "{type_id}");
    }

    // A version without a slot removes the slot's node and its edge.
    let one = store.define(filing(1, &["clerk", "witness"])).await;
    assert_eq!(status_of(&one, "filing"), written(1), "{one}");
    let witness = slot_key("filing", "witness");
    assert!(store.node(&witness).await.is_some());
    let two = store.define(filing(2, &["clerk"])).await;
    assert_eq!(status_of(&two, "filing"), written(2), "{two}");
    assert!(store.node(&witness).await.is_none());
    let (nodes, _) = store.graph(OPERATION_TYPE, "filing").await;
    assert!(!nodes.contains_key(&witness));
    assert!(nodes.contains_key(&slot_key("filing", "clerk")));
}

#[tokio::test]
async fn each_type_carries_its_own_order_on_its_own_pins_edges() {
    let store = Store::new().await;
    let step = |id: &str| json!({"id": id, "version": 1, "by": "clerk"});
    let pinned = |name: &str, after: &[&str]| json!({"name": name, "step_type": {"id": name, "version": 1}, "after": after});
    let type_ = |id: &str, first: &str, second: &str| {
        let mut last = pinned(second, &[first]);
        last["outcome"] = json!(true);
        json!({"id": id, "version": 1, "slots": [{"name": "clerk"}],
            "steps": [pinned(first, &[]), last]})
    };
    let reply = store
        .define(json!({
            "step_types": [step("intake"), step("review")],
            "operation_types": [type_("forward", "intake", "review"), type_("backward", "review", "intake")]
        }))
        .await;
    for id in ["intake", "review", "forward", "backward"] {
        assert_eq!(status_of(&reply, id), written(1), "{id}: {reply}");
    }
    for (type_id, first, second) in [
        ("forward", "intake", "review"),
        ("backward", "review", "intake"),
    ] {
        let (_, edges) = store.graph(OPERATION_TYPE, type_id).await;
        let pin = |step: &str| {
            edges
                .get(&EdgeKey {
                    source: definition_key(OPERATION_TYPE, type_id),
                    relation: "contains",
                    target: Target::Node(definition_key(STEP_TYPE, step)),
                })
                .unwrap_or_else(|| panic!("{type_id} pins {step}: {edges:?}"))["steps"]
                .clone()
        };
        assert_eq!(pin(first)[0]["after"], json!([]), "{type_id}");
        assert_eq!(pin(second)[0]["after"], json!([first]), "{type_id}");
        assert_eq!(store.frontier_owes(type_id).await, vec![json!(first)]);
    }
    // No edge joins the two step type nodes, in either direction.
    let intake = definition_key(STEP_TYPE, "intake");
    let review = definition_key(STEP_TYPE, "review");
    assert!(!store.has_edge(&intake, &review).await);
    assert!(!store.has_edge(&review, &intake).await);
}

#[tokio::test]
async fn an_order_derived_from_supplies_shows_on_its_own_types_pins_edge_only() {
    let store = Store::new().await;
    let step = |id: &str, needs: &str, makes: &str| {
        json!({"id": id, "version": 1, "by": "clerk", "requires": requires(&[needs]),
            "produces": [{"field": makes, "class": "stated"}]})
    };
    let pinned = |name: &str, supply: Option<(&str, &str, &str)>| {
        let mut step = json!({"name": name, "step_type": {"id": name, "version": 1}});
        if name == "right" {
            step["outcome"] = json!(true);
        }
        if let Some((from, field, as_field)) = supply {
            step["supplies"] = json!([{"from": from, "field": field, "as": as_field}]);
        }
        step
    };
    // `left` makes a case number `right` takes as its folio number; `right` makes a party
    // label `left` takes as its holder name. Each type supplies one way only.
    let reply = store
        .define(json!({
            "step_types": [
                step("left", "holder name", "case number"),
                step("right", "file/folio number", "party label")
            ],
            "operation_types": [
                {"id": "p", "version": 1, "slots": [{"name": "clerk"}], "steps": [
                    pinned("left", None),
                    pinned("right", Some(("left", "case number", "file/folio number")))
                ]},
                {"id": "q", "version": 1, "slots": [{"name": "clerk"}], "steps": [
                    pinned("left", Some(("right", "party label", "holder name"))),
                    pinned("right", None)
                ]}
            ]
        }))
        .await;
    assert_eq!(status_of(&reply, "p"), written(1), "{reply}");
    assert_eq!(status_of(&reply, "q"), written(1), "{reply}");
    for (type_id, later, earlier) in [("p", "right", "left"), ("q", "left", "right")] {
        let (_, edges) = store.graph(OPERATION_TYPE, type_id).await;
        let derived = |step: &str| {
            edges
                .get(&EdgeKey {
                    source: definition_key(OPERATION_TYPE, type_id),
                    relation: "contains",
                    target: Target::Node(definition_key(STEP_TYPE, step)),
                })
                .unwrap_or_else(|| panic!("{type_id} pins {step}: {edges:?}"))["steps"][0]
                ["derived_after"]
                .clone()
        };
        assert_eq!(derived(later), json!([earlier]), "{type_id}");
        assert_eq!(derived(earlier), json!([]), "{type_id}");
    }
}

#[tokio::test]
async fn products_requirements_and_slots_are_keyed_as_a2_10_says() {
    let store = Store::new().await;
    let reply = store
        .define(json!({
            "step_types": [
                {"id": "record", "version": 1, "by": "clerk",
                    "requires": [{"field": "holder name", "slot": "holder", "value_type": "text"}],
                    "produces": [
                        {"field": "signed date", "slot": "holder", "class": "stated"},
                        {"field": "signed date", "slot": "clerk", "class": "stated"}
                    ]},
                {"id": "folio", "version": 1, "by": "clerk", "requires": requires(&["file/folio number"])}
            ],
            "operation_types": (["deed", "lease"].iter().map(|id| json!({
                "id": id, "version": 1, "slots": [{"name": "clerk"}, {"name": "holder"}],
                "steps": [{"name": "record", "step_type": {"id": "record", "version": 1}, "outcome": true}]
            })).collect::<Vec<_>>())
        }))
        .await;
    for id in ["record", "folio", "deed", "lease"] {
        assert_eq!(status_of(&reply, id), written(1), "{id}: {reply}");
    }
    let (nodes, edges) = store.graph(STEP_TYPE, "record").await;
    let record = definition_key(STEP_TYPE, "record");
    // One field produced about two slots is one edge naming both.
    let produces: Vec<_> = edges
        .iter()
        .filter(|(key, properties)| {
            key.source == record && properties.get("role") == Some(&json!("produces"))
        })
        .collect();
    assert_eq!(produces.len(), 1, "{edges:?}");
    assert_eq!(
        produces[0].0.target,
        Target::Field("signed date".to_owned())
    );
    assert_eq!(produces[0].1["slots"], json!(["clerk", "holder"]));
    assert_eq!(produces[0].1["about_operation"], json!(false));
    // The requirement names its slot; no edge runs from it to either type's slot node.
    let requirement = requirement_key("record", Some("holder"), "holder name");
    assert_eq!(nodes[&requirement]["slot"], "holder");
    let from_requirement: Vec<_> = edges
        .keys()
        .filter(|key| key.source == requirement)
        .collect();
    assert_eq!(
        from_requirement,
        vec![&EdgeKey {
            source: requirement.clone(),
            relation: "links_to",
            target: Target::Field("holder name".to_owned()),
        }]
    );
    // A slot of the same name in two types is two nodes.
    let deed = store
        .node(&slot_key("deed", "holder"))
        .await
        .expect("deed's holder");
    let lease = store
        .node(&slot_key("lease", "holder"))
        .await
        .expect("lease's holder");
    assert_ne!(deed.id, lease.id);
    // A field name holding '/' is escaped, so it adds no segment to the key.
    let folio = requirement_key("folio", None, "file/folio number");
    assert_eq!(folio, "operation/requirement/folio//file%2Ffolio number");
    assert!(store.node(&folio).await.is_some());
}

#[tokio::test]
async fn names_are_refused_where_they_clash_and_taken_across_vocabularies() {
    let store = Store::new().await;
    let named = |version: i64, clerk: (&str, &str), witness: (&str, &str)| {
        json!({"operation_types": [{
            "id": "filing", "version": version,
            "slots": [
                {"name": "clerk", "known_as": [{"vocabulary": clerk.0, "name": clerk.1}]},
                {"name": "witness", "known_as": [{"vocabulary": witness.0, "name": witness.1}]}
            ],
            "steps": [{"name": "assign", "by": "clerk", "outcome": true}]
        }]})
    };
    // One vocabulary, one name, two slots: refused before anything is published.
    let clash = store
        .refused(named(1, ("court", "Officer"), ("court", "Officer")))
        .await;
    assert!(clash.contains("key_conflict"), "{clash}");
    assert!(store
        .node(&definition_key(OPERATION_TYPE, "filing"))
        .await
        .is_none());

    // The same name in two vocabularies is two keys.
    let reply = store
        .define(named(1, ("court", "Officer"), ("registry", "Officer")))
        .await;
    assert_eq!(status_of(&reply, "filing"), written(1), "{reply}");

    // A later version may not give a held name to another node of its type.
    let taken = store
        .refused(named(2, ("court", "Clerk"), ("court", "Officer")))
        .await;
    assert!(taken.contains("key_conflict"), "{taken}");
    assert!(taken.contains("existing_id"), "{taken}");

    // A published version keeps the names it was published with.
    let renamed = store
        .refused(named(1, ("court", "Officer"), ("registry", "Watcher")))
        .await;
    assert!(renamed.contains("version_conflict"), "{renamed}");

    for (vocabulary, code) in [
        ("step_type", "reserved_vocabulary"),
        ("Slot", "reserved_vocabulary"),
        ("court/appeals", "invalid_name"),
    ] {
        let refused = store
            .refused(named(3, (vocabulary, "Officer"), ("registry", "Officer")))
            .await;
        assert!(refused.contains(code), "{vocabulary}: {refused}");
    }
    let long = "n".repeat(520);
    let refused = store
        .refused(named(3, ("court", &long), ("registry", "Officer")))
        .await;
    assert!(refused.contains("invalid_name"), "{refused}");

    store
        .define(json!({"step_types": [{"id": "sign", "version": 1, "by": "clerk"}]}))
        .await;
    let pinned = store
        .refused(json!({"operation_types": [{
            "id": "deed", "version": 1, "slots": [{"name": "clerk"}],
            "steps": [{"name": "sign", "step_type": {"id": "sign", "version": 1}, "outcome": true,
                "known_as": [{"vocabulary": "court", "name": "Signing"}]}]
        }]}))
        .await;
    assert!(pinned.contains("pinned_step_shape"), "{pinned}");
}

#[tokio::test]
async fn the_publish_stands_when_the_graph_write_fails_and_the_next_define_heals_it() {
    let store = Store::new().await;
    let type_key = definition_key(OPERATION_TYPE, "filing");

    // Every attempt fails: the publish commits, the reply says failed, no node is written.
    store.refuse_inserts().await;
    let one = store.define(filing(1, &["clerk"])).await;
    assert_eq!(one["published"][0]["id"], "filing", "{one}");
    let row = &one["graph"][0];
    assert_eq!(row["status"], "failed", "{one}");
    assert!(
        row["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "{one}"
    );
    assert!(row.get("shown_version").is_none(), "{one}");
    assert!(store.node(&type_key).await.is_none());
    store.allow_inserts().await;

    // The same document again: already published, and the graph is written.
    let again = store.define(filing(1, &["clerk"])).await;
    assert_eq!(again["already_published"][0]["id"], "filing", "{again}");
    assert_eq!(status_of(&again, "filing"), written(1), "{again}");

    // A failure after a version is shown leaves the node at the old version.
    store.refuse_inserts().await;
    let two = store.define(filing(2, &["clerk", "witness"])).await;
    assert_eq!(two["published"][0]["version"], 2, "{two}");
    assert_eq!(two["graph"][0]["status"], "failed", "{two}");
    let node = store.node(&type_key).await.expect("the type's node");
    assert_eq!(props(&node)["version"], 1);
    assert!(store.node(&slot_key("filing", "witness")).await.is_none());
    store.allow_inserts().await;
    let healed = store.define(filing(2, &["clerk", "witness"])).await;
    assert_eq!(status_of(&healed, "filing"), written(2), "{healed}");

    // The healed graph is the graph a fresh build gives, graph_revision aside.
    let fresh = Store::new().await;
    fresh.define(filing(1, &["clerk"])).await;
    fresh.define(filing(2, &["clerk", "witness"])).await;
    assert_eq!(
        store.graph(OPERATION_TYPE, "filing").await,
        fresh.graph(OPERATION_TYPE, "filing").await
    );
}

/// A write that only heals version 2's graph races the write for version 3. Whichever commits
/// first, the second finds the definition node moved and reads again, and no edge of version
/// 2 is left beside the node at 3.
#[tokio::test]
async fn a_heal_for_version_two_racing_version_three_leaves_no_version_two_edge() {
    let witness = slot_key("filing", "witness");
    let type_key = definition_key(OPERATION_TYPE, "filing");
    for heal_first in [false, true] {
        let store = Store::new().await;
        let token = store.token();
        let two = store.define(filing(2, &["clerk", "witness"])).await;
        assert_eq!(status_of(&two, "filing"), written(2), "{two}");
        store.remove_edge(&type_key, &witness).await;
        let heal = plan(
            &store.runtime,
            &store.registry,
            &token,
            OPERATION_TYPE,
            "filing",
        )
        .await
        .expect("heal plan");
        assert_eq!(heal.shown, 2);
        assert!(!heal.plans.is_empty(), "the lost edge is relinked");

        if heal_first {
            // Version 3 is published while its graph write cannot create the new slot, so
            // the write for 3 is read here, before the heal commits.
            store.refuse_inserts().await;
            let three = store.define(filing(3, &["clerk", "reviewer"])).await;
            assert_eq!(three["graph"][0]["status"], "failed", "{three}");
            store.allow_inserts().await;
            let late = plan(
                &store.runtime,
                &store.registry,
                &token,
                OPERATION_TYPE,
                "filing",
            )
            .await
            .expect("plan for 3");
            assert_eq!(late.shown, 3);
            commit(&store.runtime, &token, heal.plans)
                .await
                .expect("the heal read the current node and commits");
            assert!(store.has_edge(&type_key, &witness).await);
            assert!(
                commit(&store.runtime, &token, late.plans).await.is_err(),
                "the write for 3 read the node before the heal moved it"
            );
            let again = write_graph(
                &store.runtime,
                &store.registry,
                &token,
                OPERATION_TYPE,
                "filing",
            )
            .await;
            assert_eq!(again["status"], "written", "{again}");
            assert_eq!(again["shown_version"], 3, "{again}");
        } else {
            let three = store.define(filing(3, &["clerk", "reviewer"])).await;
            assert_eq!(status_of(&three, "filing"), written(3), "{three}");
            assert!(
                commit(&store.runtime, &token, heal.plans).await.is_err(),
                "the heal read the node before the write for 3 moved it"
            );
            let again = write_graph(
                &store.runtime,
                &store.registry,
                &token,
                OPERATION_TYPE,
                "filing",
            )
            .await;
            assert_eq!(again["status"], "unchanged", "{again}");
        }

        let node = store.node(&type_key).await.expect("the type's node");
        assert_eq!(props(&node)["version"], 3, "heal first: {heal_first}");
        assert!(
            store.node(&witness).await.is_none(),
            "heal first: {heal_first}"
        );
        let (nodes, edges) = store.graph(OPERATION_TYPE, "filing").await;
        assert!(!nodes.contains_key(&witness), "heal first: {heal_first}");
        assert!(
            edges
                .keys()
                .all(|key| key.target != Target::Node(witness.clone())),
            "heal first: {heal_first}: {edges:?}"
        );
        assert!(nodes.contains_key(&slot_key("filing", "reviewer")));
    }
}

/// A node no edge leads to any more is found by its key: a heal links it again rather than
/// creating it a second time, and a version that drops it removes it.
#[tokio::test]
async fn a_node_whose_edge_was_lost_is_found_by_its_key() {
    let witness = slot_key("filing", "witness");
    let type_key = definition_key(OPERATION_TYPE, "filing");

    let store = Store::new().await;
    store.define(filing(1, &["clerk", "witness"])).await;
    let before = store.node(&witness).await.expect("the witness slot");
    store.remove_edge(&type_key, &witness).await;
    let healed = store.define(filing(1, &["clerk", "witness"])).await;
    assert_eq!(status_of(&healed, "filing"), written(1), "{healed}");
    assert!(store.has_edge(&type_key, &witness).await);
    let after = store.node(&witness).await.expect("the witness slot");
    assert_eq!(
        after.id, before.id,
        "the slot is linked again, not created again"
    );

    store.remove_edge(&type_key, &witness).await;
    let two = store.define(filing(2, &["clerk"])).await;
    assert_eq!(status_of(&two, "filing"), written(2), "{two}");
    assert!(
        store.node(&witness).await.is_none(),
        "the dropped slot is removed"
    );

    let fresh = Store::new().await;
    fresh.define(filing(1, &["clerk", "witness"])).await;
    fresh.define(filing(2, &["clerk"])).await;
    assert_eq!(
        store.graph(OPERATION_TYPE, "filing").await,
        fresh.graph(OPERATION_TYPE, "filing").await
    );
}
