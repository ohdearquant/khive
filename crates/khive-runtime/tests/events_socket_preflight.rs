#![cfg(unix)]

use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use khive_runtime::events_split::{
    validate_events_socket_path, EventsSplitClient, EventsSplitConfig,
};
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, WalCeilingSource};

fn capacity() -> usize {
    // SAFETY: sockaddr_un is integer fields and a character array; zero is valid.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_path.len()
}

fn socket_with_required_bytes(root: &Path, required: usize) -> PathBuf {
    let name_bytes = required
        .checked_sub(root.as_os_str().as_bytes().len() + 2)
        .expect("private short fixture leaves room for slash and NUL");
    assert!(name_bytes > 0);
    root.join("s".repeat(name_bytes))
}

fn assert_refusal(error: &str, socket: &Path) {
    let path_bytes = socket.as_os_str().as_bytes().len();
    assert!(error.contains(&format!("{socket:?}")), "{error}");
    assert!(
        error.contains(&format!("{path_bytes} path bytes")),
        "{error}"
    );
    assert!(
        error.contains(&format!("{} including NUL", path_bytes + 1)),
        "{error}"
    );
    assert!(
        error.contains(&format!("sun_path limit of {} bytes", capacity())),
        "{error}"
    );
}

#[test]
fn exact_platform_capacity_binds_and_one_more_byte_is_refused() {
    let dir = tempfile::Builder::new()
        .prefix("evp-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let fits = socket_with_required_bytes(&root, capacity());
    assert_eq!(fits.as_os_str().as_bytes().len() + 1, capacity());
    validate_events_socket_path(&fits).expect("NUL-inclusive exact fit");
    let listener =
        std::os::unix::net::UnixListener::bind(&fits).expect("exact fit must really bind");
    assert_eq!(
        listener.local_addr().unwrap().as_pathname(),
        Some(fits.as_path())
    );
    drop(listener);
    std::fs::remove_file(&fits).unwrap();

    let over = socket_with_required_bytes(&root, capacity() + 1);
    let error = validate_events_socket_path(&over).expect_err("one byte over");
    assert!(matches!(&error, RuntimeError::InvalidInput(_)));
    assert_refusal(&error.to_string(), &over);
    assert!(!over.exists());

    // Two UTF-8 bytes replace two ASCII bytes: character-count validation
    // would accept this overlong pathname, while the kernel still sees bytes.
    let mut utf8_name = over.file_name().unwrap().to_str().unwrap().to_owned();
    utf8_name.truncate(utf8_name.len() - 2);
    utf8_name.push('é');
    let utf8_over = root.join(utf8_name);
    assert_eq!(utf8_over.as_os_str().as_bytes().len(), capacity());
    assert_refusal(
        &validate_events_socket_path(&utf8_over)
            .unwrap_err()
            .to_string(),
        &utf8_over,
    );

    let mut non_utf8 = over.as_os_str().as_bytes().to_vec();
    *non_utf8.last_mut().unwrap() = 0xff;
    let non_utf8 = PathBuf::from(std::ffi::OsString::from_vec(non_utf8));
    assert!(non_utf8.to_str().is_none());
    assert_refusal(
        &validate_events_socket_path(&non_utf8)
            .unwrap_err()
            .to_string(),
        &non_utf8,
    );

    let relative = PathBuf::from("r".repeat(capacity() - 1));
    assert_eq!(relative.as_os_str().as_bytes().len() + 1, capacity());
    let anchored = std::env::current_dir().unwrap().join(&relative);
    assert_refusal(
        &validate_events_socket_path(&relative)
            .unwrap_err()
            .to_string(),
        &anchored,
    );

    // Client construction must fail before creating its preflight backend or
    // spawning a forwarder; this ordinary test intentionally has no Tokio runtime.
    let error = EventsSplitClient::new(over.clone()).expect_err("invalid client socket");
    assert_refusal(&error.to_string(), &over);
}

fn isolated_config(root: &Path, db_path: Option<PathBuf>, socket: PathBuf) -> RuntimeConfig {
    RuntimeConfig {
        db_path,
        embedding_model: None,
        additional_embedding_models: vec![],
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_environment: khive_db::DiskGuardEnvironment::default(),
        disk_guard_config: None,
        volume_lock_dir: Some(root.join("volume-locks")),
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        credentials: vec![],
        visibility_receipts: None,
        mounts: vec![],
        packs: vec![],
        events_split: Some(EventsSplitConfig {
            db_path: root.join("events.db"),
            socket_path: Some(socket),
        }),
        ..RuntimeConfig::no_embeddings()
    }
}

#[test]
fn forwarding_constructors_refuse_before_open_or_parent_creation() {
    let mut fixture = None;
    if khive_storage::test_support::run_exact_test_in_child(
        "EVENTS_SOCKET_CONSTRUCTOR_TEST",
        false,
        |command| {
            let private = tempfile::Builder::new()
                .prefix("evc-env-")
                .tempdir_in("/tmp")
                .unwrap();
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("KHIVE_") {
                    command.env_remove(key);
                }
            }
            command
                .env_remove("LATTICE_MODEL_CACHE")
                .env("HOME", private.path().join("home"))
                .env("KHIVE_TEST_HARNESS", "1")
                .env(
                    "KHIVE_WRITER_TIMEOUT_SINK_DIR",
                    private.path().join("writer-logs"),
                );
            fixture = Some(private);
        },
    ) {
        assert!(std::fs::read_dir(fixture.unwrap().path())
            .unwrap()
            .next()
            .is_none());
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("evc-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let over = socket_with_required_bytes(&root, capacity() + 1);
    for readonly in [false, true] {
        let parent = root.join(if readonly { "readonly" } else { "writable" });
        let db = parent.join("main.db");
        let config = isolated_config(&root, Some(db.clone()), over.clone());
        let result = if readonly {
            KhiveRuntime::new_readonly(config)
        } else {
            KhiveRuntime::new(config)
        };
        let error = result
            .err()
            .expect("preflight precedes either backend open");
        assert!(matches!(&error, RuntimeError::InvalidInput(_)));
        assert_refusal(&error.to_string(), &over);
        assert!(
            !parent.exists(),
            "neither constructor may create the database parent"
        );
        assert!(!db.exists());
        assert!(!root.join("events.db").exists());
        assert!(!root.join("volume-locks").exists());
    }
}

#[test]
fn memory_without_a_file_backend_does_not_validate_an_unused_socket() {
    let dir = tempfile::Builder::new()
        .prefix("evm-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let over = socket_with_required_bytes(&root, capacity() + 1);
    let runtime =
        KhiveRuntime::new(isolated_config(&root, None, over)).expect("in-memory constructor");
    assert!(runtime.backend().pool().config().path.is_none());
    assert!(runtime.config().db_path.is_none());
    assert!(std::fs::read_dir(&root).unwrap().next().is_none());
}
