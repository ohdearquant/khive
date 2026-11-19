#[test]
fn default_wal_ceiling_is_explicitly_disabled_in_diagnostics() {
    let pool = ConnectionPool::new(PoolConfig::for_test()).expect("in-memory pool");
    let report = collect(
        &pool,
        BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    let json = serde_json::to_value(report).expect("report serializes");
    assert_eq!(
        json.get("wal_ceiling"),
        Some(&serde_json::json!({
            "configured_bytes": 0,
            "effective_bytes": 0,
            "source": "default",
            "enabled": false,
            "status": "disabled"
        })),
        "zero is an explicit disabled policy, not an omitted field"
    );
}

#[test]
fn read_only_wal_ceiling_keeps_configured_value_without_enforcement() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("read-only-ceiling.db");
    rusqlite::Connection::open(&path)
        .expect("create source database")
        .execute_batch("CREATE TABLE seed (id INTEGER PRIMARY KEY)")
        .expect("persist source database");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        read_only: true,
        write_queue_enabled: Some(false),
        wal_ceiling: WalCeilingPolicy {
            bytes: 8192,
            source: WalCeilingSource::BackendField,
        },
        ..PoolConfig::for_test()
    })
    .expect("read-only backend must accept configured policy");
    let report = collect(
        &pool,
        BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    let json = serde_json::to_value(report).expect("report serializes");
    assert_eq!(
        json["wal_ceiling"],
        serde_json::json!({
            "configured_bytes": 8192,
            "effective_bytes": 0,
            "source": "backend_field",
            "enabled": false,
            "status": "read_only_not_enforced"
        })
    );
}
