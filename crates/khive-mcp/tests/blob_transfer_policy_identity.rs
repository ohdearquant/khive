use khive_mcp::server::compute_config_id;
use khive_runtime::{runtime_config_from_khive_config, KhiveConfig, RuntimeConfig};
use sha2::{Digest, Sha256};
use std::ffi::OsString;

struct TransferEnvironment(Option<OsString>);

impl TransferEnvironment {
    fn unset() -> Self {
        let previous = std::env::var_os("KHIVE_FILE_TRANSFERS");
        std::env::remove_var("KHIVE_FILE_TRANSFERS");
        Self(previous)
    }
}

impl Drop for TransferEnvironment {
    fn drop(&mut self) {
        match &self.0 {
            Some(value) => std::env::set_var("KHIVE_FILE_TRANSFERS", value),
            None => std::env::remove_var("KHIVE_FILE_TRANSFERS"),
        }
    }
}

fn base_config() -> RuntimeConfig {
    let mut base = RuntimeConfig::no_embeddings();
    base.db_path = None;
    base.packs = vec!["blob".to_owned()];
    base.mounts.clear();
    base.allowed_outbound_namespaces.clear();
    base.brain.fleet_readers.clear();
    base.backend_id = khive_runtime::BackendId::main();
    base.wal_ceiling_bytes = 0;
    base.display_timezone = "UTC".parse().unwrap();
    base
}

// Independent baseline after the receipt cutover, with file transfers disabled.
// The fixed receipt digest binds the v2 domain plus `[[],null]` declarations;
// this oracle never calls back into the fingerprint under test.
fn transfer_disabled_identity(base: &RuntimeConfig) -> String {
    assert!(base.db_path.is_none());
    assert!(base.gate.configuration_fingerprint().is_none());
    assert!(base.embedding_model.is_none());
    assert!(base.additional_embedding_models.is_empty());
    assert!(base.credentials.is_empty());
    assert!(base.visibility_receipts.is_none());
    assert!(base.configured_engines().is_empty());
    // An empty ordered peer list serializes as `[]` under the peer domain.
    let mut engine_peers = Sha256::new();
    engine_peers.update(b"khive.ordered-engine-peers.v1");
    engine_peers.update(b"[]");
    let mut git = Sha256::new();
    git.update(b"khive.git-write-policy.v2");
    git.update(serde_json::to_vec(&base.mounts).unwrap());
    git.update(serde_json::to_vec(&base.git_write).unwrap());
    let mut brain = Sha256::new();
    brain.update(b"khive.brain-read-policy.v1");
    brain.update(serde_json::to_vec(&base.brain.fleet_readers).unwrap());
    let mut telemetry = Sha256::new();
    telemetry.update(b"khive.telemetry-policy.v1");
    telemetry.update(serde_json::to_vec(&base.telemetry).unwrap());
    format!(
        concat!(
            "packs=[blob];db=:memory:;embed=none;extra=[];engine_peers={:x};fresh_tail={};",
            "blob_hydration_bytes={};backend={:?}:wal_ceiling_bytes=0;",
            "outbound=[];git_write={:x};brain={:x};telemetry={:x};",
            "display_tz=UTC;visibility_receipts=",
            "90b56d7d55c456ceb209a1e71c875ebc247c19c0bbcf9804aff3b5ba2417bece"
        ),
        engine_peers.finalize(),
        khive_runtime::ann_fresh_tail_enabled_from_env(),
        base.blob_hydration_bytes,
        base.backend_id,
        git.finalize(),
        brain.finalize(),
        telemetry.finalize(),
    )
}

#[test]
#[serial_test::serial(blob_transfer_environment)]
fn enabled_file_transfers_distinguish_daemon_identity_without_changing_disabled_identity() {
    let _environment = TransferEnvironment::unset();
    let base = base_config();
    let disabled = runtime_config_from_khive_config(&KhiveConfig::default(), base.clone());
    let enabled_file: KhiveConfig = toml::from_str("[blob]\nfile_transfers = true\n").unwrap();
    let enabled = runtime_config_from_khive_config(&enabled_file, base);
    let disabled_id = compute_config_id(&disabled, None);
    assert_eq!(
        disabled_id,
        transfer_disabled_identity(&disabled),
        "disabled hosts keep their established identity"
    );
    let enabled_id = compute_config_id(&enabled, None);
    assert_ne!(
        enabled_id, disabled_id,
        "enabled and disabled boot policies cannot share a warm daemon"
    );
    assert_eq!(
        enabled_id,
        format!("{disabled_id};blob_file_transfers=true")
    );
}

#[test]
#[serial_test::serial(blob_transfer_environment)]
fn daemon_identity_uses_the_boot_snapshot_after_the_transfer_environment_changes() {
    let _environment = TransferEnvironment::unset();
    let disabled = base_config();
    let before = compute_config_id(&disabled, None);
    assert_eq!(before, transfer_disabled_identity(&disabled));
    std::env::set_var("KHIVE_FILE_TRANSFERS", "1");
    assert_eq!(
        compute_config_id(&disabled, None),
        before,
        "a live environment change cannot grant a constructed host new capabilities"
    );
    let enabled = base_config();
    assert_ne!(
        compute_config_id(&enabled, None),
        before,
        "a newly constructed enabled host has a distinct identity"
    );
    std::env::remove_var("KHIVE_FILE_TRANSFERS");
    assert_ne!(
        compute_config_id(&enabled, None),
        before,
        "a constructed host retains its captured capability"
    );
    assert_eq!(compute_config_id(&disabled, None), before);
}

#[test]
#[serial_test::serial(blob_transfer_environment)]
fn knowledge_import_roots_distinguish_daemon_identity_without_changing_the_empty_identity() {
    let _environment = TransferEnvironment::unset();
    let base = base_config();
    let empty = runtime_config_from_khive_config(&KhiveConfig::default(), base.clone());
    let one: KhiveConfig = toml::from_str("[knowledge]\nimport_roots = [\"/srv/a\"]\n").unwrap();
    let other: KhiveConfig = toml::from_str("[knowledge]\nimport_roots = [\"/srv/b\"]\n").unwrap();
    let empty_id = compute_config_id(&empty, None);
    assert_eq!(
        empty_id,
        transfer_disabled_identity(&empty),
        "a host with no import roots keeps its established identity"
    );
    let one_id = compute_config_id(&runtime_config_from_khive_config(&one, base.clone()), None);
    let other_id = compute_config_id(&runtime_config_from_khive_config(&other, base), None);
    assert!(one_id.starts_with(&format!("{empty_id};knowledge_import_roots=")));
    assert_ne!(one_id, empty_id);
    assert_ne!(
        one_id, other_id,
        "different roots cannot share a warm daemon"
    );
}

#[test]
#[serial_test::serial(blob_transfer_environment)]
fn git_digest_sources_distinguish_daemon_identity_without_changing_the_empty_identity() {
    let _environment = TransferEnvironment::unset();
    let base = base_config();
    let empty = runtime_config_from_khive_config(&KhiveConfig::default(), base.clone());
    let local: KhiveConfig = toml::from_str("[git_digest]\nlocal_roots = [\"/srv/a\"]\n").unwrap();
    let remote: KhiveConfig =
        toml::from_str("[git_digest]\nremotes = [\"https://example.com/a\"]\n").unwrap();
    let empty_id = compute_config_id(&empty, None);
    assert_eq!(empty_id, transfer_disabled_identity(&empty));
    let local_id = compute_config_id(
        &runtime_config_from_khive_config(&local, base.clone()),
        None,
    );
    let remote_id = compute_config_id(&runtime_config_from_khive_config(&remote, base), None);
    assert!(local_id.starts_with(&format!("{empty_id};git_digest_sources=")));
    assert!(remote_id.starts_with(&format!("{empty_id};git_digest_sources=")));
    assert_ne!(
        local_id, remote_id,
        "different sources cannot share a warm daemon"
    );
}

#[test]
fn code_ingest_roots_distinguish_daemon_identity_without_changing_the_empty_identity() {
    let _environment = TransferEnvironment::unset();
    let base = base_config();
    let empty = runtime_config_from_khive_config(&KhiveConfig::default(), base.clone());
    let first: KhiveConfig = toml::from_str("[code_ingest]\nroots = [\"/srv/a\"]\n").unwrap();
    let second: KhiveConfig = toml::from_str("[code_ingest]\nroots = [\"/srv/b\"]\n").unwrap();
    let empty_id = compute_config_id(&empty, None);
    assert_eq!(empty_id, transfer_disabled_identity(&empty));
    let first_id = compute_config_id(
        &runtime_config_from_khive_config(&first, base.clone()),
        None,
    );
    let second_id = compute_config_id(&runtime_config_from_khive_config(&second, base), None);
    assert!(first_id.starts_with(&format!("{empty_id};code_ingest_roots=")));
    assert!(second_id.starts_with(&format!("{empty_id};code_ingest_roots=")));
    assert_ne!(
        first_id, second_id,
        "different roots cannot share a warm daemon"
    );
}
