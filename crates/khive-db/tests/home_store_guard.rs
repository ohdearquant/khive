use khive_db::{ConnectionPool, PoolConfig};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const ALLOW_HOME_STORE_ENV: &str = "KHIVE_ALLOW_HOME_STORE";

struct HomeGuard {
    home: Option<OsString>,
    allow_home_store: Option<OsString>,
}

impl HomeGuard {
    fn install(home: &Path) -> Self {
        let guard = Self {
            home: std::env::var_os("HOME"),
            allow_home_store: std::env::var_os(ALLOW_HOME_STORE_ENV),
        };
        std::env::set_var("HOME", home);
        std::env::remove_var(ALLOW_HOME_STORE_ENV);
        guard
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        restore_env("HOME", self.home.take());
        restore_env(ALLOW_HOME_STORE_ENV, self.allow_home_store.take());
    }
}

fn restore_env(key: &str, value: Option<OsString>) {
    match value {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    }
}

fn assert_harness_refuses(path: PathBuf) {
    let message = harness_refusal(path);
    assert!(
        message.contains("test harness refused"),
        "unexpected guard error: {message}"
    );
}

fn assert_harness_refuses_with_direct_binary_instruction(path: PathBuf) {
    let expected_path = path.display().to_string();
    let message = harness_refusal(path);
    assert!(
        message.contains("run the built binary directly") && message.contains(&expected_path),
        "refusal did not point at the direct-binary workflow: {message}"
    );
}

fn harness_refusal(path: PathBuf) -> String {
    let error = match ConnectionPool::new(PoolConfig {
        path: Some(path),
        ..PoolConfig::default()
    }) {
        Ok(_) => panic!("test harness opened a database under HOME/.khive"),
        Err(error) => error,
    };
    error.to_string()
}

fn fake_home() -> (tempfile::TempDir, PathBuf) {
    assert_eq!(
        std::env::var("KHIVE_TEST_HARNESS").as_deref(),
        Ok("1"),
        "Cargo must mark the test process"
    );
    let home = tempfile::tempdir().expect("temporary HOME");
    let home_data_dir = home.path().join(".khive");
    std::fs::create_dir(&home_data_dir).expect("create fake home data directory");
    (home, home_data_dir)
}

#[test]
#[serial_test::serial(home_store_guard)]
fn release_dependency_refuses_home_store() {
    let (home, home_data_dir) = fake_home();
    let _home = HomeGuard::install(home.path());
    let database = home_data_dir.join("release.db");

    assert_harness_refuses_with_direct_binary_instruction(database.clone());

    assert!(!database.exists(), "guard must run before SQLite opens");
}

#[test]
#[serial_test::serial(home_store_guard)]
fn legacy_boolean_override_does_not_allow_home_store() {
    let (home, home_data_dir) = fake_home();
    let _home = HomeGuard::install(home.path());
    let database = home_data_dir.join("legacy-override.db");
    std::env::set_var(ALLOW_HOME_STORE_ENV, "1");

    assert_harness_refuses_with_direct_binary_instruction(database.clone());

    assert!(!database.exists(), "guard must run before SQLite opens");
}

#[test]
#[serial_test::serial(home_store_guard)]
fn exact_path_override_does_not_allow_home_store() {
    let (home, home_data_dir) = fake_home();
    let _home = HomeGuard::install(home.path());
    let database = home_data_dir.join("exact-override.db");
    std::env::set_var(ALLOW_HOME_STORE_ENV, &database);

    assert_harness_refuses_with_direct_binary_instruction(database.clone());

    assert!(!database.exists(), "guard must run before SQLite opens");
}

#[test]
#[serial_test::serial(home_store_guard)]
fn different_path_override_does_not_allow_home_store() {
    let (home, home_data_dir) = fake_home();
    let _home = HomeGuard::install(home.path());
    let database = home_data_dir.join("requested.db");
    std::env::set_var(ALLOW_HOME_STORE_ENV, home_data_dir.join("allowed.db"));

    assert_harness_refuses_with_direct_binary_instruction(database.clone());

    assert!(!database.exists(), "guard must run before SQLite opens");
}

#[test]
#[serial_test::serial(home_store_guard)]
fn refuses_parent_traversal_into_home_store() {
    let (home, home_data_dir) = fake_home();
    let _home = HomeGuard::install(home.path());
    let outside = home.path().join("outside");
    std::fs::create_dir(&outside).expect("create traversal anchor");
    let database = home_data_dir.join("traversal.db");
    let traversal = outside.join("..").join(".khive").join("traversal.db");

    assert_harness_refuses(traversal);

    assert!(!database.exists(), "guard must run before SQLite opens");
}

#[cfg(unix)]
#[test]
#[serial_test::serial(home_store_guard)]
fn refuses_symlink_alias_into_home_store() {
    let (home, home_data_dir) = fake_home();
    let _home = HomeGuard::install(home.path());
    let alias = home.path().join("store-alias");
    std::os::unix::fs::symlink(&home_data_dir, &alias).expect("create store alias");
    let database = home_data_dir.join("symlink.db");

    assert_harness_refuses(alias.join("symlink.db"));

    assert!(!database.exists(), "guard must run before SQLite opens");
}

#[test]
#[serial_test::serial(home_store_guard)]
fn refuses_sqlite_uri_paths() {
    let (home, home_data_dir) = fake_home();
    let _home = HomeGuard::install(home.path());
    let database = home_data_dir.join("uri.db");
    let uri = PathBuf::from(format!("file:{}", database.display()));

    assert_harness_refuses(uri);

    assert!(!database.exists(), "guard must run before SQLite opens");
}
