mod storage_section {
    use super::*;

    // ── [storage.blob] section (ADR-111 Amendment 2) ─────────────────────────

    fn assert_unknown_destination_key(content: &str, key: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, content);
        let diagnostic_path = std::fs::canonicalize(&path).unwrap();
        let err = KhiveConfig::load(Some(&path)).expect_err("unknown destination must fail");
        let message = err.to_string();
        assert!(
            message.contains(&format!("unknown field `{key}`")),
            "{message}"
        );
        assert!(
            message.contains(&diagnostic_path.display().to_string()),
            "{message}"
        );
        match err {
            ConfigError::Parse { path, .. } => assert_eq!(path, diagnostic_path),
            other => panic!("expected a file-attributed parse error, got {other:?}"),
        }
    }

    #[test]
    fn test_unknown_write_destination_storage_keys_are_refused() {
        for content in [
            "[storage]\nmain = '.khive/scratch.db'\n",
            "[storage]\nmain = '.khive/scratch.db'\nblob = { backend = 'fs' }\n",
        ] {
            assert_unknown_destination_key(content, "main");
        }
        assert_unknown_destination_key(
            "[storage]\nblbo = { backend = 'fs', root = '/scratch/blobs' }\n",
            "blbo",
        );
    }

    #[test]
    fn test_unknown_write_destination_backend_keys_are_refused() {
        assert_unknown_destination_key(
            "[[backends]]\nname = 'main'\npaht = '/scratch/store.db'\n",
            "paht",
        );
    }

    #[test]
    fn test_unknown_write_destination_exec_keys_are_refused() {
        assert_unknown_destination_key("[exec]\nrooot = '/scratch/exec'\n", "rooot");
    }

    #[test]
    fn test_valid_write_destinations_load_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let empty_storage = write_toml(&dir, "[storage]\n");
        assert!(KhiveConfig::load(Some(&empty_storage))
            .unwrap()
            .unwrap()
            .storage
            .blob
            .is_none());

        let path = write_toml(
            &dir,
            r#"
future_top_level_key = true
[[backends]]
name = "main"
kind = "sqlite"
path = "/scratch/store.db"
read_only = false
[storage]
blob = { backend = "fs", root = "/scratch/blobs", floor_bytes = 4096 }
[exec]
root = "/scratch/exec"
read_roots = ["/scratch/tools"]
keep = true
[exec.limits]
cpu_seconds = 60
file_size = 1048576
"#,
        );
        let cfg = KhiveConfig::load(Some(&path)).unwrap().unwrap();
        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(
            cfg.backends[0].path.as_deref(),
            Some(Path::new("/scratch/store.db"))
        );
        assert!(!cfg.backends[0].read_only);
        match cfg.storage.blob {
            Some(BlobConfig::Fs { root, floor_bytes }) => {
                assert_eq!(root.as_deref(), Some("/scratch/blobs"));
                assert_eq!(floor_bytes, Some(4096));
            }
            other => panic!("expected the configured filesystem store, got {other:?}"),
        }
        assert_eq!(cfg.exec.root.as_deref(), Some("/scratch/exec"));
        assert_eq!(cfg.exec.read_roots, ["/scratch/tools"]);
        assert!(cfg.exec.keep);
        assert_eq!(cfg.exec.limits.cpu_seconds, Some(60));
        assert_eq!(cfg.exec.limits.file_size, Some(1048576));
    }

    // No [storage] section at all -> fs default, existing configurations
    // keep behaving exactly as they did before this section existed.
    #[test]
    fn test_no_storage_section_defaults_to_fs() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, "# no storage section\n");
        let cfg = KhiveConfig::load(Some(&path))
            .expect("no error")
            .expect("file found");
        assert!(cfg.storage.blob.is_none());
    }

    #[test]
    fn test_storage_blob_fs_selection_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[storage.blob]
backend = "fs"
root = "/var/lib/khive/blobs"
floor_bytes = 100000000000
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("no error")
            .expect("file found");
        match cfg.storage.blob {
            Some(BlobConfig::Fs { root, floor_bytes }) => {
                assert_eq!(root.as_deref(), Some("/var/lib/khive/blobs"));
                assert_eq!(floor_bytes, Some(100_000_000_000));
            }
            other => panic!("expected BlobConfig::Fs, got {other:?}"),
        }
    }

    #[test]
    fn test_storage_blob_s3_selection_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[storage.blob]
backend = "s3"
bucket = "khive-blobs"
region = "us-east-1"
endpoint = "https://objects.example.invalid"
prefix = "blobs"
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("no error")
            .expect("file found");
        match cfg.storage.blob {
            Some(BlobConfig::S3 {
                bucket,
                region,
                endpoint,
                prefix,
                allow_http,
            }) => {
                assert_eq!(bucket, "khive-blobs");
                assert_eq!(region, "us-east-1");
                assert_eq!(endpoint.as_deref(), Some("https://objects.example.invalid"));
                assert_eq!(prefix.as_deref(), Some("blobs"));
                assert_eq!(allow_http, None);
            }
            other => panic!("expected BlobConfig::S3, got {other:?}"),
        }
    }

    // An unknown field under [storage.blob] must be a startup error, not
    // silently ignored -- unlike the rest of KhiveConfig, this section is
    // strict (deny_unknown_fields).
    #[test]
    fn test_storage_blob_unknown_field_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[storage.blob]
backend = "fs"
made_up_field = "x"
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("unknown field must be rejected");
        assert!(
            matches!(config_error_root(&err), ConfigError::Parse { .. }),
            "got {err:?}"
        );
    }

    // An s3-only field (bucket) under backend = "fs" must be rejected: the
    // internally tagged enum's Fs variant doesn't declare it, so it is an
    // unknown field for that variant.
    #[test]
    fn test_storage_blob_other_backend_field_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[storage.blob]
backend = "fs"
bucket = "khive-blobs"
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("s3 field under fs must be rejected");
        assert!(
            matches!(config_error_root(&err), ConfigError::Parse { .. }),
            "got {err:?}"
        );
    }

    // Credentials are never accepted in TOML (ADR-111 Amendment 2): an
    // access-key field under backend = "s3" is unknown to that variant and
    // must be rejected, the same way an other-backend field is.
    #[test]
    fn test_storage_blob_credential_field_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[storage.blob]
backend = "s3"
bucket = "khive-blobs"
region = "us-east-1"
access_key_id = "AKIAEXAMPLE"
"#,
        );
        let err = KhiveConfig::load(Some(&path))
            .expect_err("a credential field in TOML must be rejected");
        assert!(
            matches!(config_error_root(&err), ConfigError::Parse { .. }),
            "got {err:?}"
        );
    }

    // An unrecognized backend value is rejected by the internally tagged
    // enum's own tag matching, same mechanism as an unknown field.
    #[test]
    fn test_storage_blob_unknown_backend_value_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[storage.blob]
backend = "gcs"
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("unknown backend must be rejected");
        assert!(
            matches!(config_error_root(&err), ConfigError::Parse { .. }),
            "got {err:?}"
        );
    }
}
