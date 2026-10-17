use khive_runtime::{CodeMapConfig, KhiveRuntime, RuntimeConfig, RuntimeError};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::CodePack;

struct Fixture {
    _directory: tempfile::TempDir,
    connection: Connection,
    pack: CodePack,
    path: std::path::PathBuf,
}

fn id(value: u128) -> String {
    Uuid::from_u128(value).to_string()
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registered-map.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(
            "CREATE TABLE entities (id TEXT PRIMARY KEY, kind TEXT, entity_type TEXT, name TEXT, deleted_at INTEGER);
             CREATE TABLE graph_edges (source_id TEXT, target_id TEXT, relation TEXT, deleted_at INTEGER, target_backend TEXT, metadata TEXT);",
        ).unwrap();
        let config = RuntimeConfig {
            db_path: None,
            code_map_root: Some(directory.path().display().to_string()),
            code_maps: vec![CodeMapConfig {
                name: Some("workspace".into()),
                path: Some(path.display().to_string()),
                source_root: Some(directory.path().display().to_string()),
                ..CodeMapConfig::default()
            }],
            ..RuntimeConfig::no_embeddings()
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        runtime.code_map_registry().resolve("workspace").unwrap();
        Self {
            _directory: directory,
            connection,
            pack: CodePack::new(runtime),
            path,
        }
    }

    fn entity(&self, number: u128, kind: &str, entity_type: Option<&str>) {
        self.connection
            .execute(
                "INSERT INTO entities (id, kind, entity_type, name) VALUES (?1, ?2, ?3, ?4)",
                params![id(number), kind, entity_type, format!("node-{number}")],
            )
            .unwrap();
    }

    fn dependency(&self, source: u128, target: u128, scope: &str) {
        self.connection.execute(
            "INSERT INTO graph_edges (source_id, target_id, relation, metadata) VALUES (?1, ?2, 'depends_on', ?3)",
            params![id(source), id(target), json!({"dependency_scopes": [scope]}).to_string()],
        ).unwrap();
    }

    fn chain() -> Self {
        let fixture = Self::new();
        for number in [1, 2, 3, 4, 5] {
            fixture.entity(number, "project", Some("library"));
        }
        fixture.dependency(5, 1, "normal");
        fixture.dependency(2, 5, "normal");
        fixture.dependency(3, 1, "dev");
        fixture.dependency(4, 1, "build");
        fixture
    }

    async fn read(&self, mut args: Value) -> Value {
        args["map"] = json!("workspace");
        let value = self.pack.handle_deps(args).await.unwrap();
        assert_eq!(value["map"], "workspace");
        assert!(!value.to_string().contains(self.path.to_str().unwrap()));
        value
    }
}

fn node_ids(value: &Value) -> Vec<String> {
    value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap().to_string())
        .collect()
}

fn edge_pairs(value: &Value) -> Vec<(String, String)> {
    value["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            (
                edge["source"].as_str().unwrap().to_string(),
                edge["target"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn dependencies_follow_every_scope_in_breadth_first_order() {
    let fixture = Fixture::chain();
    let first = fixture
        .read(json!({"id": id(1), "direction": "incoming", "max_depth": 1}))
        .await;
    assert_eq!(node_ids(&first), [1, 3, 4, 5].map(id));
    assert_eq!(first["root"], id(1));
    assert_eq!(first["truncated"], false);
    assert!(first["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|node| node["kind"] == "project"));
    assert_eq!(
        edge_pairs(&first),
        vec![(id(3), id(1)), (id(4), id(1)), (id(5), id(1))]
    );

    let second = fixture
        .read(json!({"id": id(1), "direction": "incoming", "max_depth": 2}))
        .await;
    assert_eq!(node_ids(&second), [1, 3, 4, 5, 2].map(id));
    assert_eq!(second["truncated"], false);
    assert_eq!(
        second,
        fixture
            .read(json!({"id": id(1), "direction": "incoming", "max_depth": 2}))
            .await
    );
    let outgoing = fixture
        .read(json!({"id": id(2), "direction": "outgoing"}))
        .await;
    assert_eq!(node_ids(&outgoing), [2, 5, 1].map(id));
    let defaults = fixture.read(json!({"id": id(1)})).await;
    assert_eq!(defaults, second);
}

#[tokio::test]
async fn dependency_limit_keeps_cross_edges_and_reports_only_omitted_nodes() {
    let fixture = Fixture::chain();
    fixture.dependency(3, 4, "normal");
    fixture.dependency(1, 3, "normal");
    let limited = fixture
        .read(json!({"id": id(1), "direction": "incoming", "limit": 3}))
        .await;
    assert_eq!(node_ids(&limited), [1, 3, 4].map(id));
    assert_eq!(limited["truncated"], true);
    assert_eq!(
        edge_pairs(&limited),
        vec![
            (id(1), id(3)),
            (id(3), id(1)),
            (id(3), id(4)),
            (id(4), id(1))
        ]
    );
    let root_only = fixture.read(json!({"id": id(1), "limit": 1})).await;
    assert_eq!(node_ids(&root_only), [id(1)]);
    assert_eq!(root_only["truncated"], true);
    let exact_depth = fixture
        .read(json!({"id": id(1), "direction": "incoming", "max_depth": 1, "limit": 4}))
        .await;
    assert_eq!(node_ids(&exact_depth), [1, 3, 4, 5].map(id));
    assert_eq!(exact_depth["truncated"], false);
    let exact_nodes = fixture.read(json!({"id": id(1), "limit": 5})).await;
    assert_eq!(node_ids(&exact_nodes), [1, 3, 4, 5, 2].map(id));
    assert_eq!(exact_nodes["truncated"], false);
    fixture.entity(9, "project", None);
    let isolated = fixture.read(json!({"id": id(9), "limit": 1})).await;
    assert_eq!(node_ids(&isolated), [id(9)]);
    assert_eq!(isolated["truncated"], false);
}

#[tokio::test]
async fn dependency_population_excludes_deleted_foreign_and_external_targets() {
    let fixture = Fixture::new();
    fixture.entity(1, "project", Some("repository"));
    for (number, kind) in [
        (2, "module"),
        (3, "function"),
        (4, "datatype"),
        (5, "interface"),
    ] {
        fixture.entity(number, "concept", Some(kind));
        fixture.dependency(1, number, "normal");
    }
    fixture.entity(6, "concept", Some("document"));
    fixture.entity(7, "project", None);
    fixture.entity(8, "project", None);
    fixture.entity(9, "project", None);
    fixture.entity(10, "concept", Some("function"));
    for number in 6..=10 {
        fixture.dependency(1, number, "normal");
    }
    fixture
        .connection
        .execute("UPDATE entities SET deleted_at = 1 WHERE id = ?1", [id(7)])
        .unwrap();
    fixture
        .connection
        .execute(
            "UPDATE graph_edges SET deleted_at = 1 WHERE target_id = ?1",
            [id(8)],
        )
        .unwrap();
    fixture
        .connection
        .execute(
            "UPDATE graph_edges SET target_backend = 'other-map' WHERE target_id = ?1",
            [id(9)],
        )
        .unwrap();
    fixture
        .connection
        .execute(
            "UPDATE graph_edges SET relation = 'contains' WHERE target_id = ?1",
            [id(10)],
        )
        .unwrap();
    fixture.dependency(1, 99, "normal");
    let value = fixture
        .read(json!({"id": id(1), "direction": "outgoing", "max_depth": 1}))
        .await;
    assert_eq!(node_ids(&value), [1, 2, 3, 4, 5].map(id));
    let kinds: Vec<_> = value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["project", "module", "function", "datatype", "interface"]
    );
    assert_eq!(value["edges"].as_array().unwrap().len(), 4);
    assert_eq!(value["truncated"], false);
    for number in [6, 7, 99] {
        let error = fixture
            .pack
            .handle_deps(json!({"map": "workspace", "id": id(number)}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("id was not found"), "{error}");
    }
}

#[tokio::test]
async fn dependency_validation_precedes_map_resolution_and_open() {
    let pack = CodePack::new(KhiveRuntime::memory().unwrap());
    for (parameter, values) in [
        (
            "max_depth",
            vec![
                json!(0),
                json!(11),
                json!(-1),
                json!(1.5),
                json!("2"),
                Value::Null,
            ],
        ),
        (
            "limit",
            vec![
                json!(0),
                json!(1001),
                json!(-1),
                json!(1.5),
                json!("2"),
                Value::Null,
            ],
        ),
        ("direction", vec![json!("sideways"), json!(2), Value::Null]),
    ] {
        for value in values {
            let mut args = json!({"map": "not-registered", "id": id(1)});
            args[parameter] = value;
            let error = pack.handle_deps(args).await.unwrap_err();
            assert!(matches!(&error, RuntimeError::InvalidInput(_)), "{error}");
            assert!(error.to_string().contains(parameter), "{error}");
            assert!(!error.to_string().contains("unknown code map"), "{error}");
        }
    }
    for parameter in ["map", "id"] {
        for value in [json!(""), json!(" "), json!(3), Value::Null] {
            let mut args = json!({"map": "not-registered", "id": id(1)});
            args[parameter] = value;
            let error = pack.handle_deps(args).await.unwrap_err();
            assert!(error.to_string().contains(parameter), "{error}");
            assert!(!error.to_string().contains("unknown code map"), "{error}");
        }
    }
    let unknown = pack
        .handle_deps(json!({"map": "not-registered", "id": id(1)}))
        .await
        .unwrap_err();
    assert!(unknown.to_string().contains("unknown code map"));
    let location = pack
        .handle_deps(json!({"map": "not-registered", "id": id(1), "db": "hidden-path"}))
        .await
        .unwrap_err();
    assert!(!location.to_string().contains("hidden-path"));
    assert!(!location.to_string().contains("unknown code map"));
}

#[tokio::test]
async fn dependency_ids_resolve_only_inside_the_selected_map() {
    let first = Fixture::chain();
    let second = Fixture::new();
    second.entity(1, "project", None);
    let absent = second
        .pack
        .handle_deps(json!({"map": "workspace", "id": id(2)}))
        .await
        .unwrap_err();
    assert!(absent.to_string().contains("id was not found"));
    let local = second.read(json!({"id": id(1)})).await;
    assert_eq!(node_ids(&local), [id(1)]);
    assert!(local["edges"].as_array().unwrap().is_empty());
    assert_eq!(local["truncated"], false);
    assert_eq!(
        first.read(json!({"id": id(1)})).await["nodes"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}
