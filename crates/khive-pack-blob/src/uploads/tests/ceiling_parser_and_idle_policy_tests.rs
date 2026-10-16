#[test]
fn upload_ceiling_parser_accepts_positive_usize_and_rejects_invalid_values() {
    assert_eq!(UploadPolicy::positive_limit("1"), Some(1));
    assert_eq!(UploadPolicy::positive_limit("128"), Some(128));
    assert_eq!(
        UploadPolicy::positive_limit(&usize::MAX.to_string()),
        Some(usize::MAX)
    );
    for invalid in ["", "0", "-1", "1.5", " 16", "sixteen"] {
        assert_eq!(UploadPolicy::positive_limit(invalid), None, "{invalid:?}");
    }
    assert_eq!(
        UploadPolicy::positive_limit(&format!("{}0", usize::MAX)),
        None
    );
}

#[tokio::test]
async fn filesystem_idle_clamp_precedes_instant_fallback_and_preserves_non_fs_policy() {
    if khive_storage::test_support::run_exact_test_in_child("KHIVE_IDLE_CLAMP_CHILD", false, |_| {})
    {
        return;
    }
    let log = tempfile::NamedTempFile::new().unwrap();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(Arc::new(log.reopen().unwrap()))
            .without_time()
            .with_ansi(false)
            .finish(),
    )
    .unwrap();
    let f = fixture();
    for (raw, expected) in [
        ("61", 61),
        ("43200", 21600),
        ("18446744073709551615", 21600),
    ] {
        std::env::set_var("KHIVE_BLOB_UPLOAD_IDLE_SECS", raw);
        let manager = UploadManager::new(f.manager.runtime.clone());
        let response = manager.begin(0, None, "uploader:a".into()).await;
        assert!(
            response.is_ok(),
            "{raw}: FS begin must succeed: {response:?}"
        );
        let id = upload_id(&response.unwrap());
        assert_eq!(manager.policy.idle_for.as_secs(), expected);
        assert_eq!(lease(&f.root, &id)["idle_secs"], expected);
        manager.abort(&id).await.unwrap();
    }
    std::env::set_var("KHIVE_BLOB_UPLOAD_IDLE_SECS", "43200");
    assert_eq!(UploadPolicy::from_env(None).idle_for.as_secs(), 43200);
    let text = std::fs::read_to_string(log.path()).unwrap();
    assert!(text.contains("KHIVE_BLOB_UPLOAD_IDLE_SECS") && text.contains("configured=43200"));
    assert!(text.contains("configured=18446744073709551615") && text.contains("clamp=21600"));
    let warnings: Vec<_> = text
        .lines()
        .filter(|line| line.contains("configured=43200"))
        .collect();
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].contains("WARN")
            && warnings[0].contains("KHIVE_BLOB_UPLOAD_IDLE_SECS")
            && warnings[0].contains("clamp=21600")
    );
}
