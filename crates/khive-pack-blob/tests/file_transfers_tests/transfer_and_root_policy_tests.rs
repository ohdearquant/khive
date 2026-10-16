#[tokio::test]
async fn server_file_import_stat_export_round_trip() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, _store) = registry(&roots);
    let bytes: Vec<u8> = (0..200_000).map(|index| (index % 251) as u8).collect();
    std::fs::write(roots.path("imports/original.bin"), &bytes).unwrap();
    let imported = registry
        .dispatch(
            "blob.import",
            json!({"path": "original.bin", "media_type": "application/octet-stream"}),
        )
        .await
        .unwrap();
    assert_eq!(
        imported["content_ref"],
        blake3::hash(&bytes).to_hex().as_str()
    );
    assert_eq!(imported["size"], bytes.len());
    assert_eq!(imported["media_type"], "application/octet-stream");
    assert!(imported.get("bytes").is_none());
    let stat = registry
        .dispatch("blob.stat", json!({"content_ref": imported["content_ref"]}))
        .await
        .unwrap();
    assert_eq!(stat["size"], bytes.len());
    assert_eq!(stat["exists"], true);
    std::fs::write(roots.path("exports/copy.bin"), b"old destination").unwrap();
    let exported = registry
        .dispatch(
            "blob.export",
            json!({"content_ref": imported["content_ref"], "path": "copy.bin"}),
        )
        .await
        .unwrap();
    assert_eq!(exported["size"], bytes.len());
    assert!(exported.get("bytes").is_none());
    assert_eq!(
        std::fs::read(roots.path("exports/copy.bin")).unwrap(),
        bytes
    );
    assert_eq!(
        exported["path"],
        roots.path("exports/copy.bin").to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn import_outside_root_refuses() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, _) = registry(&roots);
    let outside = roots.path("outside.bin");
    std::fs::write(&outside, b"outside file").unwrap();
    import_refusal(&registry, &outside, "escapes the allowed import root").await;
}

#[tokio::test]
async fn missing_outside_import_refuses_before_path_inspection() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, _) = registry(&roots);
    let existing = roots.path("outside.bin");
    let missing = roots.path("missing-outside.bin");
    std::fs::write(&existing, b"outside file").unwrap();
    assert!(!missing.exists());
    let expected = format!(
        "blob.import: import path escapes the allowed import root ({})",
        roots.path("imports").display()
    );
    for outside in [&existing, &missing] {
        let error = registry
            .dispatch("blob.import", json!({"path": outside}))
            .await
            .expect_err("outside paths must refuse before inspection");
        match error {
            khive_runtime::RuntimeError::InvalidInput(text) => assert_eq!(text, expected),
            other => panic!("expected confined-path InvalidInput, got {other}"),
        }
    }
    assert!(!missing.exists());
    assert_eq!(std::fs::read(&existing).unwrap(), b"outside file");
    assert_eq!(std::fs::read_dir(roots.path("cas")).unwrap().count(), 0);
}

#[tokio::test]
async fn import_traversal_directory_and_size_refuse() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, _) = registry(&roots);
    import_refusal(&registry, Path::new("../outside.bin"), "'..' traversal").await;
    import_refusal(&registry, &roots.path("imports"), "regular file").await;
    std::fs::File::create(roots.path("imports/oversize.bin"))
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    import_refusal(&registry, Path::new("oversize.bin"), "maximum").await;
}

#[cfg(unix)]
#[tokio::test]
async fn import_symlinks_inside_and_outside_root_refuse() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, _) = registry(&roots);
    std::fs::write(roots.path("imports/original.bin"), b"inside target").unwrap();
    std::fs::write(roots.path("outside.bin"), b"outside target").unwrap();
    std::os::unix::fs::symlink(
        roots.path("imports/original.bin"),
        roots.path("imports/inside-link"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        roots.path("outside.bin"),
        roots.path("imports/outside-link"),
    )
    .unwrap();
    // An inside-target link isolates symlink validation from canonical containment.
    import_refusal(
        &registry,
        Path::new("inside-link"),
        "must not contain a symlink",
    )
    .await;
    import_refusal(
        &registry,
        Path::new("outside-link"),
        "must not contain a symlink",
    )
    .await;
    std::fs::create_dir_all(roots.path("imports/subdirectory")).unwrap();
    std::fs::write(
        roots.path("imports/subdirectory/nested.bin"),
        b"nested target",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        roots.path("imports/subdirectory"),
        roots.path("imports/directory-link"),
    )
    .unwrap();
    import_refusal(
        &registry,
        Path::new("directory-link/nested.bin"),
        "must not contain a symlink",
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn export_existing_symlink_refuses_without_replacing_target() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, store) = registry(&roots);
    let reference = store.put(b"exported bytes".to_vec()).await.unwrap();
    let target = roots.path("exports/target.bin");
    std::fs::write(&target, b"preserved target").unwrap();
    std::os::unix::fs::symlink(&target, roots.path("exports/link.bin")).unwrap();
    let error = registry
        .dispatch(
            "blob.export",
            json!({"content_ref": reference, "path": "link.bin"}),
        )
        .await
        .expect_err("export symlink refusal");
    assert!(
        error.to_string().contains("must not be a symlink"),
        "{error}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"preserved target");
}

#[tokio::test]
async fn equal_and_nested_file_roots_refuse_both_verbs() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, store) = registry(&roots);
    let reference = store.put(b"root overlap".to_vec()).await.unwrap();
    for (imports, exports) in [
        ("shared", "shared"),
        ("shared/inner", "shared"),
        ("shared", "shared/inner"),
    ] {
        std::env::set_var("KHIVE_IMPORT_FROM_ROOT", roots.path(imports));
        std::env::set_var("KHIVE_SAVE_TO_ROOT", roots.path(exports));
        for (verb, params) in [
            ("blob.import", json!({"path": "input.bin"})),
            (
                "blob.export",
                json!({"content_ref": reference, "path": "output.bin"}),
            ),
        ] {
            let error = registry
                .dispatch(verb, params)
                .await
                .expect_err("equal or nested roots must refuse");
            assert!(
                error.to_string().contains("must not be equal or nested"),
                "{error}"
            );
        }
    }
}
