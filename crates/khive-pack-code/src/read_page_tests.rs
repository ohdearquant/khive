use std::path::PathBuf;
use std::sync::Arc;

use khive_runtime::{
    CodeMapConfig, KhiveRuntime, RuntimeConfig, StorageBackend, VerbRegistryBuilder,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use super::*;

struct Fixture {
    root: tempfile::TempDir,
    path: PathBuf,
    pack: CodePack,
}

fn runtime(root: &std::path::Path, path: &std::path::Path) -> KhiveRuntime {
    KhiveRuntime::from_backend(
        Arc::new(StorageBackend::memory().unwrap()),
        RuntimeConfig {
            db_path: None,
            code_map_root: Some(root.display().to_string()),
            code_maps: vec![CodeMapConfig {
                name: Some("workspace".into()),
                path: Some(path.display().to_string()),
                source_root: Some(root.display().to_string()),
                ..CodeMapConfig::default()
            }],
            ..RuntimeConfig::no_embeddings()
        },
    )
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let path = root.path().join("map.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE entities (id TEXT PRIMARY KEY, kind TEXT, entity_type TEXT, name TEXT, deleted_at INTEGER); CREATE TABLE graph_edges (source_id TEXT, target_id TEXT, relation TEXT, deleted_at INTEGER, target_backend TEXT);").unwrap();
        for (id, kind, subtype, name, deleted) in [
            ("p", "project", Some("crate"), "pkg", None),
            ("q", "project", None, "other", None),
            ("m", "concept", Some("module"), "module", None),
            ("f1", "concept", Some("function"), "parse", None),
            ("f2", "concept", Some("function"), "PARSE", None),
            ("d", "concept", Some("datatype"), "parser", None),
            ("i", "concept", Some("interface"), "reparse", None),
            ("same1", "concept", Some("function"), "same", None),
            ("same2", "concept", Some("function"), "same", None),
            ("literal", "concept", Some("function"), "100%_done", None),
            ("unicode", "concept", Some("function"), "ÉTUDE", None),
            ("foreign", "document", Some("function"), "parse", None),
            ("otherkind", "concept", Some("widget"), "parse", None),
            ("deleted", "concept", Some("function"), "parse", Some(1)),
        ] {
            connection
                .execute(
                    "INSERT INTO entities VALUES (?1,?2,?3,?4,?5)",
                    params![id, kind, subtype, name, deleted],
                )
                .unwrap();
        }
        for (source, target) in [
            ("p", "m"),
            ("m", "f1"),
            ("m", "f2"),
            ("m", "d"),
            ("m", "i"),
            ("m", "same1"),
            ("m", "same2"),
            ("m", "literal"),
            ("m", "unicode"),
            ("f1", "m"),
        ] {
            connection
                .execute(
                    "INSERT INTO graph_edges VALUES (?1,?2,'contains',NULL,NULL)",
                    params![source, target],
                )
                .unwrap();
        }
        // Neither deleted nor cross-backend containment may change ownership.
        connection.execute_batch("INSERT INTO graph_edges VALUES ('q','f1','contains',1,NULL), ('q','f1','contains',NULL,'foreign');").unwrap();
        drop(connection);
        let pack = CodePack::new(runtime(root.path(), &path));
        Self { root, path, pack }
    }
}

fn ids(result: &Value) -> Vec<&str> {
    result["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn list_projects_and_nested_code_projection_are_map_local_and_ordered() {
    let fixture = Fixture::new();
    let projects = fixture
        .pack
        .handle_list(json!({"map":"workspace","kind":"project","limit":500}))
        .await
        .unwrap();
    assert_eq!(ids(&projects), ["q", "p"]);
    assert!(projects["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["project_id"].is_null() && row["kind"] == "project"));
    let all = fixture
        .pack
        .handle_list(json!({"map":"workspace"}))
        .await
        .unwrap();
    assert_eq!(all["map"], "workspace");
    assert!(all["next_cursor"].is_null());
    assert_eq!(ids(&all).len(), 11);
    assert!(!all
        .to_string()
        .contains(fixture.root.path().to_str().unwrap()));
    let rows = all["items"].as_array().unwrap();
    let keys: Vec<_> = rows
        .iter()
        .map(|row| (row["name"].as_str().unwrap(), row["id"].as_str().unwrap()))
        .collect();
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    let scoped = fixture
        .pack
        .handle_list(json!({"map":"workspace","project_id":"p"}))
        .await
        .unwrap();
    assert_eq!(ids(&scoped).len(), 9);
    assert!(scoped["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["project_id"] == "p"));
    let kinds: std::collections::BTreeSet<_> =
        rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["project", "module", "function", "datatype", "interface"]
            .into_iter()
            .collect()
    );
}

#[tokio::test]
async fn list_cursor_pages_tied_names_without_duplicates_or_omissions() {
    let fixture = Fixture::new();
    let expected = fixture
        .pack
        .handle_list(json!({"map":"workspace"}))
        .await
        .unwrap();
    let mut got = Vec::new();
    let mut request = json!({"map":"workspace","limit":1});
    loop {
        let page = fixture.pack.handle_list(request.clone()).await.unwrap();
        got.extend(ids(&page).into_iter().map(str::to_owned));
        if page["next_cursor"].is_null() {
            break;
        }
        request["cursor"] = page["next_cursor"].clone();
        assert!(got.len() <= 11);
    }
    assert_eq!(got, ids(&expected));
}

#[tokio::test]
async fn search_ranks_literal_case_insensitive_bands_with_id_ties_and_pages() {
    let fixture = Fixture::new();
    let first = fixture
        .pack
        .handle_search(json!({"map":"workspace","query":"parse","limit":2}))
        .await
        .unwrap();
    assert_eq!(ids(&first), ["f1", "f2"]);
    assert_eq!(first["items"][0]["score"], first["items"][1]["score"]);
    let second = fixture
        .pack
        .handle_search(
            json!({"map":"workspace","query":"parse","limit":2,"cursor":first["next_cursor"]}),
        )
        .await
        .unwrap();
    assert_eq!(ids(&second), ["d", "i"]);
    assert!(second["next_cursor"].is_null());
    assert!(
        first["items"][1]["score"].as_f64().unwrap()
            > second["items"][0]["score"].as_f64().unwrap()
    );
    assert!(
        second["items"][0]["score"].as_f64().unwrap()
            > second["items"][1]["score"].as_f64().unwrap()
    );
    let literal = fixture
        .pack
        .handle_search(json!({"map":"workspace","query":"%_"}))
        .await
        .unwrap();
    assert_eq!(ids(&literal), ["literal"]);
    let unicode = fixture
        .pack
        .handle_search(json!({"map":"workspace","query":"étude"}))
        .await
        .unwrap();
    assert_eq!(ids(&unicode), ["unicode"]);
    let filtered = fixture
        .pack
        .handle_search(
            json!({"map":"workspace","query":"parse","kind":"datatype","project_id":"p"}),
        )
        .await
        .unwrap();
    assert_eq!(ids(&filtered), ["d"]);
    let empty = fixture
        .pack
        .handle_search(json!({"map":"workspace","query":"nothing"}))
        .await
        .unwrap();
    assert!(ids(&empty).is_empty());
    assert!(empty["next_cursor"].is_null());
}

#[tokio::test]
async fn cursor_binds_kind_project_query_map_and_registration_generation() {
    let fixture = Fixture::new();
    let first = fixture
        .pack
        .handle_list(json!({"map":"workspace","limit":1}))
        .await
        .unwrap();
    for change in [json!({"kind":"function"}), json!({"project_id":"p"})] {
        let mut request = json!({"map":"workspace","cursor":first["next_cursor"]});
        request
            .as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        assert!(fixture
            .pack
            .handle_list(request)
            .await
            .unwrap_err()
            .to_string()
            .contains("cursor is stale"));
    }
    assert!(fixture
        .pack
        .handle_list(json!({"map":"unregistered", "cursor":first["next_cursor"]}))
        .await
        .unwrap_err()
        .to_string()
        .contains("cursor is stale"));
    let restarted = CodePack::new(runtime(fixture.root.path(), &fixture.path));
    assert!(restarted
        .handle_list(json!({"map":"workspace","cursor":first["next_cursor"]}))
        .await
        .unwrap_err()
        .to_string()
        .contains("cursor is stale"));
    let search = fixture
        .pack
        .handle_search(json!({"map":"workspace","query":"parse","limit":1}))
        .await
        .unwrap();
    assert!(fixture
        .pack
        .handle_search(json!({"map":"workspace","query":"parser","cursor":search["next_cursor"]}))
        .await
        .unwrap_err()
        .to_string()
        .contains("cursor is stale"));
    for change in [json!({"kind":"function"}), json!({"project_id":"p"})] {
        let mut request = json!({"map":"workspace","query":"parse","cursor":search["next_cursor"]});
        request
            .as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        assert!(fixture
            .pack
            .handle_search(request)
            .await
            .unwrap_err()
            .to_string()
            .contains("cursor is stale"));
    }
    assert!(fixture
        .pack
        .handle_search(json!({"map":"workspace","query":"parse","cursor":first["next_cursor"]}))
        .await
        .unwrap_err()
        .to_string()
        .contains("cursor"));
}

#[tokio::test]
async fn absent_cursor_position_is_rejected_instead_of_skipped() {
    let fixture = Fixture::new();
    let first = fixture
        .pack
        .handle_list(json!({"map":"workspace","limit":1}))
        .await
        .unwrap();
    let mut cursor = Cursor::decode(first["next_cursor"].as_str().unwrap(), false).unwrap();
    cursor.position = Position::List {
        name: "nonexistent".into(),
        id: "absent".into(),
    };
    let token = cursor.encode().unwrap();
    assert!(fixture
        .pack
        .handle_list(json!({"map":"workspace","cursor":token}))
        .await
        .unwrap_err()
        .to_string()
        .contains("cursor is stale"));
    let search = fixture
        .pack
        .handle_search(json!({"map":"workspace","query":"parse","limit":1}))
        .await
        .unwrap();
    let mut cursor = Cursor::decode(search["next_cursor"].as_str().unwrap(), true).unwrap();
    cursor.position = Position::Search {
        score: 1,
        id: "f1".into(),
    };
    assert!(fixture
        .pack
        .handle_search(json!({"map":"workspace","query":"parse","cursor":cursor.encode().unwrap()}))
        .await
        .unwrap_err()
        .to_string()
        .contains("cursor is stale"));
}

#[tokio::test]
async fn malformed_parameters_are_rejected_before_any_map_open() {
    let fixture = Fixture::new();
    std::fs::remove_file(&fixture.path).unwrap();
    for limit in [
        json!(0),
        json!(501),
        json!(-1),
        json!(1.5),
        json!("1"),
        Value::Null,
    ] {
        let error = fixture
            .pack
            .handle_list(json!({"map":"workspace","limit":limit}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("limit") && error.contains("1..=500"),
            "{error}"
        );
    }
    for limit in [
        json!(0),
        json!(201),
        json!(-1),
        json!(1.5),
        json!("1"),
        Value::Null,
    ] {
        let error = fixture
            .pack
            .handle_search(json!({"map":"workspace","query":"parse","limit":limit}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("limit") && error.contains("1..=200"),
            "{error}"
        );
    }
    for (field, value) in [
        ("map", json!(1)),
        ("kind", json!("unknown")),
        ("project_id", json!(1)),
        ("cursor", json!("bad-token")),
        ("cursor", json!(1)),
    ] {
        let mut request = json!({"map":"workspace"});
        request[field] = value;
        assert!(fixture
            .pack
            .handle_list(request)
            .await
            .unwrap_err()
            .to_string()
            .contains(field));
    }
    for query in [json!(""), json!("  "), json!(1), Value::Null] {
        assert!(fixture
            .pack
            .handle_search(json!({"map":"workspace","query":query}))
            .await
            .unwrap_err()
            .to_string()
            .contains("query"));
    }
    assert!(fixture
        .pack
        .handle_list(json!({"map":"absent"}))
        .await
        .unwrap_err()
        .to_string()
        .contains("unknown code map"));
}

#[tokio::test]
async fn map_binding_and_sql_errors_never_expose_configured_paths() {
    let first = Fixture::new();
    let second = Fixture::new();
    Connection::open(&second.path)
        .unwrap()
        .execute("DELETE FROM entities WHERE id != 'q'", [])
        .unwrap();
    let runtime = KhiveRuntime::from_backend(
        Arc::new(StorageBackend::memory().unwrap()),
        RuntimeConfig {
            db_path: None,
            code_map_root: Some(
                std::env::temp_dir()
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string(),
            ),
            code_maps: vec![
                CodeMapConfig {
                    name: Some("one".into()),
                    path: Some(first.path.display().to_string()),
                    source_root: Some(first.root.path().display().to_string()),
                    ..CodeMapConfig::default()
                },
                CodeMapConfig {
                    name: Some("two".into()),
                    path: Some(second.path.display().to_string()),
                    source_root: Some(second.root.path().display().to_string()),
                    ..CodeMapConfig::default()
                },
            ],
            ..RuntimeConfig::no_embeddings()
        },
    );
    let pack = CodePack::new(runtime);
    let one = pack
        .handle_list(json!({"map":"one","limit":1}))
        .await
        .unwrap();
    let two = pack.handle_list(json!({"map":"two"})).await.unwrap();
    assert_eq!(ids(&two), ["q"]);
    assert!(pack
        .handle_list(json!({"map":"two","cursor":one["next_cursor"]}))
        .await
        .unwrap_err()
        .to_string()
        .contains("cursor is stale"));
    assert!(ids(&pack
        .handle_search(json!({"map":"two","query":"parse"}))
        .await
        .unwrap())
    .is_empty());
    Connection::open(&second.path)
        .unwrap()
        .execute("DROP TABLE entities", [])
        .unwrap();
    let error = pack
        .handle_list(json!({"map":"two"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("code map query failed"));
    assert!(!error.contains(second.path.to_str().unwrap()));
}

#[tokio::test]
async fn registered_dispatch_exposes_all_three_read_verbs() {
    let fixture = Fixture::new();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(fixture.pack.runtime.clone()));
    builder.register(CodePack::new(fixture.pack.runtime.clone()));
    let registry = builder.build().unwrap();
    assert_eq!(
        ids(&registry
            .dispatch("code.list", json!({"map":"workspace","kind":"project"}))
            .await
            .unwrap()),
        ["q", "p"]
    );
    assert_eq!(
        ids(&registry
            .dispatch("code.search", json!({"map":"workspace","query":"parse"}))
            .await
            .unwrap()),
        ["f1", "f2", "d", "i"]
    );
    let deps = registry
        .dispatch("code.deps", json!({"map":"workspace","id":"p"}))
        .await
        .unwrap();
    assert_eq!(deps["root"], "p");
    assert_eq!(deps["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(deps["truncated"], false);
}
