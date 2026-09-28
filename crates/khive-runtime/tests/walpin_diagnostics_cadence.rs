//! The environment overrides are isolated in this single-test binary.

#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use khive_db::{walpin, CheckpointConfig, SessionSweepConfig, StorageBackend};
use khive_runtime::{KhiveRuntime, RuntimeConfig};

struct RestoreEnv(Vec<(&'static str, Option<OsString>)>);

impl Drop for RestoreEnv {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_forecast_uses_compiled_session_fallback_and_preserves_evidence() {
    let overrides = [
        ("KHIVE_CHECKPOINT_INTERVAL_MS", "500"),
        ("KHIVE_SESSION_SWEEP_INTERVAL_MS", "60000"),
        ("KHIVE_WALPIN_SIDECAR", "1"),
    ];
    let _restore = RestoreEnv(
        overrides
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect(),
    );
    for (key, value) in overrides {
        std::env::set_var(key, value);
    }
    let checkpoint = CheckpointConfig::from_env().interval;
    assert_eq!(checkpoint, Duration::from_millis(500));
    assert_eq!(
        SessionSweepConfig::default().interval,
        Duration::from_secs(5)
    );
    assert_eq!(
        SessionSweepConfig::from_env().interval,
        Duration::from_secs(60)
    );

    let root = tempfile::tempdir().expect("fixture root");
    let backend =
        Arc::new(StorageBackend::sqlite_for_test(root.path().join("forecast.db")).unwrap());
    let pool = backend.pool_arc();
    let runtime = KhiveRuntime::from_backend(backend, RuntimeConfig::no_embeddings());
    let path = pool.canonical_path().expect("canonical fixture path");
    let sidecar = walpin::sidecar_dir_for(path);
    walpin::ensure_sidecar_dir(&sidecar).unwrap();
    let mut evidence = Vec::new();
    // Both ages exceed a checkpoint-derived 3s window; only one exceeds
    // housekeeping's 15s window, and neither exceeds a local 180s override.
    for (pid, age) in [(2_000_000_001, 5), (2_000_000_002, 40)] {
        assert!(!walpin::is_process_alive(pid));
        let temp = sidecar.join(format!(".{pid}.beacon.tmp"));
        let body = serde_json::to_vec(&serde_json::json!({
            "pid": pid, "process_role": "session", "started_at": 1
        }))
        .unwrap();
        fs::write(&temp, &body).unwrap();
        fs::File::options()
            .write(true)
            .open(&temp)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(age))
            .unwrap();
        let modified = fs::metadata(&temp).unwrap().modified().unwrap();
        evidence.push((temp, body, modified));
    }

    let acquisitions = pool.writer_acquisition_snapshot();
    let report = runtime.db_diagnostics().await.expect("diagnostics report");
    assert_eq!(report.wal_pin.sidecar_listing_truncated, Some(false));
    assert_eq!(report.wal_pin.sidecar_entries_cleanup_would_reap, Some(1));
    assert_eq!(pool.writer_acquisition_snapshot(), acquisitions);
    for (temp, body, modified) in evidence {
        assert_eq!(fs::read(&temp).unwrap(), body);
        assert_eq!(fs::metadata(&temp).unwrap().modified().unwrap(), modified);
    }
}
