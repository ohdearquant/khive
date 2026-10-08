use super::*;
use tempfile::TempDir;

// ── F201 test helpers ─────────────────────────────────────────────────────

/// Create a minimal git repository under `dir` with the given NDJSON content
/// inside `.khive/kg/`. Returns the URL-style path suitable for `git clone`.
fn make_git_remote(dir: &Path, entities_ndjson: &str, edges_ndjson: &str) -> String {
    let kg_dir = dir.join(".khive/kg");
    std::fs::create_dir_all(&kg_dir).unwrap();
    std::fs::write(kg_dir.join("entities.ndjson"), entities_ndjson).unwrap();
    std::fs::write(kg_dir.join("edges.ndjson"), edges_ndjson).unwrap();

    // Initialise git repo with a single commit on `main`.
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", "init"]);

    dir.to_string_lossy().into_owned()
}

fn run_git(dir: &Path, args: &[&str]) {
    // Hermetic: user-level core.hooksPath (e.g. the machine-wide JSON/JSONL
    // data-leak guard) must not run against fixture commits in temp repos.
    let status = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
    assert!(
        status.success(),
        "git {} exited with {}",
        args.join(" "),
        status
    );
}

/// Compute the canonical `SnapshotId` for entity/edge NDJSON strings without
/// touching the filesystem, so we can build expected pins from in-memory data.
fn compute_pin(entities_ndjson: &str, edges_ndjson: &str, namespace: &str) -> SnapshotId {
    let tmp = TempDir::new().unwrap();
    let kg = tmp.path().join(".khive/kg");
    std::fs::create_dir_all(&kg).unwrap();
    std::fs::write(kg.join("entities.ndjson"), entities_ndjson).unwrap();
    std::fs::write(kg.join("edges.ndjson"), edges_ndjson).unwrap();

    let entities = read_entities(&kg.join("entities.ndjson")).unwrap();
    let edges = read_edges(&kg.join("edges.ndjson")).unwrap();
    let archive = build_kg_archive(namespace, &entities, &edges).unwrap();
    snapshot_id_for_archive(&archive).unwrap()
}

#[test]
fn remote_snapshot_pin_detects_edge_metadata_only_change() {
    let source = "11111111-1111-1111-1111-111111111111";
    let target = "22222222-2222-2222-2222-222222222222";
    let edge_id = "33333333-3333-3333-3333-333333333333";
    let entities = [
        format!(r#"{{"id":"{source}","kind":"concept","name":"A"}}"#),
        format!(r#"{{"id":"{target}","kind":"concept","name":"B"}}"#),
    ]
    .join("\n");
    let before = format!(
        r#"{{"edge_id":"{edge_id}","source":"{source}","target":"{target}","relation":"extends","weight":0.8,"properties":{{"confidence":0.4}}}}"#
    );
    let after = format!(
        r#"{{"edge_id":"{edge_id}","source":"{source}","target":"{target}","relation":"extends","weight":0.8,"properties":{{"confidence":0.9}}}}"#
    );

    assert_ne!(
        compute_pin(&entities, &before, "remote-ns"),
        compute_pin(&entities, &after, "remote-ns"),
        "remote snapshot identity must cover edge properties"
    );
}

// ── test_run_sync_local_path_unchanged_behavior ───────────────────────────

fn write_repo(dir: &Path, entities_ndjson: &str, edges_ndjson: &str) {
    let kg_dir = dir.join(".khive/kg");
    std::fs::create_dir_all(&kg_dir).unwrap();
    std::fs::write(kg_dir.join("entities.ndjson"), entities_ndjson).unwrap();
    std::fs::write(kg_dir.join("edges.ndjson"), edges_ndjson).unwrap();
}

#[tokio::test]
async fn sync_empty_ndjson_produces_real_sqlite_file() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    write_repo(repo, "", "");

    let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
    assert_eq!(report.entities, 0);
    assert_eq!(report.edges, 0);

    let bytes = std::fs::read(&db_path).unwrap();
    assert!(!bytes.is_empty(), "DB file must be non-empty after sync");
    assert!(
        bytes.starts_with(b"SQLite format 3\0"),
        "DB file must start with SQLite magic header, got {:?}",
        &bytes[..bytes.len().min(20)]
    );
}

#[tokio::test]
async fn sync_refuses_live_target_wal_before_replacement() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join("working.db");
    write_repo(repo, "", "");

    // Keep an actual SQLite writer open with uncheckpointed WAL frames.
    // Replacing only the main file here would pair those frames with the
    // newly built DB on the next open.
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE old_data (value TEXT);
             INSERT INTO old_data VALUES ('still-open');",
    )
    .unwrap();
    let wal_path = with_extension_suffix(&db_path, "-wal");
    assert!(fs::metadata(&wal_path).unwrap().len() > 0);
    let original = fs::read(&db_path).unwrap();

    let err = run_sync(repo, &db_path, "test-ns")
        .await
        .expect_err("live WAL must stop sync before replacement");
    assert!(err.to_string().contains("SQLite sidecar"), "{err:#}");
    assert_eq!(fs::read(&db_path).unwrap(), original);
    drop(db);
}

#[tokio::test]
async fn sync_refuses_target_held_by_another_sync() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join("working.db");
    write_repo(repo, "", "");
    let lock_path = with_extension_suffix(&db_path, ".sync.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    fs4::FileExt::try_lock(&lock).unwrap();

    let err = run_sync(repo, &db_path, "test-ns")
        .await
        .expect_err("another sync's lock must stop this call");
    assert!(
        err.to_string().contains("sync already owns target"),
        "{err:#}"
    );
    assert!(!db_path.exists());
    drop(lock);
}

#[tokio::test]
async fn sync_does_not_reuse_crashed_fixed_temp_name() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join("working.db");
    write_repo(repo, "", "");
    let old_temp = with_extension_suffix(&db_path, ".tmp");
    let old_wal = with_extension_suffix(&old_temp, "-wal");
    fs::write(&old_temp, b"old temporary database").unwrap();
    fs::write(&old_wal, b"old temporary WAL").unwrap();

    run_sync(repo, &db_path, "test-ns").await.unwrap();
    assert_eq!(fs::read(old_temp).unwrap(), b"old temporary database");
    assert_eq!(fs::read(old_wal).unwrap(), b"old temporary WAL");
}

#[tokio::test]
async fn sync_accepts_registered_resource_kind_and_rejects_nonsense() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join("working.db");
    let entity_id = "11111111-1111-1111-1111-111111111111";
    let resource = format!(r#"{{"id":"{entity_id}","kind":"resource","name":"Runbook"}}"#);
    write_repo(repo, &resource, "");

    let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
    assert_eq!(report.entities, 1);
    let db =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let stored_kind: String = db
        .query_row(
            "SELECT kind FROM entities WHERE id = ?1",
            [entity_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_kind, "resource");
    drop(db);

    let nonsense = format!(r#"{{"id":"{entity_id}","kind":"nonsense","name":"Bad"}}"#);
    write_repo(repo, &nonsense, "");
    let invalid_db = repo.join("invalid.db");
    let err = run_sync(repo, &invalid_db, "test-ns")
        .await
        .expect_err("unknown pack kind must be rejected");
    assert!(format!("{err:#}").contains("unknown kind \"nonsense\""));
    assert!(
        !invalid_db.exists(),
        "invalid input must fail before DB build"
    );
}

#[tokio::test]
async fn sync_imports_entities_and_edges_into_real_db() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");

    let id_a = "11111111-1111-1111-1111-111111111111";
    let id_b = "22222222-2222-2222-2222-222222222222";
    let edge_id = "33333333-3333-3333-3333-333333333333";

    let line_a =
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"Alpha","properties":{{}},"tags":[]}}"#);
    let line_b =
        format!(r#"{{"id":"{id_b}","kind":"concept","name":"Beta","properties":{{}},"tags":[]}}"#);
    let entities = format!("{line_a}\n{line_b}\n");
    let edges = format!(
        r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":1.0,"properties":{{}}}}"#
    );
    write_repo(repo, &entities, &edges);

    let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
    assert_eq!(report.entities, 2);
    assert_eq!(report.edges, 1);

    let ns = khive_types::Namespace::parse("test-ns").unwrap();
    let config = RuntimeConfig {
        db_path: Some(db_path.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..RuntimeConfig::default()
    };
    let rt = KhiveRuntime::new(config).unwrap();
    let token = rt.authorize(ns).unwrap();
    let alpha = rt
        .entities(&token)
        .unwrap()
        .get_entity(id_a.parse().unwrap())
        .await
        .unwrap()
        .expect("entity Alpha must be retrievable after sync");
    assert_eq!(alpha.name, "Alpha");
    assert_eq!(alpha.kind, "concept");
}

/// Chunk-boundary round-trip across `SYNC_CHUNK_SIZE`. See
/// `docs/api/sync.md#local-sync--run_sync`.
#[tokio::test]
async fn sync_chunk_boundary_round_trip() {
    const N: usize = 11; // > SYNC_CHUNK_SIZE (5 in test mode)

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");

    // Generate N synthetic entity lines with predictable UUIDs.
    let ids: Vec<uuid::Uuid> = (0..N).map(|_| uuid::Uuid::new_v4()).collect();
    // #476 requires entities.ndjson to be in canonical ascending-by-id
    // order (matching `write_sorted_entities`), so sort the lines below
    // even though `ids` itself stays in generation order (used to build
    // the wrap-around edges and to spot-check the last-generated entity).
    let mut entity_lines: Vec<(uuid::Uuid, String)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                (
                    *id,
                    format!(
                        r#"{{"id":"{id}","kind":"concept","name":"SyntheticEntity{i}","description":"Synthetic test entity {i} for chunk-boundary coverage","properties":{{}},"tags":["bench","synthetic"]}}"#
                    ),
                )
            })
            .collect();
    entity_lines.sort_by(|a, b| {
        a.0.to_string()
            .to_ascii_lowercase()
            .cmp(&b.0.to_string().to_ascii_lowercase())
    });
    let entities_ndjson = entity_lines
        .into_iter()
        .map(|(_, line)| line)
        .collect::<Vec<_>>()
        .join("\n");

    // Generate N synthetic edges: each entity points at the next one via
    // "extends". The last entity wraps back to the first. #476 requires
    // edges.ndjson sorted by (source, target, relation), matching
    // `write_sorted_edges`.
    let mut edge_lines: Vec<((String, String, String), String)> = ids
            .iter()
            .enumerate()
            .map(|(i, &src)| {
                let tgt = ids[(i + 1) % N];
                let eid = uuid::Uuid::new_v4();
                let key = (src.to_string(), tgt.to_string(), "extends".to_string());
                let line = format!(
                    r#"{{"edge_id":"{eid}","source":"{src}","target":"{tgt}","relation":"extends","weight":0.9,"properties":{{}}}}"#
                );
                (key, line)
            })
            .collect();
    edge_lines.sort_by(|a, b| a.0.cmp(&b.0));
    let edges_ndjson = edge_lines
        .into_iter()
        .map(|(_, line)| line)
        .collect::<Vec<_>>()
        .join("\n");

    write_repo(repo, &entities_ndjson, &edges_ndjson);

    let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
    assert_eq!(report.entities, N, "all {N} entities must be written");
    assert_eq!(report.edges, N, "all {N} edges must be written");

    // Spot-check: the last entity (in the final partial chunk) must be readable.
    let last_id = *ids.last().unwrap();
    let ns = khive_types::Namespace::parse("test-ns").unwrap();
    let config = RuntimeConfig {
        db_path: Some(db_path.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..RuntimeConfig::default()
    };
    let rt = KhiveRuntime::new(config).unwrap();
    let token = rt.authorize(ns).unwrap();
    let last_entity = rt
        .entities(&token)
        .unwrap()
        .get_entity(last_id)
        .await
        .unwrap()
        .expect("last entity (final chunk) must be readable after sync");
    assert_eq!(
        last_entity.name,
        format!("SyntheticEntity{}", N - 1),
        "last entity name must match"
    );
}

/// Error-abort: a parse failure before any DB write leaves the existing DB intact.
/// This covers the failure-semantics contract: sync aborts on the first error
/// and the target DB is never replaced by a partial result.
#[tokio::test]
async fn sync_aborts_on_invalid_ndjson_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    std::fs::write(&db_path, b"ORIGINAL").unwrap();

    // Mix valid entities with one invalid line to trigger a parse error
    // before any DB write.
    let id_a = uuid::Uuid::new_v4();
    let good_line =
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"Good","properties":{{}},"tags":[]}}"#);
    let bad_ndjson = format!("{good_line}\nnot-valid-json\n");
    write_repo(repo, &bad_ndjson, "");

    let err = run_sync(repo, &db_path, "test-ns")
        .await
        .expect_err("sync must fail on invalid NDJSON");
    assert!(
        err.to_string().contains("parsing entity")
            || err.chain().any(|e| e.to_string().contains("expected")),
        "error must describe the parse failure, got: {err}"
    );

    // DB must be untouched (atomic rename guarantee).
    let after = std::fs::read(&db_path).unwrap();
    assert_eq!(
        after, b"ORIGINAL",
        "failed sync must not replace existing DB"
    );
}

/// ADR-115 Amendment 1 §3: an edge record carrying the runtime-owned
/// `khive:secret_gate` property key must fail the sync before the tmp DB
/// replaces the working DB — sync is a caller-controlled write path.
#[tokio::test]
async fn sync_rejects_edge_with_reserved_secret_gate_property() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    std::fs::write(&db_path, b"ORIGINAL").unwrap();

    let id_a = "11111111-1111-1111-1111-111111111111";
    let id_b = "22222222-2222-2222-2222-222222222222";
    let ent_a =
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#);
    let ent_b =
        format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#);
    let edge_id = "33333333-3333-3333-3333-333333333333";
    let edge = format!(
        r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.8,"properties":{{"khive:secret_gate":"exempted:content-sha256-manifest-v1"}}}}"#
    );
    write_repo(repo, &format!("{ent_a}\n{ent_b}\n"), &format!("{edge}\n"));

    let err = run_sync(repo, &db_path, "test-ns")
        .await
        .expect_err("sync must reject the reserved edge property key");
    assert!(
        err.chain()
            .any(|e| e.to_string().contains("khive:secret_gate")),
        "error must name the reserved key, got: {err:#}"
    );

    let after = std::fs::read(&db_path).unwrap();
    assert_eq!(
        after, b"ORIGINAL",
        "failed sync must not replace existing DB"
    );
}

/// ADR-115 Amendment 1 §3: a credential-shaped value in edge
/// properties fails the sync before the tmp DB replaces the working DB.
#[tokio::test]
async fn sync_rejects_edge_with_credential_shaped_property() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    std::fs::write(&db_path, b"ORIGINAL").unwrap();

    let id_a = "11111111-1111-1111-1111-111111111111";
    let id_b = "22222222-2222-2222-2222-222222222222";
    let ent_a =
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#);
    let ent_b =
        format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#);
    let edge_id = "33333333-3333-3333-3333-333333333333";
    let edge = format!(
        r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.8,"properties":{{"api_key":"AKIAFAKEKEY1234567890"}}}}"#
    );
    write_repo(repo, &format!("{ent_a}\n{ent_b}\n"), &format!("{edge}\n"));

    let err = run_sync(repo, &db_path, "test-ns")
        .await
        .expect_err("sync must reject the credential-shaped edge property");
    assert!(
        err.chain()
            .any(|e| e.to_string().contains("properties rejected")),
        "error must attribute the rejection to edge properties, got: {err:#}"
    );

    let after = std::fs::read(&db_path).unwrap();
    assert_eq!(
        after, b"ORIGINAL",
        "failed sync must not replace existing DB"
    );
}

#[tokio::test]
async fn sync_is_atomic_via_tmp_rename() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    std::fs::write(&db_path, b"SENTINEL").unwrap();

    write_repo(repo, "not json\n", "");
    let err = run_sync(repo, &db_path, "test-ns").await.unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("parsing entity")
            || err.chain().any(|e| e.to_string().contains("expected")),
        "expected parse error, got: {err}"
    );

    let after = std::fs::read(&db_path).unwrap();
    assert_eq!(
        after, b"SENTINEL",
        "atomic guarantee: failed sync must not replace existing DB"
    );
}

#[tokio::test]
async fn sync_missing_ndjson_files_succeeds_with_zero_counts() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");

    let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
    assert_eq!(report.entities, 0);
    assert_eq!(report.edges, 0);
}

// ── #476: validate-first gate — one test per violation type ─────────────────
//
// Each test writes a sentinel DB file, runs `run_sync` against NDJSON that
// violates exactly one structural rule, asserts `run_sync` returns `Err`,
// and asserts the sentinel DB file is byte-unchanged: the violation must be
// caught before the temp DB is even created, let alone renamed over the
// target.

async fn assert_sync_rejected_before_db_write(
    repo: &Path,
    db_path: &Path,
    entities_ndjson: &str,
    edges_ndjson: &str,
    expected_substr: &str,
) {
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    std::fs::write(db_path, b"SENTINEL").unwrap();
    write_repo(repo, entities_ndjson, edges_ndjson);

    let err = run_sync(repo, db_path, "test-ns")
        .await
        .expect_err("run_sync must reject invalid NDJSON");
    assert!(
        err.chain().any(|e| e.to_string().contains(expected_substr)),
        "error must mention {expected_substr:?}, got: {err:#}"
    );

    let after = std::fs::read(db_path).unwrap();
    assert_eq!(
        after, b"SENTINEL",
        "rejected sync must leave the target DB completely untouched"
    );
}

#[tokio::test]
async fn sync_rejects_unknown_entity_kind_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id = "11111111-1111-1111-1111-111111111111";
    let entities = format!(
        r#"{{"id":"{id}","kind":"not-a-real-kind","name":"Bad","properties":{{}},"tags":[]}}"#
    );

    assert_sync_rejected_before_db_write(repo, &db_path, &entities, "", "unknown kind").await;
}

/// The normal parser accepts aliases, but sync must not store one as the
/// base kind: kind-filtered reads and a subsequent export use canonical
/// names. Both the subtype alias and a case variant failed open before.
#[tokio::test]
async fn sync_rejects_noncanonical_entity_kind_before_db_write() {
    let id = "11111111-1111-1111-1111-111111111111";
    for (input, canonical) in [("Paper", "document"), ("Concept", "concept")] {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let entities = format!(
            r#"{{"id":"{id}","kind":"{input}","name":"Alias","properties":{{}},"tags":[]}}"#
        );
        let expected = format!("non-canonical kind {input:?}; use {canonical:?}");
        assert_sync_rejected_before_db_write(repo, &db_path, &entities, "", &expected).await;
    }
}

/// ADR-115 Amendment 1 §3 applies to entity properties as well as edge
/// properties. The sentinel proves this is rejected before DB replacement.
#[tokio::test]
async fn sync_rejects_entity_with_reserved_secret_gate_property_before_db_write() {
    let id = "11111111-1111-1111-1111-111111111111";
    for kind in ["concept", "resource"] {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let entities = format!(
            r#"{{"id":"{id}","kind":"{kind}","name":"A","properties":{{"khive:secret_gate":"exempted:content-sha256-manifest-v1"}},"tags":[]}}"#
        );
        assert_sync_rejected_before_db_write(repo, &db_path, &entities, "", "khive:secret_gate")
            .await;
    }
}

#[tokio::test]
async fn sync_rejects_whitespace_only_entity_name_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id = "11111111-1111-1111-1111-111111111111";
    let entities =
        format!(r#"{{"id":"{id}","kind":"concept","name":"   ","properties":{{}},"tags":[]}}"#);

    assert_sync_rejected_before_db_write(repo, &db_path, &entities, "", "non-blank name").await;
}

#[tokio::test]
async fn sync_rejects_out_of_range_edge_weight_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id_a = "11111111-1111-1111-1111-111111111111";
    let id_b = "22222222-2222-2222-2222-222222222222";
    let entities = [
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
        format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
    ]
    .join("\n");
    let edge_id = "33333333-3333-3333-3333-333333333333";
    let edges = format!(
        r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":1.5,"properties":{{}}}}"#
    );

    assert_sync_rejected_before_db_write(repo, &db_path, &entities, &edges, "out of range").await;
}

#[tokio::test]
async fn sync_rejects_duplicate_entity_ids_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id = "11111111-1111-1111-1111-111111111111";
    let entities = [
        format!(r#"{{"id":"{id}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
        format!(r#"{{"id":"{id}","kind":"concept","name":"A2","properties":{{}},"tags":[]}}"#),
    ]
    .join("\n");

    assert_sync_rejected_before_db_write(repo, &db_path, &entities, "", "duplicate entity id")
        .await;
}

#[tokio::test]
async fn sync_rejects_duplicate_edge_ids_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id_a = "11111111-1111-1111-1111-111111111111";
    let id_b = "22222222-2222-2222-2222-222222222222";
    let id_c = "33333333-3333-3333-3333-333333333333";
    let entities = [
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
        format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
        format!(r#"{{"id":"{id_c}","kind":"concept","name":"C","properties":{{}},"tags":[]}}"#),
    ]
    .join("\n");
    let edge_id = "44444444-4444-4444-4444-444444444444";
    let edges = [
            format!(r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.5,"properties":{{}}}}"#),
            format!(r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_c}","relation":"extends","weight":0.5,"properties":{{}}}}"#),
        ]
        .join("\n");

    assert_sync_rejected_before_db_write(repo, &db_path, &entities, &edges, "duplicate edge id")
        .await;
}

#[tokio::test]
async fn sync_rejects_duplicate_edge_triples_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id_a = "11111111-1111-1111-1111-111111111111";
    let id_b = "22222222-2222-2222-2222-222222222222";
    let entities = [
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
        format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
    ]
    .join("\n");
    let edge_id_1 = "33333333-3333-3333-3333-333333333333";
    let edge_id_2 = "44444444-4444-4444-4444-444444444444";
    let edges = [
            format!(r#"{{"edge_id":"{edge_id_1}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.5,"properties":{{}}}}"#),
            format!(r#"{{"edge_id":"{edge_id_2}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.9,"properties":{{}}}}"#),
        ]
        .join("\n");

    assert_sync_rejected_before_db_write(
        repo,
        &db_path,
        &entities,
        &edges,
        "duplicate edge triple",
    )
    .await;
}

/// Both lateral relations are symmetric in storage. A reversed pair is
/// therefore the same semantic triple even when edge IDs differ.
#[tokio::test]
async fn sync_rejects_reversed_symmetric_edge_triples_before_db_write() {
    let id_a = "11111111-1111-1111-1111-111111111111";
    let id_b = "22222222-2222-2222-2222-222222222222";
    let entities = [
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
        format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
    ]
    .join("\n");
    let edge_id_1 = "33333333-3333-3333-3333-333333333333";
    let edge_id_2 = "44444444-4444-4444-4444-444444444444";
    for relation in ["competes_with", "composed_with"] {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let edges = [
                format!(r#"{{"edge_id":"{edge_id_1}","source":"{id_a}","target":"{id_b}","relation":"{relation}","weight":0.5,"properties":{{}}}}"#),
                format!(r#"{{"edge_id":"{edge_id_2}","source":"{id_b}","target":"{id_a}","relation":"{relation}","weight":0.9,"properties":{{}}}}"#),
            ]
            .join("\n");
        assert_sync_rejected_before_db_write(
            repo,
            &db_path,
            &entities,
            &edges,
            "duplicate edge triple",
        )
        .await;
    }
}

#[tokio::test]
async fn sync_rejects_dangling_edge_source_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id_b = "22222222-2222-2222-2222-222222222222";
    let missing_source = "99999999-9999-9999-9999-999999999999";
    let entities =
        format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#);
    let edge_id = "33333333-3333-3333-3333-333333333333";
    let edges = format!(
        r#"{{"edge_id":"{edge_id}","source":"{missing_source}","target":"{id_b}","relation":"extends","weight":0.5,"properties":{{}}}}"#
    );

    assert_sync_rejected_before_db_write(repo, &db_path, &entities, &edges, "dangling source")
        .await;
}

#[tokio::test]
async fn sync_rejects_dangling_edge_target_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id_a = "11111111-1111-1111-1111-111111111111";
    let missing_target = "99999999-9999-9999-9999-999999999999";
    let entities =
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#);
    let edge_id = "33333333-3333-3333-3333-333333333333";
    let edges = format!(
        r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{missing_target}","relation":"extends","weight":0.5,"properties":{{}}}}"#
    );

    assert_sync_rejected_before_db_write(repo, &db_path, &entities, &edges, "dangling target")
        .await;
}

#[tokio::test]
async fn sync_rejects_invalid_entity_timestamp_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id = "11111111-1111-1111-1111-111111111111";
    let entities = format!(
        r#"{{"id":"{id}","kind":"concept","name":"A","properties":{{}},"tags":[],"created_at":"not-a-timestamp"}}"#
    );

    assert_sync_rejected_before_db_write(repo, &db_path, &entities, "", "invalid created_at").await;
}

#[tokio::test]
async fn sync_rejects_invalid_edge_timestamp_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id_a = "11111111-1111-1111-1111-111111111111";
    let id_b = "22222222-2222-2222-2222-222222222222";
    let entities = [
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
        format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
    ]
    .join("\n");
    let edge_id = "33333333-3333-3333-3333-333333333333";
    let edges = format!(
        r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.5,"properties":{{}},"updated_at":"not-a-timestamp"}}"#
    );

    assert_sync_rejected_before_db_write(repo, &db_path, &entities, &edges, "invalid updated_at")
        .await;
}

#[tokio::test]
async fn sync_preserves_distinct_edge_timestamps() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id_a = "11111111-1111-1111-1111-111111111111";
    let id_b = "22222222-2222-2222-2222-222222222222";
    let edge_id = "33333333-3333-3333-3333-333333333333";
    let entities = [
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
        format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
    ]
    .join("\n");
    let edges = format!(
        r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.5,"properties":{{"confidence":0.95}},"created_at":"2026-03-03T00:00:00Z","updated_at":"2026-04-04T00:00:00Z"}}"#
    );
    write_repo(repo, &entities, &edges);

    run_sync(repo, &db_path, "test-ns").await.unwrap();

    let ns = khive_types::Namespace::parse("test-ns").unwrap();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(db_path),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let token = runtime.authorize(ns).unwrap();
    let edge = runtime
        .get_edge(&token, edge_id.parse().unwrap())
        .await
        .unwrap()
        .expect("synced edge must exist");
    assert_eq!(edge.created_at.to_rfc3339(), "2026-03-03T00:00:00+00:00");
    assert_eq!(edge.updated_at.to_rfc3339(), "2026-04-04T00:00:00+00:00");
    assert_eq!(
        edge.metadata,
        Some(serde_json::json!({"confidence": 0.95})),
        "parsed edge properties must persist as storage metadata"
    );
}

#[tokio::test]
async fn sync_rejects_unsorted_entities_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    // "b..." sorts after "a...", so writing it first violates the
    // ascending-by-lowercase-id order that `write_sorted_entities` enforces.
    let id_hi = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    let id_lo = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let entities = [
        format!(r#"{{"id":"{id_hi}","kind":"concept","name":"Hi","properties":{{}},"tags":[]}}"#),
        format!(r#"{{"id":"{id_lo}","kind":"concept","name":"Lo","properties":{{}},"tags":[]}}"#),
    ]
    .join("\n");

    assert_sync_rejected_before_db_write(
        repo,
        &db_path,
        &entities,
        "",
        "entities.ndjson is not sorted",
    )
    .await;
}

#[tokio::test]
async fn sync_rejects_unsorted_edges_before_db_write() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");
    let id_a = "11111111-1111-1111-1111-111111111111";
    let id_b = "22222222-2222-2222-2222-222222222222";
    let id_c = "33333333-3333-3333-3333-333333333333";
    let entities = [
        format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
        format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
        format!(r#"{{"id":"{id_c}","kind":"concept","name":"C","properties":{{}},"tags":[]}}"#),
    ]
    .join("\n");
    let edge_id_1 = "44444444-4444-4444-4444-444444444444";
    let edge_id_2 = "55555555-5555-5555-5555-555555555555";
    // (source=c, target=a) sorts after (source=a, target=b) lexicographically
    // by UUID string, so writing it first violates edge sort order.
    let edges = [
            format!(r#"{{"edge_id":"{edge_id_1}","source":"{id_c}","target":"{id_a}","relation":"extends","weight":0.5,"properties":{{}}}}"#),
            format!(r#"{{"edge_id":"{edge_id_2}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.5,"properties":{{}}}}"#),
        ]
        .join("\n");

    assert_sync_rejected_before_db_write(
        repo,
        &db_path,
        &entities,
        &edges,
        "edges.ndjson is not sorted",
    )
    .await;
}

/// #473: `run_sync` must preserve `entity_type` from NDJSON into the
/// SQLite-backed entity store so subtype-filtered reads see it.
#[tokio::test]
async fn sync_preserves_entity_type() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");

    let id_a = "44444444-4444-4444-4444-444444444444";
    let line_a = format!(
        r#"{{"id":"{id_a}","kind":"document","entity_type":"paper","name":"Attention Is All You Need","properties":{{}},"tags":[]}}"#
    );
    write_repo(repo, &line_a, "");

    let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
    assert_eq!(report.entities, 1);

    let ns = khive_types::Namespace::parse("test-ns").unwrap();
    let config = RuntimeConfig {
        db_path: Some(db_path.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..RuntimeConfig::default()
    };
    let rt = KhiveRuntime::new(config).unwrap();
    let token = rt.authorize(ns).unwrap();
    let entity = rt
        .entities(&token)
        .unwrap()
        .get_entity(id_a.parse().unwrap())
        .await
        .unwrap()
        .expect("entity must be retrievable after sync");
    assert_eq!(
        entity.entity_type.as_deref(),
        Some("paper"),
        "entity_type must survive NDJSON sync, not be stored as NULL"
    );
}

/// #473: two archives differing ONLY by `entity_type` must hash to
/// different `SnapshotId`s — the pin must be injective over entity_type,
/// not collide with the untyped archive.
#[test]
fn remote_hash_includes_ndjson_entity_type() {
    let id_a = "55555555-5555-5555-5555-555555555555";
    let untyped = format!(
        r#"{{"id":"{id_a}","kind":"document","name":"Some Doc","properties":{{}},"tags":[]}}"#
    );
    let typed = format!(
        r#"{{"id":"{id_a}","kind":"document","entity_type":"paper","name":"Some Doc","properties":{{}},"tags":[]}}"#
    );

    let untyped_pin = compute_pin(&untyped, "", "test-ns");
    let typed_pin = compute_pin(&typed, "", "test-ns");

    assert_ne!(
        untyped_pin.as_str(),
        typed_pin.as_str(),
        "entity_type must be part of the canonical hash input; a pin for the \
             untyped archive must not validate typed content"
    );
}

/// F195: verify that FTS5 is populated during sync so text search works
/// after sync without a separate `kkernel reindex` pass.
#[tokio::test]
async fn sync_populates_fts_for_text_search() {
    use khive_runtime::RuntimeConfig;
    use khive_storage::types::{TextFilter, TextQueryMode, TextSearchRequest};

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let db_path = repo.join(".khive/state/working.db");

    let id_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let line_a = format!(
        r#"{{"id":"{id_a}","kind":"concept","name":"FlashAttention","description":"Fast attention algorithm","properties":{{}},"tags":[]}}"#
    );
    write_repo(repo, &line_a, "");

    run_sync(repo, &db_path, "test-ns").await.unwrap();

    let ns = khive_types::Namespace::parse("test-ns").unwrap();
    let config = RuntimeConfig {
        db_path: Some(db_path.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..RuntimeConfig::default()
    };
    let rt = KhiveRuntime::new(config).unwrap();
    let token = rt.authorize(ns).unwrap();

    let hits = rt
        .text(&token)
        .expect("text store must be available")
        .search(TextSearchRequest {
            query: "FlashAttention".to_string(),
            filter: Some(TextFilter {
                namespaces: vec!["test-ns".to_string()],
                ..Default::default()
            }),
            mode: TextQueryMode::Phrase,
            top_k: 10,
            snippet_chars: 128,
        })
        .await
        .expect("text search must succeed after sync");

    assert!(
        !hits.is_empty(),
        "FTS search for 'FlashAttention' must return results after sync (F195)"
    );
    assert_eq!(
        hits[0].subject_id.to_string(),
        id_a,
        "FTS hit must reference the synced entity UUID"
    );
}

// ── #474 RemoteName tests (VCS-AUD-002) ─────────────────────────────────────

/// #474: `RemoteName::parse` must reject every path-traversal / absolute
/// / separator-containing shape before it can reach `run_sync_remote`.
#[test]
fn remote_name_rejects_path_traversal_cases() {
    for bad in [
        "",
        ".",
        "..",
        "../evil",
        "../../outside",
        "/tmp/evil",
        "safe/name",
        "safe\\name",
        "safe/../evil",
    ] {
        assert!(
            RemoteName::parse(bad).is_err(),
            "expected RemoteName::parse to reject {bad:?}"
        );
    }
}

/// #474: safe single-segment names must be accepted and preserved verbatim.
#[test]
fn remote_name_accepts_single_safe_segment() {
    for good in ["upstream", "team.data-1", "remote_2"] {
        let parsed = RemoteName::parse(good).expect("expected safe name to be accepted");
        assert_eq!(parsed.as_str(), good);
    }
}

/// Issue #474: path-traversal remote names must be unconstructable. See
/// `docs/api/sync.md#remotename--construction-time-path-traversal-safety`.
#[tokio::test]
async fn run_sync_remote_cannot_be_constructed_with_invalid_name() {
    let repo_dir = TempDir::new().unwrap();
    let outside_target = repo_dir.path().parent().unwrap().join("evil-outside-probe");
    let _ = std::fs::remove_file(&outside_target);

    for bad in ["../evil", "/tmp/evil", "safe/name"] {
        assert!(
            RemoteName::parse(bad).is_err(),
            "RemoteName::parse must reject {bad:?} before any RemoteConfig can be built"
        );
    }

    // No RemoteConfig could be built from these names, so run_sync_remote was
    // never called: the cache tree and any traversal target are both absent.
    assert!(
        !repo_dir.path().join(".khive/kg/remotes").exists(),
        "cache tree must not exist — no sync ever ran"
    );
    assert!(
        !std::path::Path::new("/tmp/evil").exists(),
        "/tmp/evil must not have been created by this test run"
    );
    assert!(
        !outside_target.exists(),
        "nothing must be written outside the repo root"
    );
}

// ── #475 atomic publish failure-injection tests (VCS-AUD-003) ───────────────

fn sample_entity(id: &str, name: &str) -> NdjsonEntity {
    NdjsonEntity {
        id: id.parse().unwrap(),
        kind: "concept".to_string(),
        entity_type: None,
        name: name.to_string(),
        description: None,
        properties: Some(serde_json::json!({})),
        tags: vec![],
        created_at: None,
        updated_at: None,
    }
}

fn sample_meta(tag: &str) -> MetaJson {
    MetaJson {
        fetched_at: "2026-01-01T00:00:00Z".to_string(),
        git_ref: "main".to_string(),
        commit_sha: "deadbeef".to_string(),
        content_hash: format!("sha256:{tag}"),
    }
}

/// Publish an "old" cache generation successfully, used as the baseline
/// state that a subsequent failed publish must leave untouched.
fn publish_old_generation(remotes_root: &Path, name: &str) -> PathBuf {
    let entities = vec![sample_entity(
        "11111111-1111-1111-1111-111111111111",
        "OldEntity",
    )];
    publish_remote_cache(
        remotes_root,
        name,
        &entities,
        &[],
        &sample_meta("old"),
        None,
    )
    .expect("baseline publish must succeed")
}

fn assert_cache_is_old_generation(cache_dir: &Path) {
    let entities = std::fs::read_to_string(cache_dir.join("entities.ndjson")).unwrap();
    assert!(
        entities.contains("OldEntity"),
        "cache entities must still be the old generation, got: {entities}"
    );
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache_dir.join("meta.json")).unwrap())
            .unwrap();
    assert_eq!(meta["content_hash"], "sha256:old");
}

#[test]
fn remote_cache_ignore_marker_recovers_interrupted_creation() {
    let repo = TempDir::new().unwrap();
    run_git(repo.path(), &["init", "--quiet"]);
    let remotes_root = repo.path().join(".khive/kg/remotes");
    fs::create_dir_all(&remotes_root).unwrap();
    fs::write(
        repo.path().join(".khive/.gitignore"),
        "*\n!.gitignore\n!kg/\n!kg/**\nkg/.remote-cache/\n",
    )
    .unwrap();

    let error = publish_remote_cache(
        &remotes_root,
        "upstream",
        &[],
        &[],
        &sample_meta("interrupted"),
        Some(PublishFailAt::AfterGitignoreTempCreate),
    )
    .expect_err("injected interruption must precede marker publication");
    assert!(
        error
            .to_string()
            .contains("injected failure after cache ignore temp creation"),
        "{error:#}"
    );
    assert!(!remotes_root.join(".gitignore").exists());
    let pending_names = || {
        fs::read_dir(&remotes_root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(REMOTES_GITIGNORE_PENDING_PREFIX))
            .collect::<Vec<_>>()
    };
    assert_eq!(pending_names().len(), 1, "seam must leave one orphan");
    let status = || {
        let output = Command::new("git")
            .args([
                "-c",
                "core.excludesFile=/dev/null",
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--",
                ".khive/kg/remotes",
            ])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "git status must succeed");
        String::from_utf8(output.stdout).unwrap()
    };
    assert!(
        status().contains(REMOTES_GITIGNORE_PENDING_PREFIX),
        "interrupted marker temp must be trackable before recovery"
    );

    publish_remote_cache(
        &remotes_root,
        "upstream",
        &[],
        &[],
        &sample_meta("complete"),
        None,
    )
    .expect("next publish must recover from interrupted marker creation");
    assert_eq!(
        fs::read(remotes_root.join(".gitignore")).unwrap(),
        REMOTES_GITIGNORE
    );
    assert!(
        pending_names().is_empty(),
        "completed marker sweeps the orphan"
    );
    assert!(
        !status().contains(REMOTES_GITIGNORE_PENDING_PREFIX),
        "no marker temp remains visible to git status"
    );
}

#[test]
fn remote_cache_ignore_marker_refuses_contributor_content() {
    let temp = TempDir::new().unwrap();
    let remotes_root = temp.path().join("remotes");
    fs::create_dir_all(&remotes_root).unwrap();
    let marker = remotes_root.join(".gitignore");
    fs::write(&marker, "contributor rule\n").unwrap();
    let error = publish_remote_cache(
        &remotes_root,
        "upstream",
        &[],
        &[],
        &sample_meta("candidate"),
        None,
    )
    .expect_err("a contributor marker must not be replaced");
    assert!(
        error.to_string().contains("not the tool-owned `*` rule"),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(&marker).unwrap(), "contributor rule\n");
    assert!(!remotes_root.join("upstream").exists());

    fs::remove_file(&marker).unwrap();
    fs::create_dir(&marker).unwrap();
    let error = publish_remote_cache(
        &remotes_root,
        "upstream",
        &[],
        &[],
        &sample_meta("candidate"),
        None,
    )
    .expect_err("a non-file marker must not be replaced");
    assert!(
        error.to_string().contains("not the tool-owned `*` rule"),
        "{error:#}"
    );
    assert!(marker.is_dir());
    assert!(!remotes_root.join("upstream").exists());
}

#[test]
fn remote_cache_publish_failure_after_entities_keeps_old_cache() {
    let tmp = TempDir::new().unwrap();
    let remotes_root = tmp.path().join(".khive/kg/remotes");
    let cache_dir = publish_old_generation(&remotes_root, "upstream");

    let new_entities = vec![sample_entity(
        "22222222-2222-2222-2222-222222222222",
        "NewEntity",
    )];
    let err = publish_remote_cache(
        &remotes_root,
        "upstream",
        &new_entities,
        &[],
        &sample_meta("new"),
        Some(PublishFailAt::AfterEntities),
    )
    .expect_err("injected failure must surface as an error");
    assert!(err.to_string().contains("injected failure"));

    assert_cache_is_old_generation(&cache_dir);
}

#[test]
fn remote_cache_publish_failure_after_edges_keeps_old_cache() {
    let tmp = TempDir::new().unwrap();
    let remotes_root = tmp.path().join(".khive/kg/remotes");
    let cache_dir = publish_old_generation(&remotes_root, "upstream");

    let new_entities = vec![sample_entity(
        "22222222-2222-2222-2222-222222222222",
        "NewEntity",
    )];
    let err = publish_remote_cache(
        &remotes_root,
        "upstream",
        &new_entities,
        &[],
        &sample_meta("new"),
        Some(PublishFailAt::AfterEdges),
    )
    .expect_err("injected failure must surface as an error");
    assert!(err.to_string().contains("injected failure"));

    assert_cache_is_old_generation(&cache_dir);
}

#[test]
fn remote_cache_publish_failure_after_meta_keeps_old_cache() {
    let tmp = TempDir::new().unwrap();
    let remotes_root = tmp.path().join(".khive/kg/remotes");
    let cache_dir = publish_old_generation(&remotes_root, "upstream");

    let new_entities = vec![sample_entity(
        "22222222-2222-2222-2222-222222222222",
        "NewEntity",
    )];
    let err = publish_remote_cache(
        &remotes_root,
        "upstream",
        &new_entities,
        &[],
        &sample_meta("new"),
        Some(PublishFailAt::AfterMeta),
    )
    .expect_err("injected failure must surface as an error");
    assert!(err.to_string().contains("injected failure"));

    assert_cache_is_old_generation(&cache_dir);
}

/// Failure injected immediately before the directory swap: the staged
/// directory is fully built but never made visible, so the reader-visible
/// cache must still be exactly the old generation.
#[test]
fn remote_cache_publish_failure_before_swap_keeps_old_cache() {
    let tmp = TempDir::new().unwrap();
    let remotes_root = tmp.path().join(".khive/kg/remotes");
    let cache_dir = publish_old_generation(&remotes_root, "upstream");

    let new_entities = vec![sample_entity(
        "22222222-2222-2222-2222-222222222222",
        "NewEntity",
    )];
    let err = publish_remote_cache(
        &remotes_root,
        "upstream",
        &new_entities,
        &[],
        &sample_meta("new"),
        Some(PublishFailAt::BeforeSwap),
    )
    .expect_err("injected failure must surface as an error");
    assert!(err.to_string().contains("injected failure"));

    assert_cache_is_old_generation(&cache_dir);
}

/// A successful publish exposes the complete new entities+edges+meta
/// together — never a partial mix with the previous generation.
#[test]
fn remote_cache_publish_success_exposes_complete_new_cache() {
    let tmp = TempDir::new().unwrap();
    let remotes_root = tmp.path().join(".khive/kg/remotes");
    let cache_dir = publish_old_generation(&remotes_root, "upstream");
    assert_cache_is_old_generation(&cache_dir);

    let new_entities = vec![sample_entity(
        "22222222-2222-2222-2222-222222222222",
        "NewEntity",
    )];
    let published = publish_remote_cache(
        &remotes_root,
        "upstream",
        &new_entities,
        &[],
        &sample_meta("new"),
        None,
    )
    .expect("publish must succeed");
    assert_eq!(published, cache_dir);

    let entities = std::fs::read_to_string(cache_dir.join("entities.ndjson")).unwrap();
    assert!(entities.contains("NewEntity"));
    assert!(
        !entities.contains("OldEntity"),
        "old generation entity must not linger after a successful publish"
    );
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache_dir.join("meta.json")).unwrap())
            .unwrap();
    assert_eq!(meta["content_hash"], "sha256:new");
}

#[test]
fn remote_cache_publish_recovers_crash_backup_before_replacement() {
    let tmp = TempDir::new().unwrap();
    let remotes_root = tmp.path().join("remotes");
    let cache_dir = publish_old_generation(&remotes_root, "upstream");
    let backup = remotes_root.join("upstream.replaced~99999");
    mark_cache_for_backup(&cache_dir, &backup).unwrap();
    std::fs::rename(&cache_dir, &backup).unwrap();
    assert!(is_owned_backup(&backup).unwrap());

    let entities = vec![sample_entity(
        "22222222-2222-2222-2222-222222222222",
        "NewEntity",
    )];
    let error = publish_remote_cache(
        &remotes_root,
        "upstream",
        &entities,
        &[],
        &sample_meta("new"),
        Some(PublishFailAt::BeforeSwap),
    )
    .expect_err("staging failure must follow backup restoration");
    assert!(error.to_string().contains("injected failure before swap"));
    assert_cache_is_old_generation(&cache_dir);
    assert!(!cache_dir.join(REMOTE_BACKUP_OWNER_FILE).exists());
    assert!(!backup.exists());

    publish_remote_cache(
        &remotes_root,
        "upstream",
        &entities,
        &[],
        &sample_meta("new"),
        None,
    )
    .unwrap();

    assert!(cache_dir.join("entities.ndjson").exists());
    assert!(!backup.exists());
    assert_eq!(
        std::fs::read_dir(&remotes_root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("upstream.replaced~"))
            .count(),
        0
    );
}

#[test]
fn remote_cache_publish_reports_failed_rollback_and_retained_backup() {
    let tmp = TempDir::new().unwrap();
    let remotes_root = tmp.path().join("remotes");
    let cache_dir = publish_old_generation(&remotes_root, "upstream");
    let staged = tempfile::TempDir::new_in(&remotes_root).unwrap();
    let mut renames = 0;
    let error = atomic_replace_dir_with(staged.path(), &cache_dir, |from, to| {
        renames += 1;
        if renames >= 2 {
            return Err(std::io::Error::other("injected rename failure"));
        }
        std::fs::rename(from, to)
    })
    .unwrap_err();
    let backup = remotes_root.join(format!("upstream.replaced~{}", std::process::id()));
    assert!(!cache_dir.exists());
    assert_cache_is_old_generation(&backup);
    assert!(is_owned_backup(&backup).unwrap());
    let message = error.to_string();
    assert!(message.contains("restoring old cache failed"), "{message}");
    assert!(message.contains(&backup.display().to_string()), "{message}");
    assert!(!message.contains("old cache restored"), "{message}");
}

// ── F201 tests ────────────────────────────────────────────────────────────

/// F201-1: `run_sync_remote` with a correct pin succeeds and writes the
/// expected cache files and `meta.json`.
#[tokio::test]
async fn run_sync_remote_fetches_and_verifies_hash_match() {
    let remote_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();

    let id_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let entities = format!(
        r#"{{"id":"{id_a}","kind":"concept","name":"RemoteEntity","properties":{{}},"tags":[]}}"#
    );
    let edges = "";

    let remote_url = make_git_remote(remote_dir.path(), &entities, edges);
    let expected_pin = compute_pin(&entities, edges, "remote-ns");

    let remote = RemoteConfig {
        name: RemoteName::parse("upstream").unwrap(),
        url: remote_url,
        git_ref: "main".to_string(),
        namespace: "remote-ns".to_string(),
        pin: Some(expected_pin.clone()),
    };

    let report = run_sync_remote(repo_dir.path(), &remote, false)
        .await
        .expect("run_sync_remote must succeed with correct pin");

    assert_eq!(report.entities, 1, "must report 1 entity");
    assert_eq!(report.edges, 0, "must report 0 edges");
    assert_eq!(
        report.content_hash,
        expected_pin.as_str(),
        "content_hash must match the pin"
    );
    assert!(!report.repinned, "repin was not requested");

    // Cache files must exist.
    let cache = repo_dir.path().join(".khive/kg/remotes/upstream");
    assert!(
        cache.join("entities.ndjson").exists(),
        "entities.ndjson must exist in cache"
    );
    assert!(
        cache.join("edges.ndjson").exists(),
        "edges.ndjson must exist in cache"
    );
    assert!(
        cache.join("meta.json").exists(),
        "meta.json must exist in cache"
    );

    // meta.json must be valid JSON with the expected fields.
    let meta_bytes = std::fs::read(cache.join("meta.json")).unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&meta_bytes).unwrap();
    assert_eq!(
        meta["content_hash"].as_str().unwrap(),
        expected_pin.as_str(),
        "meta.json content_hash must match"
    );
    assert!(
        meta["fetched_at"].as_str().is_some(),
        "meta.json must have fetched_at"
    );
    assert!(
        meta["commit_sha"].as_str().is_some(),
        "meta.json must have commit_sha"
    );
}

#[tokio::test]
async fn run_sync_remote_rejects_blank_name_before_cache_publish() {
    let remote_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();
    let entity_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let entities = format!(
        r#"{{"id":"{entity_id}","kind":"concept","name":"   ","properties":{{}},"tags":[]}}"#
    );
    let remote_url = make_git_remote(remote_dir.path(), &entities, "");
    let remote = RemoteConfig {
        name: RemoteName::parse("blank-name").unwrap(),
        url: remote_url,
        git_ref: "main".to_string(),
        namespace: "remote-ns".to_string(),
        pin: None,
    };

    let err = run_sync_remote(repo_dir.path(), &remote, false)
        .await
        .expect_err("remote fetch must reject a whitespace-only entity name");
    assert!(
        err.chain()
            .any(|cause| cause.to_string().contains("non-blank name")),
        "error must explain the name invariant: {err:#}"
    );
    assert!(
        !repo_dir
            .path()
            .join(".khive/kg/remotes/blank-name")
            .exists(),
        "invalid remote records must not publish a cache generation"
    );
}

#[tokio::test]
async fn run_sync_remote_rejects_malformed_timestamp_before_cache_publish() {
    let remote_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();
    let entity_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let entities = format!(
        r#"{{"id":"{entity_id}","kind":"concept","name":"Valid","properties":{{}},"tags":[],"updated_at":"not-rfc3339"}}"#
    );
    let remote_url = make_git_remote(remote_dir.path(), &entities, "");
    let remote = RemoteConfig {
        name: RemoteName::parse("bad-timestamp").unwrap(),
        url: remote_url,
        git_ref: "main".to_string(),
        namespace: "remote-ns".to_string(),
        pin: None,
    };

    let err = run_sync_remote(repo_dir.path(), &remote, false)
        .await
        .expect_err("remote fetch must reject a present malformed timestamp");
    assert!(
        err.chain()
            .any(|cause| cause.to_string().contains("invalid updated_at")),
        "error must identify the malformed timestamp: {err:#}"
    );
    assert!(
        !repo_dir
            .path()
            .join(".khive/kg/remotes/bad-timestamp")
            .exists(),
        "invalid remote records must not publish a cache generation"
    );
}

/// F201-2: `run_sync_remote` with a wrong pin fails before touching the
/// cache (fail-closed guarantee).
/// The remote publication path must invoke the blocking secret scanner:
/// a credential-shaped edge property fails the sync before
/// `publish_remote_cache` writes any reader-visible cache generation.
#[tokio::test]
async fn run_sync_remote_rejects_credential_shaped_edge_property_before_cache_publish() {
    let remote_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();
    let id_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let id_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    let entities = format!(
            "{{\"id\":\"{id_a}\",\"kind\":\"concept\",\"name\":\"A\",\"properties\":{{}},\"tags\":[]}}\n{{\"id\":\"{id_b}\",\"kind\":\"concept\",\"name\":\"B\",\"properties\":{{}},\"tags\":[]}}"
        );
    let edge_id = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    let edges = format!(
            "{{\"edge_id\":\"{edge_id}\",\"source\":\"{id_a}\",\"target\":\"{id_b}\",\"relation\":\"extends\",\"weight\":0.8,\"properties\":{{\"api_key\":\"AKIAFAKEKEY1234567890\"}}}}"
        );
    let remote_url = make_git_remote(remote_dir.path(), &entities, &edges);
    let remote = RemoteConfig {
        name: RemoteName::parse("secret-edge").unwrap(),
        url: remote_url,
        git_ref: "main".to_string(),
        namespace: "remote-ns".to_string(),
        pin: None,
    };

    let err = run_sync_remote(repo_dir.path(), &remote, false)
        .await
        .expect_err("remote fetch must reject the credential-shaped edge property");
    assert!(
        err.chain()
            .any(|cause| cause.to_string().contains("properties rejected")),
        "error must attribute the rejection to edge properties: {err:#}"
    );
    assert!(
        !repo_dir
            .path()
            .join(".khive/kg/remotes/secret-edge")
            .exists(),
        "credential-shaped remote records must not publish a cache generation"
    );
}

#[tokio::test]
async fn run_sync_remote_rejects_hash_mismatch() {
    let remote_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();

    let id_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    let entities = format!(
        r#"{{"id":"{id_b}","kind":"concept","name":"AnotherEntity","properties":{{}},"tags":[]}}"#
    );
    let edges = "";

    let remote_url = make_git_remote(remote_dir.path(), &entities, edges);

    // Deliberate wrong pin: 64 zero hex chars.
    let wrong_pin = SnapshotId::from_hash(&"0".repeat(64)).unwrap();

    let remote = RemoteConfig {
        name: RemoteName::parse("upstream").unwrap(),
        url: remote_url,
        git_ref: "main".to_string(),
        namespace: "remote-ns".to_string(),
        pin: Some(wrong_pin.clone()),
    };

    let err = run_sync_remote(repo_dir.path(), &remote, false)
        .await
        .expect_err("run_sync_remote must fail on hash mismatch");

    let err_msg = err.to_string();
    assert!(
        err_msg.contains("hash mismatch") || err_msg.contains("sha256:"),
        "error must mention hash mismatch, got: {err_msg}"
    );

    // Cache must NOT have been written (fail-closed).
    let cache = repo_dir.path().join(".khive/kg/remotes/upstream");
    assert!(
        !cache.join("entities.ndjson").exists(),
        "entities.ndjson must NOT exist after mismatch"
    );
    assert!(
        !cache.join("meta.json").exists(),
        "meta.json must NOT exist after mismatch"
    );
}

/// F201-3: `run_sync_remote` with no pin still proceeds and writes `meta.json`
/// (hash is still computed and written for auditability).
#[tokio::test]
async fn run_sync_remote_no_pin_proceeds_and_writes_meta() {
    let remote_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();

    let id_c = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    let entities = format!(
        r#"{{"id":"{id_c}","kind":"concept","name":"Pinless","properties":{{}},"tags":[]}}"#
    );

    let remote_url = make_git_remote(remote_dir.path(), &entities, "");

    let remote = RemoteConfig {
        name: RemoteName::parse("no-pin-remote").unwrap(),
        url: remote_url,
        git_ref: "main".to_string(),
        namespace: "remote-ns".to_string(),
        pin: None,
    };

    let report = run_sync_remote(repo_dir.path(), &remote, false)
        .await
        .expect("run_sync_remote must succeed with no pin");

    assert_eq!(report.entities, 1);
    assert!(
        report.content_hash.starts_with("sha256:"),
        "content_hash must have sha256: prefix even without pin"
    );

    let cache = repo_dir.path().join(".khive/kg/remotes/no-pin-remote");
    assert!(
        cache.join("meta.json").exists(),
        "meta.json must be written even when pin is absent"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn remote_fetch_rejects_symlinked_ndjson_members_before_publish() {
    use std::os::unix::fs::symlink;

    let entities = r#"{"id":"cccccccc-cccc-cccc-cccc-cccccccccccc","kind":"concept","name":"Outside","properties":{},"tags":[]}"#;
    for member in ["entities.ndjson", "edges.ndjson"] {
        let source = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let bare_parent = TempDir::new().unwrap();
        let client = TempDir::new().unwrap();
        make_git_remote(source.path(), entities, "");

        let outside_file = outside.path().join(member);
        std::fs::write(
            &outside_file,
            if member == "entities.ndjson" {
                entities
            } else {
                ""
            },
        )
        .unwrap();
        let member_path = source.path().join(".khive/kg").join(member);
        std::fs::remove_file(&member_path).unwrap();
        symlink(&outside_file, &member_path).unwrap();
        run_git(source.path(), &["add", "-A"]);
        run_git(source.path(), &["commit", "-m", "link remote member"]);

        let bare = bare_parent.path().join("remote.git");
        run_git(
            source.path(),
            &["clone", "--bare", ".", bare.to_str().unwrap()],
        );
        let remote = RemoteConfig {
            name: RemoteName::parse("outside").unwrap(),
            url: format!("file://{}", bare.display()),
            git_ref: "main".to_string(),
            namespace: "remote-ns".to_string(),
            pin: None,
        };

        let err = run_sync_remote(client.path(), &remote, false)
            .await
            .expect_err("a checked-out symlink must not be read");
        assert!(
            format!("{err:#}").contains("not a regular file"),
            "{member}: {err:#}"
        );
        assert!(
            !client.path().join(".khive/kg/remotes/outside").exists(),
            "{member} must not publish an external file"
        );
    }
}

#[cfg(unix)]
#[test]
fn remote_reader_rejects_symlinked_ancestor() {
    use std::os::unix::fs::symlink;

    let entity =
        r#"{"id":"cccccccc-cccc-cccc-cccc-cccccccccccc","kind":"concept","name":"Outside"}"#;
    for ancestor in [".khive", ".khive/kg"] {
        let staging = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_kg = if ancestor == ".khive" {
            outside.path().join("kg")
        } else {
            outside.path().to_path_buf()
        };
        std::fs::create_dir_all(&outside_kg).unwrap();
        std::fs::write(outside_kg.join("entities.ndjson"), entity).unwrap();
        if ancestor == ".khive/kg" {
            std::fs::create_dir(staging.path().join(".khive")).unwrap();
        }
        let link = staging.path().join(ancestor);
        symlink(outside.path(), &link).unwrap();

        let member = staging.path().join(".khive/kg/entities.ndjson");
        let err = read_remote_entities(&member).expect_err("a symlinked ancestor must be refused");
        assert!(
            format!("{err:#}").contains("not a regular directory"),
            "{ancestor}: {err:#}"
        );

        std::fs::remove_file(&link).unwrap();
        std::fs::create_dir_all(staging.path().join(".khive/kg")).unwrap();
        std::fs::write(&member, entity).unwrap();
        assert_eq!(read_remote_entities(&member).unwrap().len(), 1);
    }
}

/// F201-4: `--repin` skips pin comparison and returns the actual hash,
/// allowing the caller to update `schema.yaml`.
#[tokio::test]
async fn run_sync_remote_repin_updates_hash_ignoring_old_pin() {
    let remote_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();

    let id_d = "dddddddd-dddd-dddd-dddd-dddddddddddd";
    let entities = format!(
        r#"{{"id":"{id_d}","kind":"concept","name":"RepinTarget","properties":{{}},"tags":[]}}"#
    );

    let remote_url = make_git_remote(remote_dir.path(), &entities, "");
    let actual_hash = compute_pin(&entities, "", "repin-ns");

    // Deliberately stale/wrong pin — repin must ignore it.
    let stale_pin = SnapshotId::from_hash(&"f".repeat(64)).unwrap();

    let remote = RemoteConfig {
        name: RemoteName::parse("repinned").unwrap(),
        url: remote_url,
        git_ref: "main".to_string(),
        namespace: "repin-ns".to_string(),
        pin: Some(stale_pin),
    };

    let report = run_sync_remote(repo_dir.path(), &remote, true)
        .await
        .expect("repin must succeed even with wrong existing pin");

    assert!(report.repinned, "repinned flag must be true");
    assert_eq!(
        report.content_hash,
        actual_hash.as_str(),
        "repinned hash must be the actual fetched archive hash"
    );

    // Cache must be populated.
    let cache = repo_dir.path().join(".khive/kg/remotes/repinned");
    assert!(cache.join("entities.ndjson").exists());
    assert!(cache.join("meta.json").exists());
}

// ── URL redaction tests ───────────────────────────────────────────────────

/// Credentials embedded in a URL must not survive redact_git_stderr.
#[test]
fn redact_strips_credential_url() {
    let raw = "fatal: Authentication failed for 'https://user:token@host/repo.git'";
    let out = redact_git_stderr(raw);
    assert!(
        !out.contains("user:token"),
        "credential must be redacted, got: {out}"
    );
    assert!(
        !out.contains("host/repo.git"),
        "host/path must be redacted, got: {out}"
    );
    assert!(
        out.contains("<url-redacted>"),
        "placeholder must be present, got: {out}"
    );
}

/// Plain text without a URL must pass through unchanged.
#[test]
fn redact_passes_plain_text() {
    let raw = "error: unable to read refs from remote";
    assert_eq!(redact_git_stderr(raw), raw);
}

/// Multiple URLs in the same stderr string must all be redacted.
#[test]
fn redact_handles_multiple_urls() {
    let raw = "fetch https://a:b@host1/r1.git and push https://c:d@host2/r2.git failed";
    let out = redact_git_stderr(raw);
    assert!(!out.contains("a:b"), "first credential must be redacted");
    assert!(!out.contains("c:d"), "second credential must be redacted");
    assert_eq!(
        out.matches("<url-redacted>").count(),
        2,
        "both URLs must be replaced"
    );
}

/// A bare URL without credentials is also redacted (the host is still sensitive).
#[test]
fn redact_handles_url_without_credentials() {
    let raw = "fatal: repository 'https://github.com/org/private-repo.git/' not found";
    let out = redact_git_stderr(raw);
    assert!(
        !out.contains("github.com/org/private-repo"),
        "URL path must be redacted"
    );
    assert!(
        out.contains("<url-redacted>"),
        "placeholder must be present"
    );
}

/// scp-style `git@host:org/repo.git` must be fully redacted.
#[test]
fn redact_strips_scp_style_remote() {
    let raw = "ERROR: Repository not found.\nfatal: Could not read from remote repository git@github.com:org/private-repo.git";
    let out = redact_git_stderr(raw);
    assert!(
        !out.contains("git@"),
        "scp userinfo must be redacted, got: {out}"
    );
    assert!(
        !out.contains("github.com"),
        "scp host must be redacted, got: {out}"
    );
    assert!(
        !out.contains("private-repo"),
        "scp path must be redacted, got: {out}"
    );
    assert!(
        out.contains("<url-redacted>"),
        "placeholder must be present, got: {out}"
    );
}

/// `user@host:path` (non-git@ prefix) must also be redacted.
#[test]
fn redact_strips_user_at_host_colon_path() {
    let raw = "fatal: repository user@bitbucket.org:myteam/myrepo.git not found";
    let out = redact_git_stderr(raw);
    assert!(
        !out.contains("user@"),
        "userinfo must be redacted, got: {out}"
    );
    assert!(
        !out.contains("bitbucket.org"),
        "host must be redacted, got: {out}"
    );
    assert!(
        out.contains("<url-redacted>"),
        "placeholder must be present, got: {out}"
    );
}

/// Plain `host:path` without a `user@` prefix must NOT be over-redacted
/// (it is not a recognised remote form).
#[test]
fn redact_does_not_over_redact_plain_colon() {
    let raw = "error: src refspec main does not match any";
    let out = redact_git_stderr(raw);
    assert_eq!(out, raw, "plain text with colon must not be altered");
}

// ── Public error boundary tests ───────────────────────────────────────────
//
// These tests verify that the sanitiser is wired into the actual public error
// path (the `anyhow` error returned by `run_sync_remote`).  They use realistic
// git-stderr fragments — the kind git emits when a clone fails for auth or
// network reasons — and assert that the rendered error string contains no raw
// credentials or remote-URL tokens.
//
// The FAIL-before / PASS-after property is demonstrated by the
// `redact_git_stderr` unit tests above (which call the function directly)
// combined with these wiring tests that confirm the sanitised output is what
// the caller actually sees in `err.to_string()`.

/// Credential-bearing HTTPS URLs must not leak into the public error
/// string. See `docs/api/sync.md#credential-redaction-in-git-error-output--redact_git_stderr`.
#[tokio::test]
async fn public_error_redacts_https_credential_url() {
    let repo_dir = tempfile::TempDir::new().unwrap();
    // Use a credential-bearing HTTPS URL that will fail immediately.
    let remote = RemoteConfig {
        name: RemoteName::parse("cred-test").unwrap(),
        url: "https://user:secret_token@nonexistent.example.invalid/org/repo.git".to_string(),
        git_ref: "main".to_string(),
        namespace: "test-ns".to_string(),
        pin: None,
    };
    let err = run_sync_remote(repo_dir.path(), &remote, false)
        .await
        .expect_err("clone of nonexistent URL must fail");

    let err_str = err.to_string();
    let err_chain: String = err
        .chain()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(" | ");

    assert!(
        !err_str.contains("secret_token") && !err_chain.contains("secret_token"),
        "credential must not appear in public error string or chain: {err_str} | {err_chain}"
    );
    assert!(
        !err_str.contains("user:") && !err_chain.contains("user:"),
        "userinfo must not appear in public error: {err_str} | {err_chain}"
    );
    // The remote name is used in the error, but the URL must not be.
    assert!(
        err_str.contains("cred-test") || err_chain.contains("cred-test"),
        "remote name must be present for diagnostics: {err_str} | {err_chain}"
    );
}

/// scp-style `git@host:org/repo.git` must not leak through the sanitiser.
/// See `docs/api/sync.md#credential-redaction-in-git-error-output--redact_git_stderr`.
#[tokio::test]
async fn public_error_redacts_scp_style_remote() {
    let repo_dir = tempfile::TempDir::new().unwrap();
    let remote = RemoteConfig {
        name: RemoteName::parse("scp-test").unwrap(),
        url: "git@nonexistent.example.invalid:org/private-repo.git".to_string(),
        git_ref: "main".to_string(),
        namespace: "test-ns".to_string(),
        pin: None,
    };
    let err = run_sync_remote(repo_dir.path(), &remote, false)
        .await
        .expect_err("clone of nonexistent scp remote must fail");

    let err_str = err.to_string();
    let err_chain: String = err
        .chain()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(" | ");

    // The remote URL must not appear verbatim in the public error.
    // git does not echo scp URLs into stderr on SSH-level failures —
    // the host appears in SSH's own message, not from URL echoing.
    // We assert that our scp token (user@host:path form) is absent.
    assert!(
        !err_str.contains("git@nonexistent.example.invalid")
            && !err_chain.contains("git@nonexistent.example.invalid"),
        "scp remote URL must not appear in public error: {err_str} | {err_chain}"
    );
    assert!(
        !err_str.contains("private-repo") && !err_chain.contains("private-repo"),
        "scp repo path must not appear in public error: {err_str} | {err_chain}"
    );
    // Remote name must still be present for diagnostics.
    assert!(
        err_str.contains("scp-test") || err_chain.contains("scp-test"),
        "remote name must be present for diagnostics: {err_str} | {err_chain}"
    );
}

/// Regression: VCS sync FTS document must be field-identical to
/// `entity_fts_document`'s output. See
/// `docs/api/sync.md#fts-document-consistency`.
#[test]
fn sync_fts_document_matches_entity_fts_document() {
    use khive_runtime::entity_fts_document;
    use khive_storage::SubstrateKind;

    let id = uuid::Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
    let props: Option<serde_json::Value> =
        Some(serde_json::json!({"domain": "attention", "status": "researched"}));

    let entity = khive_storage::entity::Entity {
        id,
        namespace: "test-ns".to_string(),
        kind: "concept".to_string(),
        entity_type: None,
        name: "FlashAttention".to_string(),
        description: Some("Fast attention algorithm".to_string()),
        properties: props.clone(),
        tags: vec!["attention".to_string(), "inference".to_string()],
        created_at: 1_000_000,
        updated_at: 2_000_000,
        deleted_at: None,
        merge_event_id: None,
        merged_into: None,
        version: 1,
        content_ref: None,
    };

    let doc = entity_fts_document(&entity);

    assert_eq!(doc.subject_id, id);
    assert_eq!(doc.kind, SubstrateKind::Entity);
    assert_eq!(doc.namespace, "test-ns");
    assert_eq!(doc.title.as_deref(), Some("FlashAttention"));
    assert_eq!(doc.body, "FlashAttention Fast attention algorithm");
    assert_eq!(
        doc.tags,
        vec!["attention".to_string(), "inference".to_string()]
    );
    assert_eq!(doc.metadata, props);
    assert_eq!(
        doc.updated_at,
        chrono::DateTime::from_timestamp_micros(2_000_000).unwrap()
    );
}

/// `user@host:path` scp form must not appear in the public error string.
#[tokio::test]
async fn public_error_redacts_user_pass_at_host_scp() {
    let repo_dir = tempfile::TempDir::new().unwrap();
    let remote = RemoteConfig {
        name: RemoteName::parse("userpass-scp").unwrap(),
        url: "deploy@nonexistent.example.invalid:infra/secret-repo.git".to_string(),
        git_ref: "main".to_string(),
        namespace: "test-ns".to_string(),
        pin: None,
    };
    let err = run_sync_remote(repo_dir.path(), &remote, false)
        .await
        .expect_err("clone of nonexistent scp remote must fail");

    let err_str = err.to_string();
    let err_chain: String = err
        .chain()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(" | ");

    assert!(
        !err_str.contains("deploy@nonexistent.example.invalid")
            && !err_chain.contains("deploy@nonexistent.example.invalid"),
        "scp userinfo+host must not appear in public error: {err_str} | {err_chain}"
    );
    assert!(
        !err_str.contains("secret-repo") && !err_chain.contains("secret-repo"),
        "scp repo path must not appear in public error: {err_str} | {err_chain}"
    );
}
