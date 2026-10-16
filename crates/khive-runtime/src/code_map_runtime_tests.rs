use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{CodeMapConfig, CodeMapRegistry};
use crate::{runtime_config_from_khive_config, KhiveConfig, KhiveRuntime, RuntimeConfig};

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

    let registry = CodeMapRegistry::load(
        Some(maps.to_str().unwrap()),
        &[entry("map", &map, fixture.path())],
        std::slice::from_ref(&production),
    );

    assert!(registry.is_empty());
    assert!(registry.resolve("map").is_err());
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

    let registry = CodeMapRegistry::load_for_runtime(&config, None);

    assert!(registry.is_empty());
    assert!(registry.resolve("map").is_err());
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

    let first = CodeMapRegistry::load(Some(maps.to_str().unwrap()), &[entry.clone()], &[]);
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
