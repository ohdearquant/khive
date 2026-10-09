// Environment fixtures share the parent tests module through include!.
// Cases that open pools configure the child before any reader or writer starts.

const POOL_ENV_VARS: [&str; 10] = [
    "KHIVE_BUSY_TIMEOUT_SECS",
    "KHIVE_CHECKOUT_TIMEOUT_SECS",
    "KHIVE_READER_CHECKOUT_WARN_SECS",
    "KHIVE_READER_MAX_AGE_SECS",
    "KHIVE_READER_MAX_OPS",
    "KHIVE_WAL_AUTOCHECKPOINT_PAGES",
    "KHIVE_JOURNAL_SIZE_LIMIT_BYTES",
    "KHIVE_WRITE_QUEUE",
    "KHIVE_WRITE_QUEUE_CAPACITY",
    "KHIVE_WRITE_ROUTING",
];

struct PoolEnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl PoolEnvGuard {
    fn capture() -> Self {
        Self {
            saved: POOL_ENV_VARS
                .into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect(),
        }
    }
}

impl Drop for PoolEnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => crate::test_process::set_var(key, value),
                None => crate::test_process::remove_var(key),
            }
        }
    }
}

fn clear_pool_env() -> PoolEnvGuard {
    let guard = PoolEnvGuard::capture();
    for var in POOL_ENV_VARS {
        crate::test_process::remove_var(var);
    }
    guard
}
#[test]
#[serial]
fn pool_config_default_values_match_constants() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    // Ensure defaults are not accidentally changed. The process env may
    // legitimately carry overrides inherited from the test invocation, so
    // clear them in this isolated child first: this test asserts constants.
    let _pool_env = clear_pool_env();
    let cfg = PoolConfig::default();
    assert_eq!(
        cfg.journal_size_limit_bytes,
        DEFAULT_JOURNAL_SIZE_LIMIT_BYTES
    );
    assert_eq!(cfg.busy_timeout, Duration::from_secs(30));
    assert_eq!(cfg.checkout_timeout, Duration::from_secs(5));
    assert_eq!(cfg.reader_checkout_warn_after, Duration::from_secs(10));
}

#[test]
fn pool_config_reader_checkout_warning_threshold() {
    if crate::test_process::run_in_child(|command| {
        command.env_remove("KHIVE_READER_CHECKOUT_WARN_SECS");
    }) {
        return;
    }
    let key = "KHIVE_READER_CHECKOUT_WARN_SECS";
    assert_eq!(
        PoolConfig::default().reader_checkout_warn_after,
        Duration::from_secs(10)
    );
    for (value, seconds) in [("1", 1), ("0", 0), ("invalid", 10), ("-1", 10)] {
        crate::test_process::set_var(key, value);
        assert_eq!(
            PoolConfig::default().reader_checkout_warn_after,
            Duration::from_secs(seconds)
        );
    }
}

#[test]
#[serial]
fn legacy_env_cannot_change_wal_autocheckpoint() {
    if crate::test_process::run_in_child(|command| {
        for key in POOL_ENV_VARS {
            command.env_remove(key);
        }
        command.env("KHIVE_WAL_AUTOCHECKPOINT_PAGES", "8000");
    }) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy_autocheckpoint_env.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        ..PoolConfig::for_test()
    })
    .expect("pool open");
    {
        let writer = pool.writer().expect("writer");
        assert_eq!(
            wal_autocheckpoint_pages(writer.conn()),
            FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
            "the removed env override must not change the unclaimed fallback"
        );
    }
    // The pooled writer takes its setting at pool open; every later
    // writer-capable connection selects it from the ownership state.
    let standalone = pool
        .open_standalone_writer_untracked()
        .expect("standalone writer before the claim");
    assert_eq!(
        wal_autocheckpoint_pages(&standalone),
        FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
        "the removed env override must not change a later writer's unclaimed fallback"
    );
    drop(standalone);
    pool.claim_checkpoint_ownership().expect("claim ownership");
    let writer = pool.writer().expect("writer after claim");
    assert_eq!(
        wal_autocheckpoint_pages(writer.conn()),
        0,
        "the removed env override must not change the claimed-owner setting"
    );
    drop(writer);
    let standalone = pool
        .open_standalone_writer_untracked()
        .expect("standalone writer after the claim");
    assert_eq!(
        wal_autocheckpoint_pages(&standalone),
        0,
        "the removed env override must not change a later writer's claimed-owner setting"
    );
}

#[test]
#[serial]
fn pool_config_env_override_journal_size_limit() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES", "134217728");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES");
    assert_eq!(cfg.journal_size_limit_bytes, 134_217_728);
}

#[test]
#[serial]
fn pool_config_env_override_busy_timeout() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_BUSY_TIMEOUT_SECS", "60");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_BUSY_TIMEOUT_SECS");
    assert_eq!(cfg.busy_timeout, Duration::from_secs(60));
}

#[test]
#[serial]
fn pool_config_env_override_checkout_timeout() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_CHECKOUT_TIMEOUT_SECS", "10");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_CHECKOUT_TIMEOUT_SECS");
    assert_eq!(cfg.checkout_timeout, Duration::from_secs(10));
}

#[test]
#[serial]
fn pool_config_write_queue_defaults_unset() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let _pool_env = clear_pool_env();
    let cfg = PoolConfig::default();
    assert_eq!(cfg.write_queue_enabled, None);
    assert_eq!(cfg.write_queue_capacity, DEFAULT_WRITE_QUEUE_CAPACITY);
}

#[test]
#[serial]
fn clear_pool_env_restores_overrides_on_drop() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let _ambient_env = PoolEnvGuard::capture();
    crate::test_process::set_var("KHIVE_BUSY_TIMEOUT_SECS", "73");

    {
        let _pool_env = clear_pool_env();
        assert_eq!(std::env::var_os("KHIVE_BUSY_TIMEOUT_SECS"), None);
    }

    assert_eq!(
        std::env::var_os("KHIVE_BUSY_TIMEOUT_SECS"),
        Some(std::ffi::OsString::from("73"))
    );
}

#[test]
#[serial]
fn pool_config_env_override_write_queue_enabled() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_WRITE_QUEUE", "1");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_WRITE_QUEUE");
    assert_eq!(cfg.write_queue_enabled, Some(true));
}

#[test]
#[serial]
fn pool_config_env_override_write_queue_enabled_accepts_true_case_insensitive() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_WRITE_QUEUE", "True");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_WRITE_QUEUE");
    assert_eq!(cfg.write_queue_enabled, Some(true));
}

#[test]
#[serial]
fn pool_config_env_override_write_queue_enabled_accepts_zero_as_explicit_off() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_WRITE_QUEUE", "0");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_WRITE_QUEUE");
    assert_eq!(cfg.write_queue_enabled, Some(false));
}

/// A SET-but-non-Unicode `KHIVE_WRITE_QUEUE` value (invalid UTF-8 on
/// unix) must count as SET — `Some(false)` ("any SET value other than
/// 1/true means off"), never a fall-through to the file-backed default.
/// That is why `PoolConfig::default()` reads `var_os`, not `var`.
#[cfg(unix)]
#[test]
#[serial]
fn pool_config_env_override_write_queue_non_unicode_value_is_explicit_off() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    use std::os::unix::ffi::OsStrExt;
    let _pool_env = clear_pool_env();
    crate::test_process::set_var(
        "KHIVE_WRITE_QUEUE",
        std::ffi::OsStr::from_bytes(b"\xff\xfe"),
    );
    let cfg = PoolConfig::default();
    assert_eq!(cfg.write_queue_enabled, Some(false));
}

#[test]
#[serial]
fn pool_config_env_override_write_queue_invalid_value_is_explicit_off() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    // Documented contract (`write_queue_enabled` docs): `"1"`/`"true"`
    // (case-insensitive) set `Some(true)`; any other value — garbage
    // included — sets `Some(false)`, never `None`.
    crate::test_process::set_var("KHIVE_WRITE_QUEUE", "banana");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_WRITE_QUEUE");
    assert_eq!(cfg.write_queue_enabled, Some(false));
}

#[test]
#[serial]
fn pool_config_write_routing_strict_defaults_off() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let _pool_env = clear_pool_env();
    let cfg = PoolConfig::default();
    assert!(!cfg.write_routing_strict);
}

#[test]
#[serial]
fn pool_config_env_override_write_routing_strict() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_WRITE_ROUTING", "strict");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_WRITE_ROUTING");
    assert!(cfg.write_routing_strict);
}

#[test]
#[serial]
fn pool_config_env_override_write_routing_strict_case_insensitive() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_WRITE_ROUTING", "STRICT");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_WRITE_ROUTING");
    assert!(cfg.write_routing_strict);
}

#[test]
#[serial]
fn pool_config_env_write_routing_ignores_unrecognized_value() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_WRITE_ROUTING", "eventual");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_WRITE_ROUTING");
    assert!(!cfg.write_routing_strict);
}

#[test]
#[serial]
fn pool_config_env_override_write_queue_capacity() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_WRITE_QUEUE_CAPACITY", "64");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_WRITE_QUEUE_CAPACITY");
    assert_eq!(cfg.write_queue_capacity, 64);
}

#[test]
#[serial]
fn pool_config_env_invalid_write_queue_capacity_falls_back_to_default() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_WRITE_QUEUE_CAPACITY", "0");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_WRITE_QUEUE_CAPACITY");
    assert_eq!(cfg.write_queue_capacity, DEFAULT_WRITE_QUEUE_CAPACITY);
}

#[test]
#[serial]
fn pool_config_invalid_journal_size_limit_falls_back_to_default() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::set_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES", "");
    let cfg = PoolConfig::default();
    crate::test_process::remove_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES");
    assert_eq!(
        cfg.journal_size_limit_bytes,
        DEFAULT_JOURNAL_SIZE_LIMIT_BYTES
    );
}
#[tokio::test]
#[serial]
async fn unset_write_queue_resolves_on_for_file_backed_pool() {
    if crate::test_process::run_in_child(|command| {
        for key in POOL_ENV_VARS {
            command.env_remove(key);
        }
    }) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unset_file_backed.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        write_queue_enabled: None,
        ..PoolConfig::for_test()
    })
    .expect("file-backed pool should open");
    assert_eq!(pool.config().write_queue_enabled, Some(true));
    // Behavioral half: the resolved value actually routes — a writer
    // task spawns for this pool, not merely a config field flipping.
    assert!(
        pool.writer_task_handle()
            .expect("spawn inside a runtime context must not error")
            .is_some(),
        "resolved-on file-backed pool must actually spawn the writer task"
    );
}

#[tokio::test]
#[serial]
async fn unset_write_queue_resolves_off_for_memory_backed_pool() {
    if crate::test_process::run_in_child(|command| {
        for key in POOL_ENV_VARS {
            command.env_remove(key);
        }
    }) {
        return;
    }
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        write_queue_enabled: None,
        ..PoolConfig::default()
    })
    .expect("in-memory pool should open");
    assert_eq!(pool.config().write_queue_enabled, Some(false));
    // Behavioral half: resolved-off means no writer task, even inside a
    // runtime context where one could spawn.
    assert!(
        pool.writer_task_handle()
            .expect("disabled queue must resolve without error")
            .is_none(),
        "resolved-off in-memory pool must not spawn a writer task"
    );
}

#[test]
#[serial]
fn explicit_false_stays_off_for_file_backed_pool() {
    if crate::test_process::run_in_child(|command| {
        for key in POOL_ENV_VARS {
            command.env_remove(key);
        }
    }) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("explicit_false_file_backed.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("file-backed pool should open");
    assert_eq!(pool.config().write_queue_enabled, Some(false));
}

#[tokio::test]
#[serial]
async fn explicit_true_stays_on_for_memory_backed_pool() {
    if crate::test_process::run_in_child(|command| {
        for key in POOL_ENV_VARS {
            command.env_remove(key);
        }
    }) {
        return;
    }
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        write_queue_enabled: Some(true),
        ..PoolConfig::default()
    })
    .expect("in-memory pool should open");
    assert_eq!(pool.config().write_queue_enabled, Some(true));
    // Pinned behavioral contract: the explicit-on preference survives in
    // the stored config, but an in-memory pool cannot host a writer
    // task — `writer_task::spawn` fails its standalone-connection open
    // and degrades to no writer task, so callers fall back to the
    // legacy pool-mutex write path and there is no JoinHandle to drain.
    assert!(
        pool.writer_task_handle()
            .expect("spawn degrade must resolve without error")
            .is_none(),
        "explicit-on in-memory pool must degrade to no writer task"
    );
    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "the spawn attempt must happen exactly once and degrade, not retry"
    );
    assert!(
        pool.take_writer_task_join().is_none(),
        "a degraded spawn stores no JoinHandle to drain"
    );
}

#[test]
#[serial]
fn explicit_true_on_memory_pool_warns_but_false_and_none_do_not() {
    if crate::test_process::run_in_child(|command| {
        for key in POOL_ENV_VARS {
            command.env_remove(key);
        }
    }) {
        return;
    }
    let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = WarningCapture {
        messages: Arc::clone(&messages),
    };

    tracing::subscriber::with_default(subscriber, || {
        let _explicit_true = ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: Some(true),
            ..PoolConfig::default()
        })
        .expect("in-memory pool should open");
        let _explicit_false = ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .expect("in-memory pool should open");
        let _unset = ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: None,
            ..PoolConfig::default()
        })
        .expect("in-memory pool should open");
    });

    let messages = messages.lock().unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.contains("write queue explicitly requested"))
            .count(),
        1,
        "only an explicit in-memory queue request should warn: {messages:?}"
    );
    let warning = messages
        .iter()
        .find(|message| message.contains("write queue explicitly requested"))
        .expect("explicit in-memory queue warning should be captured");
    assert!(
        warning.contains("in-memory pools cannot host a writer task"),
        "warning must explain why the request is inert: {messages:?}"
    );
}
