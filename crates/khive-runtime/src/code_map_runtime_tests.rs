use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{CodeMapConfig, CodeMapRegistry};
use crate::{
    runtime_config_from_khive_config, KhiveConfig, KhiveRuntime, RuntimeConfig, RuntimeError,
};

fn entry(name: &str, path: &Path, source_root: &Path) -> CodeMapConfig {
    CodeMapConfig {
        name: Some(name.to_string()),
        path: Some(path.display().to_string()),
        source_root: Some(source_root.display().to_string()),
        ..CodeMapConfig::default()
    }
}

fn create_file(path: &Path) {
    std::fs::write(path, b"map data").unwrap();
}

fn create_queryable_map(path: &Path) {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE records (value TEXT NOT NULL); INSERT INTO records VALUES ('registered');",
        )
        .unwrap();
}

fn read_map_value(registry: &CodeMapRegistry) -> Result<String, RuntimeError> {
    registry.with_read("map", |connection| {
        connection
            .query_row("SELECT value FROM records", [], |row| row.get(0))
            .map_err(|_| RuntimeError::Internal("map query failed".to_string()))
    })
}

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Run `load` with WARN logs captured; return its value and the captured text.
fn capture_warnings<T>(load: impl FnOnce() -> T) -> (T, String) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let log_buffer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || LogWriter(log_buffer.clone()))
        .finish();
    // Keep a second dispatcher alive so concurrent tests cannot cache the
    // rejection callsite as disabled before the capture is registered.
    let _second_dispatcher = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let value = tracing::subscriber::with_default(subscriber, load);
    let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    (value, text)
}

#[test]
fn valid_entry_registers_and_resolves_by_name() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("workspace.db");
    create_file(&map);
    let source = fixture.path().join("project");

    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("workspace", &map, &source)],
        &[],
    );
    let registered = registry.resolve("workspace").unwrap();

    let canonical_map = std::fs::canonicalize(&map).unwrap();
    assert_eq!(registered.path(), canonical_map.as_path());
    assert_eq!(registered.source_root(), source.as_path());
    assert_eq!(registry.len(), 1);
    assert!(!registered.generation().to_string().is_empty());
}

#[test]
fn outside_entry_is_logged_and_does_not_block_valid_entry() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    let other = fixture.path().join("maps-other");
    std::fs::create_dir(&maps).unwrap();
    std::fs::create_dir(&other).unwrap();
    let outside = other.join("outside.db");
    let valid = maps.join("valid.db");
    create_file(&outside);
    create_file(&valid);
    let logs = Arc::new(Mutex::new(Vec::new()));
    let log_buffer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || LogWriter(log_buffer.clone()))
        .finish();
    // Keep a second dispatcher alive so concurrent tests cannot cache the
    // rejection callsite as disabled before the capture is registered.
    let _second_dispatcher = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());

    let registry = tracing::subscriber::with_default(subscriber, || {
        CodeMapRegistry::load(
            Some(maps.to_str().unwrap()),
            &[
                entry("outside", &outside, fixture.path()),
                entry("valid", &valid, fixture.path()),
            ],
            &[],
        )
    });
    let log_text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();

    assert!(registry.resolve("outside").is_err());
    assert!(registry.resolve("valid").is_ok());
    assert!(log_text.contains("outside"), "{log_text}");
    assert!(log_text.contains("outside code_map_root"), "{log_text}");
}

#[test]
fn sibling_prefix_path_is_outside_the_map_root() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    let sibling = fixture.path().join("maps-other");
    std::fs::create_dir(&maps).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    let map = sibling.join("a.db");
    create_file(&map);

    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("sibling", &map, fixture.path())],
        &[],
    );

    assert!(registry.resolve("sibling").is_err());
    assert!(registry.is_empty());
}

#[test]
fn duplicate_names_are_rejected() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let first = maps.join("first.db");
    let second = maps.join("second.db");
    create_file(&first);
    create_file(&second);

    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[
            entry("duplicate", &first, fixture.path()),
            entry("duplicate", &second, fixture.path()),
        ],
        &[],
    );

    assert!(registry.is_empty());
    assert!(registry.resolve("duplicate").is_err());
}

#[test]
fn relative_uri_missing_directory_and_symlink_paths_are_rejected() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let directory = maps.join("directory");
    std::fs::create_dir(&directory).unwrap();
    let valid = maps.join("valid.db");
    create_file(&valid);
    let source = fixture.path().join("project");
    let mut paths = vec![
        PathBuf::from("relative.db"),
        PathBuf::from("file:///tmp/map.db"),
        maps.join("missing.db"),
        directory,
    ];
    #[cfg(unix)]
    {
        let symlink = maps.join("linked.db");
        std::os::unix::fs::symlink(&valid, &symlink).unwrap();
        paths.push(symlink);
    }

    for (index, path) in paths.iter().enumerate() {
        let registry = CodeMapRegistry::load(
            Some(maps.to_str().unwrap()),
            &[entry(&format!("invalid-{index}"), path, &source)],
            &[],
        );
        assert!(
            registry.resolve(&format!("invalid-{index}")).is_err(),
            "accepted invalid path {}",
            path.display()
        );
    }
}

#[test]
fn invalid_root_rejects_every_entry() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    create_file(&map);
    let file_root = fixture.path().join("root-file");
    create_file(&file_root);
    let missing_root = fixture.path().join("missing-root");

    for root in [
        None,
        Some("relative/root"),
        Some("file:///tmp/maps"),
        Some(file_root.to_str().unwrap()),
        Some(missing_root.to_str().unwrap()),
    ] {
        let registry = CodeMapRegistry::load(root, &[entry("map", &map, fixture.path())], &[]);
        assert!(registry.is_empty(), "registered with root {root:?}");
    }
}

#[test]
fn production_database_identity_is_rejected() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let production = maps.join("production.db");
    create_file(&production);

    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("production", &production, fixture.path())],
        std::slice::from_ref(&production),
    );

    assert!(registry.is_empty());
    assert!(registry.resolve("production").is_err());
}

#[test]
fn production_companion_identity_is_rejected() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let production = maps.join("production.db");
    let map = maps.join("map.db");
    create_file(&production);
    let production_wal = PathBuf::from(format!("{}-wal", production.display()));
    create_file(&production_wal);
    std::fs::hard_link(&production_wal, &map).unwrap();

    // The alias is a second link, so the link-count check refuses it too; the
    // log shows that the production identity check fired first.
    let (registry, log_text) = capture_warnings(|| {
        CodeMapRegistry::load(
            Some(maps.to_str().unwrap()),
            &[entry("map", &map, fixture.path())],
            std::slice::from_ref(&production),
        )
    });

    assert!(registry.is_empty());
    assert!(registry.resolve("map").is_err());
    assert!(
        log_text.contains("production database or companion"),
        "{log_text}"
    );
}

#[test]
fn production_events_database_identity_is_rejected() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let production = fixture.path().join("production.db");
    create_file(&production);
    let events = crate::events_split::events_db_path_beside(&production);
    create_file(&events);
    let map = maps.join("map.db");
    std::fs::hard_link(&events, &map).unwrap();
    let config = RuntimeConfig {
        db_path: Some(production),
        code_map_root: Some(maps.to_string_lossy().into_owned()),
        code_maps: vec![entry("map", &map, fixture.path())],
        ..RuntimeConfig::no_embeddings()
    };

    // The alias is a second link, so the link-count check refuses it too; the
    // log shows that the production identity check fired first.
    let (registry, log_text) =
        capture_warnings(|| CodeMapRegistry::load_for_runtime(&config, None));

    assert!(registry.is_empty());
    assert!(registry.resolve("map").is_err());
    assert!(
        log_text.contains("production database or companion"),
        "{log_text}"
    );
}

#[test]
fn missing_production_database_does_not_block_a_valid_map() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    let production = fixture.path().join("not-created-yet.db");
    create_file(&map);

    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        std::slice::from_ref(&production),
    );

    assert!(registry.resolve("map").is_ok());
}

#[test]
fn malformed_code_map_entries_do_not_block_valid_runtime_registration() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let valid = maps.join("valid.db");
    create_file(&valid);

    let config_text = format!(
        "code_map_root = {:?}\n\
         [[code_maps]]\nname = \"valid\"\npath = {:?}\nsource_root = {:?}\n\
         [[code_maps]]\nname = \"unknown-key\"\npath = {:?}\nsource_root = {:?}\nextra = true\n\
         [[code_maps]]\nname = \"missing-path\"\nsource_root = {:?}\n",
        maps.display().to_string(),
        valid.display().to_string(),
        fixture.path().display().to_string(),
        valid.display().to_string(),
        fixture.path().display().to_string(),
        fixture.path().display().to_string(),
    );
    let parsed: KhiveConfig = toml::from_str(&config_text).expect("malformed entries parse");
    let runtime_config = runtime_config_from_khive_config(
        &parsed,
        RuntimeConfig {
            db_path: None,
            ..RuntimeConfig::no_embeddings()
        },
    );

    let logs = Arc::new(Mutex::new(Vec::new()));
    let log_buffer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || LogWriter(log_buffer.clone()))
        .finish();
    // Keep a second dispatcher alive so concurrent tests cannot cache the
    // rejection callsite as disabled before the capture is registered.
    let _second_dispatcher = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let runtime = tracing::subscriber::with_default(subscriber, || {
        KhiveRuntime::new(runtime_config).expect("runtime starts with malformed map entries")
    });
    let log_text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();

    assert_eq!(runtime.code_map_registry().len(), 1);
    assert!(runtime.code_map_registry().resolve("valid").is_ok());
    assert!(runtime.code_map_registry().resolve("unknown-key").is_err());
    assert!(runtime.code_map_registry().resolve("missing-path").is_err());
    assert!(log_text.contains("unknown-key"), "{log_text}");
    assert!(log_text.contains("unknown field extra"), "{log_text}");
    assert!(log_text.contains("missing-path"), "{log_text}");
    assert!(log_text.contains("missing path"), "{log_text}");
}

#[test]
fn absent_and_rejected_names_have_the_same_resolution_error() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let missing = maps.join("missing.db");
    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("rejected", &missing, fixture.path())],
        &[],
    );

    let unknown_error = registry.resolve("unknown").unwrap_err().to_string();
    let rejected_error = registry.resolve("rejected").unwrap_err().to_string();
    assert_eq!(unknown_error, rejected_error);
}

#[test]
fn repeated_loads_receive_distinct_registration_generations() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    create_file(&map);
    let entry = entry("map", &map, fixture.path());

    let first = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        std::slice::from_ref(&entry),
        &[],
    );
    let second = CodeMapRegistry::load(Some(maps.to_str().unwrap()), &[entry], &[]);

    assert_ne!(
        first.resolve("map").unwrap().generation(),
        second.resolve("map").unwrap().generation()
    );
}

#[test]
#[serial_test::serial]
fn default_production_database_is_protected_without_a_runtime_database_path() {
    let original_home = std::env::var_os("HOME");
    let fixture = tempfile::tempdir().expect("fixture");
    let home = fixture.path().join("home");
    let map_root = home.join(".khive");
    std::fs::create_dir_all(&map_root).expect("default database directory");
    let default_database = map_root.join("khive.db");
    create_file(&default_database);
    std::env::set_var("HOME", &home);

    let outcome = std::panic::catch_unwind(|| {
        let config = RuntimeConfig {
            db_path: None,
            code_map_root: Some(map_root.display().to_string()),
            code_maps: vec![entry("default", &default_database, fixture.path())],
            ..RuntimeConfig::default()
        };
        let registry = CodeMapRegistry::load_for_runtime(&config, None);

        assert!(registry.is_empty());
        assert!(registry.resolve("default").is_err());
    });

    match original_home {
        Some(home) => std::env::set_var("HOME", home),
        None => std::env::remove_var("HOME"),
    }
    outcome.expect("default production database must be protected");
}

#[test]
fn core_runtime_shares_the_source_code_map_registration() {
    let fixture = tempfile::tempdir().expect("fixture");
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).expect("map root");
    let map = maps.join("workspace.db");
    create_file(&map);
    let config = RuntimeConfig {
        db_path: None,
        backend_id: crate::BackendId::parse("code").expect("backend id"),
        code_map_root: Some(maps.display().to_string()),
        code_maps: vec![entry("workspace", &map, fixture.path())],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::from_backend(
        std::sync::Arc::new(crate::StorageBackend::memory().expect("pack backend")),
        config,
    );
    let source_generation = runtime
        .code_map_registry()
        .resolve("workspace")
        .expect("source registration")
        .generation();
    let core = runtime
        .with_core_backend(std::sync::Arc::new(
            crate::StorageBackend::memory().expect("main backend"),
        ))
        .core();

    assert_eq!(
        core.code_map_registry()
            .resolve("workspace")
            .expect("core registration")
            .generation(),
        source_generation
    );
}

#[test]
#[serial_test::serial]
fn rollback_map_reads_rows_through_a_short_read_only_handle() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    create_queryable_map(&map);
    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        &[],
    );
    let before = crate::StorageBackend::active_code_map_handles_for_test(&map);

    let value = registry
        .with_read("map", |connection| {
            assert_eq!(
                crate::StorageBackend::active_code_map_handles_for_test(&map),
                before + 1
            );
            assert!(connection
                .execute("INSERT INTO records VALUES ('write')", [])
                .is_err());
            connection
                .query_row("SELECT value FROM records", [], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|_| RuntimeError::Internal("map query failed".to_string()))
        })
        .unwrap();

    assert_eq!(value, "registered");
    assert_eq!(
        crate::StorageBackend::active_code_map_handles_for_test(&map),
        before
    );
}

#[test]
fn replaced_registered_main_file_fails_closed() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    let replacement = maps.join("replacement.db");
    create_queryable_map(&map);
    create_queryable_map(&replacement);
    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        &[],
    );

    std::fs::rename(&map, maps.join("displaced.db")).unwrap();
    std::fs::rename(&replacement, &map).unwrap();

    let error = read_map_value(&registry).unwrap_err().to_string();
    assert!(error.contains("identity") || error.contains("available") || error.contains("safely"));
    assert!(!error.contains(&map.display().to_string()));
}

#[test]
fn removed_registered_main_file_fails_closed() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    create_queryable_map(&map);
    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        &[],
    );

    std::fs::remove_file(&map).unwrap();

    assert!(read_map_value(&registry).is_err());
}

#[cfg(unix)]
#[test]
fn registered_map_refuses_a_companion_symlink_to_a_production_file() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    let production = fixture.path().join("production.db");
    let production_wal = PathBuf::from(format!("{}-wal", production.display()));
    create_queryable_map(&map);
    create_file(&production);
    // An empty journal is not hot, so without the companion fence this read would succeed.
    std::fs::write(&production_wal, b"").unwrap();
    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        std::slice::from_ref(&production),
    );
    let journal = PathBuf::from(format!("{}-journal", map.display()));
    std::os::unix::fs::symlink(&production_wal, &journal).unwrap();

    assert!(read_map_value(&registry).is_err());
}

#[test]
fn registered_map_refuses_a_companion_hard_link_to_a_production_file() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    let production = fixture.path().join("production.db");
    let production_wal = PathBuf::from(format!("{}-wal", production.display()));
    create_queryable_map(&map);
    create_file(&production);
    // An empty journal is not hot, so without the companion fence this read would succeed.
    std::fs::write(&production_wal, b"").unwrap();
    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        std::slice::from_ref(&production),
    );
    let journal = PathBuf::from(format!("{}-journal", map.display()));
    std::fs::hard_link(&production_wal, &journal).unwrap();

    assert!(read_map_value(&registry).is_err());
}

#[test]
fn rollback_map_reads_through_a_legitimate_companion() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    create_queryable_map(&map);
    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        &[],
    );
    // SQLite opens an existing journal read-only to learn whether it is hot. The companion has its
    // own identity and must not be compared against the registered main file.
    let journal = PathBuf::from(format!("{}-journal", map.display()));
    std::fs::write(&journal, b"").unwrap();

    assert_eq!(read_map_value(&registry).unwrap(), "registered");
}

#[test]
fn wal_mode_map_is_refused_with_code_ingest_as_the_remedy() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    let wal = PathBuf::from(format!("{}-wal", map.display()));
    let writer = rusqlite::Connection::open(&map).unwrap();
    let mode: String = writer
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    writer
        .execute_batch(
            "CREATE TABLE records (value TEXT NOT NULL); INSERT INTO records VALUES ('registered');",
        )
        .unwrap();
    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        &[],
    );
    assert!(wal.exists());
    let with_companions = read_map_value(&registry).unwrap_err().to_string();

    // A clean close removes the companions; the header still records WAL mode.
    drop(writer);
    assert!(!wal.exists());
    let header_only = read_map_value(&registry).unwrap_err().to_string();

    for error in [with_companions, header_only] {
        assert!(
            error.contains("WAL mode") && error.contains("code.ingest"),
            "{error}"
        );
        assert!(!error.contains(&map.display().to_string()));
    }
}

#[test]
fn rollback_map_with_a_stray_wal_companion_is_refused_as_wal_mode() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    create_queryable_map(&map);
    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        &[],
    );
    std::fs::write(format!("{}-wal", map.display()), b"").unwrap();

    let error = read_map_value(&registry).unwrap_err().to_string();
    assert!(
        error.contains("WAL mode") && error.contains("code.ingest"),
        "{error}"
    );
}

#[test]
fn hard_linked_map_file_is_not_registered() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    // A second link inside the root names a file outside it, and canonical
    // containment does not resolve hard links.
    let outside = fixture.path().join("outside.db");
    create_queryable_map(&outside);
    let linked = maps.join("linked.db");
    std::fs::hard_link(&outside, &linked).unwrap();
    let valid = maps.join("valid.db");
    create_queryable_map(&valid);

    let (registry, log_text) = capture_warnings(|| {
        CodeMapRegistry::load(
            Some(maps.to_str().unwrap()),
            &[
                entry("linked", &linked, fixture.path()),
                entry("valid", &valid, fixture.path()),
            ],
            &[],
        )
    });

    assert!(registry.resolve("linked").is_err());
    assert!(registry.resolve("valid").is_ok());
    assert!(log_text.contains("linked"), "{log_text}");
    assert!(log_text.contains("exactly one link"), "{log_text}");
}

#[test]
fn hard_link_added_after_registration_fails_the_next_read() {
    let fixture = tempfile::tempdir().unwrap();
    let maps = fixture.path().join("maps");
    std::fs::create_dir(&maps).unwrap();
    let map = maps.join("map.db");
    create_queryable_map(&map);
    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        &[],
    );
    assert_eq!(read_map_value(&registry).unwrap(), "registered");

    let second = fixture.path().join("second-link.db");
    std::fs::hard_link(&map, &second).unwrap();
    let error = read_map_value(&registry).unwrap_err().to_string();
    assert!(error.contains("\"map\""), "{error}");
    assert!(error.contains("exactly one link"), "{error}");
    assert!(!error.contains(&map.display().to_string()), "{error}");
    assert!(!error.contains(&second.display().to_string()), "{error}");

    std::fs::remove_file(&second).unwrap();
    let reloaded = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        &[],
    );
    assert_eq!(read_map_value(&reloaded).unwrap(), "registered");
}

#[test]
fn read_only_open_refuses_a_main_file_with_a_second_link() {
    let fixture = tempfile::tempdir().unwrap();
    let map = fixture.path().join("map.db");
    create_queryable_map(&map);
    let identity = super::path_identity(&map).unwrap();
    // With one link the guarded open succeeds, so the refusal below is the
    // link check on the descriptor SQLite reads.
    drop(crate::StorageBackend::open_code_map_read_only(&map, identity, &[]).unwrap());

    std::fs::hard_link(&map, fixture.path().join("second-link.db")).unwrap();
    let error = crate::StorageBackend::open_code_map_read_only(&map, identity, &[])
        .err()
        .expect("a second link refuses the read-only open")
        .to_string();
    assert!(error.contains("exactly one link"), "{error}");
}
