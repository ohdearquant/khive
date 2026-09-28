// Synthetic, isolated filesystem fixtures for source-walk boundaries.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::Path;

use chrono::Utc;
use khive_pack_code::source_ingest::{
    run_code_ingest, CodeSourceIngestOptions, CodeSourceIngestReport,
};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::types::{SqlStatement, SqlValue};
use tempfile::TempDir;

fn write_root(root: &Path) {
    std::fs::create_dir_all(root.join("src")).expect("source directory");
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"scan_root\"\n")
        .expect("synthetic manifest");
    std::fs::write(root.join("src/lib.rs"), "pub fn safe() {}\n").expect("synthetic Rust source");
}

async fn scan(
    root: &Path,
    languages: BTreeSet<&'static str>,
) -> (CodeSourceIngestReport, Vec<String>) {
    // Keep the database outside the source tree so WAL sidecars cannot affect the walk.
    let storage = TempDir::new().expect("isolated storage directory");
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(storage.path().join("map.db")),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("isolated runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root,
            languages,
            sweep_time: Utc::now(),
            enable_l1: false,
            enable_l1_5: true,
            enable_l2: false,
        },
    )
    .await
    .expect("L1.5 scan");

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT properties FROM entities \
                  WHERE deleted_at IS NULL AND entity_type='module'"
                .into(),
            params: vec![],
            label: Some("source_walk_module_paths".into()),
        })
        .await
        .expect("read module evidence");
    let mut paths = Vec::new();
    for row in rows {
        let text = match row.get("properties") {
            Some(SqlValue::Text(value)) => value,
            other => panic!("module properties must be JSON text: {other:?}"),
        };
        let properties: serde_json::Value =
            serde_json::from_str(text).expect("module properties JSON");
        paths.push(
            properties["source_path"]
                .as_str()
                .expect("source path")
                .to_owned(),
        );
    }
    paths.sort();
    (report, paths)
}

#[tokio::test]
async fn directory_alias_must_not_reenter_excluded_subtrees() {
    use std::os::unix::fs::symlink;

    for excluded in ["target", "node_modules", ".cache"] {
        let fixture = TempDir::new().expect("source fixture");
        let root = fixture.path();
        write_root(root);
        std::fs::create_dir(root.join(excluded)).expect("excluded directory");
        std::fs::write(
            root.join(excluded).join("generated.rs"),
            "pub fn generated() {}\n",
        )
        .expect("excluded synthetic source");

        // Positive control: the direct spelling of the excluded tree is not ingested.
        let (_, direct_paths) = scan(root, ["rust"].into_iter().collect()).await;
        assert_eq!(direct_paths, vec!["src/lib.rs"]);

        symlink(root.join(excluded), root.join("source_alias")).expect("directory alias");
        let (_, aliased_paths) = scan(root, ["rust"].into_iter().collect()).await;
        assert_eq!(
            aliased_paths, direct_paths,
            "an alias must not override canonical excluded-subtree policy: {excluded}"
        );
    }
}

#[tokio::test]
async fn dangling_non_source_link_is_not_a_rust_source_drop() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("source fixture");
    let root = fixture.path();
    write_root(root);
    symlink(root.join("missing.txt"), root.join("notes.txt")).expect("dangling metadata link");
    let (report, paths) = scan(root, ["rust"].into_iter().collect()).await;
    assert_eq!(paths, vec!["src/lib.rs"]);
    assert_eq!(
        report.files_dropped_without_source_path, 0,
        "a dangling .txt entry is not an omitted Rust source file"
    );
    // A separate traversal warning is acceptable; a false outside-source claim is not.
    assert!(!report.warnings.iter().any(|warning| {
        warning.contains("notes.txt")
            && warning.contains("skipped source outside the canonical ingest root")
    }));
}

#[tokio::test]
async fn non_source_drop_count_does_not_scale_with_selected_languages() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("source fixture");
    let root = fixture.path();
    write_root(root);
    symlink(root.join("missing.txt"), root.join("notes.txt")).expect("dangling metadata link");
    let (report, paths) = scan(root, ["rust", "python", "typescript"].into_iter().collect()).await;
    assert_eq!(paths, vec!["src/lib.rs"]);
    assert_eq!(report.languages, vec!["rust"]);
    assert_eq!(
        report.files_dropped_without_source_path, 0,
        "language selection must not multiply a non-source entry into omitted sources"
    );
}

#[tokio::test]
async fn permitted_aliases_and_directory_cycles_keep_one_module() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("source fixture");
    let root = fixture.path();
    write_root(root);
    symlink(root.join("src/lib.rs"), root.join("src/alias.rs")).expect("file alias");
    symlink(root.join("src"), root.join("source_alias")).expect("directory alias");
    symlink(root, root.join("src/back_to_root")).expect("directory cycle");
    let (report, paths) = scan(root, ["rust"].into_iter().collect()).await;
    assert_eq!(paths, vec!["src/lib.rs"]);
    assert_eq!(report.modules_created, 1);
    assert_eq!(report.files_dropped_without_source_path, 0);
}

#[tokio::test]
async fn outside_prefix_sibling_and_socket_stay_excluded() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    let fixture = TempDir::new().expect("source fixture");
    let root = fixture.path().join("source");
    let sibling = fixture.path().join("source-other");
    write_root(&root);
    std::fs::create_dir(&sibling).expect("prefix sibling");
    let external = sibling.join("external.rs");
    std::fs::write(&external, "pub fn outside() {}\n").expect("outside fixture");
    symlink(&external, root.join("src/external.rs")).expect("outside source alias");
    let _socket = UnixListener::bind(root.join("src/s.rs")).expect("non-regular source");
    let (report, paths) = scan(&root, ["rust"].into_iter().collect()).await;
    assert_eq!(paths, vec!["src/lib.rs"]);
    assert_eq!(report.files_dropped_without_source_path, 2);
    assert!(report.warnings.iter().any(|warning| {
        warning.contains("external.rs") && warning.contains("outside the canonical ingest root")
    }));
    assert!(report
        .warnings
        .iter()
        .any(|warning| { warning.contains("s.rs") && warning.contains("non-regular source") }));
}
