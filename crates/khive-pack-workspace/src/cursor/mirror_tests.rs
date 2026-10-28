async fn mirror_deleted_head_case(versioned: bool, changed: bool) {
    let tree = MirrorTree::new();
    let bad = ".khive/notes/a-deleted.md";
    let good = ".khive/notes/b-good.md";
    tree.write(bad, "first deleted file version");
    tree.write(good, "first healthy file version");
    let (runtime, registry, token) = mirror_fixture();
    let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
    let first =
        crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
            .await
            .expect("two-file positive baseline");
    assert_eq!(
        first.committed, 2,
        "positive baseline must mirror both files"
    );
    let (bad_uri, bad_key) = mirror_slot(&tree, bad, &token);
    let (good_uri, good_key) = mirror_slot(&tree, good, &token);
    let initial = mirror_documents(&runtime, &token).await;
    let initial_bad = initial
        .iter()
        .find(|doc| doc["properties"]["source_uri"].as_str() == Some(bad_uri.as_str()))
        .expect("positive baseline must have a document for the deleted file")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let head = if versioned {
        tree.write(bad, "second deleted file version");
        let second =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect("positive versioned baseline");
        assert_eq!(
            second.committed, 1,
            "must seed a new head before deleting it"
        );
        let docs = mirror_documents(&runtime, &token).await;
        docs.iter()
            .find(|doc| {
                doc["properties"]["source_uri"].as_str() == Some(bad_uri.as_str())
                    && doc["id"].as_str() != Some(initial_bad.as_str())
            })
            .expect("second live version must exist")["id"]
            .as_str()
            .unwrap()
            .to_owned()
    } else {
        initial_bad
    };
    let before_deleted_cursor = read(&runtime, &bad_key)
        .await
        .expect("deleted file cursor must exist");
    let before_good_cursor = read(&runtime, &good_key)
        .await
        .expect("healthy file cursor must exist");
    registry
        .dispatch("delete", serde_json::json!({"id":head.clone()}))
        .await
        .expect("soft-delete the existing document through the public verb");
    let deleted = mirror_documents(&runtime, &token).await;
    assert!(
        !deleted
            .iter()
            .any(|doc| doc["id"].as_str() == Some(head.as_str())),
        "delete fixture must remove the live head"
    );
    let old_bad_count = deleted
        .iter()
        .filter(|doc| doc["properties"]["source_uri"].as_str() == Some(bad_uri.as_str()))
        .count();
    assert_eq!(
        old_bad_count,
        if versioned { 1 } else { 0 },
        "deleted head fixture must have the expected surviving history"
    );
    let before_edges = mirror_edges(&runtime, &token).await;
    assert_eq!(
        before_edges.len(),
        if versioned { 1 } else { 0 },
        "supersedes edge must survive the deletion"
    );
    if changed {
        tree.write(bad, "changed content after deletion");
    }
    tree.write(good, "second healthy file version");
    let summary =
        crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
            .await
            .expect("deleted head must be skipped while healthy file commits");
    assert_eq!(
        summary.skipped_no_current,
        vec![std::path::PathBuf::from(bad)]
    );
    assert_eq!(
        summary.committed, 1,
        "healthy file must commit after deleted-head skip"
    );
    assert_eq!(
        read(&runtime, &bad_key).await,
        Some(before_deleted_cursor),
        "deleted file cursor must not advance"
    );
    assert_ne!(
        read(&runtime, &good_key).await,
        Some(before_good_cursor),
        "healthy file cursor must advance"
    );
    let after = mirror_documents(&runtime, &token).await;
    assert_eq!(
        after
            .iter()
            .filter(|doc| doc["properties"]["source_uri"].as_str() == Some(bad_uri.as_str()))
            .count(),
        old_bad_count,
        "deleted slot must not acquire a new first version"
    );
    assert_eq!(
        after
            .iter()
            .filter(|doc| doc["properties"]["source_uri"].as_str() == Some(good_uri.as_str()))
            .count(),
        2,
        "healthy slot must have both document versions"
    );
}

#[tokio::test]
async fn mirror_deleted_versioned_head_skips_and_continues() {
    const NAME: &str = "mirror_deleted_versioned_head_skips_and_continues";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(mirror_deleted_head_case(true, false)).await;
    mirror_done(NAME);
}

#[tokio::test]
async fn mirror_deleted_single_matching_hash_skips_and_continues() {
    const NAME: &str = "mirror_deleted_single_matching_hash_skips_and_continues";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(mirror_deleted_head_case(false, false)).await;
    mirror_done(NAME);
}

#[tokio::test]
async fn mirror_deleted_single_changed_hash_skips_without_recreation() {
    const NAME: &str = "mirror_deleted_single_changed_hash_skips_without_recreation";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(mirror_deleted_head_case(false, true)).await;
    mirror_done(NAME);
}

#[tokio::test]
async fn mirror_credential_path_refusal_is_masked_and_other_file_commits() {
    const NAME: &str = "mirror_credential_path_refusal_is_masked_and_other_file_commits";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        const SECRET: &str = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
        let tree = MirrorTree::new();
        let (runtime, registry, token) = mirror_fixture();
        let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
        tree.write(".khive/notes/baseline.md", "healthy baseline");
        assert_eq!(
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect("healthy write before refusal")
                .committed,
            1
        );
        let refused = format!(".khive/notes/{SECRET}.md");
        let (uri, key) = mirror_slot(&tree, &refused, &token);
        let hash = khive_types::Hash32::from_blake3(b"safe body").to_string();
        let direct = runtime
            .create_workspace_document_with_cursor(
                &registry,
                &token,
                (&refused, "safe body"),
                (&uri, "text/markdown", &hash),
                None,
                mirror_no_cursor,
            )
            .await;
        let observed = match &direct {
            Ok(_) => "Ok".to_owned(),
            Err(khive_runtime::RuntimeError::CanonicalAdmission { op_index, source }) => {
                format!(
                    "CanonicalAdmission(op_index={op_index}, source={:?})",
                    source.channel_ingest_failure_class()
                )
            }
            Err(error) => format!("OtherOuter({:?})", error.channel_ingest_failure_class()),
        };
        assert!(
            matches!(
                &direct,
                Err(khive_runtime::RuntimeError::CanonicalAdmission { op_index: 0, source })
                    if matches!(source.as_ref(), khive_runtime::RuntimeError::SecretDetected(_))
            ),
            "fixture must prove the exact outer pre-unit admission refusal; observed {observed}"
        );
        assert!(
            read(&runtime, &key).await.is_none(),
            "direct refusal must not write cursor"
        );
        tree.write(&refused, "safe body");
        tree.write(".khive/notes/z-healthy.md", "healthy second file");
        let summary =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect("refused path must not abort the pass");
        assert_eq!(summary.committed, 1, "other new file must commit");
        assert_eq!(summary.refused.len(), 1);
        assert_eq!(summary.refused[0].1, "SecretDetected");
        let shown = summary.refused[0].0.to_string_lossy();
        assert!(
            !shown.contains(SECRET),
            "refused diagnostic leaked raw path credential"
        );
        assert_ne!(
            shown.to_string(),
            refused,
            "refused path must actually be masked"
        );
        assert_eq!(
            shown,
            khive_runtime::secret_gate::mask_for_redaction_surface(
                khive_runtime::secret_gate::RedactionSurface::WorkspaceMirror,
                &refused,
            ),
            "refused diagnostic must use the canonical whole-path mask"
        );
        assert!(
            read(&runtime, &key).await.is_none(),
            "refused file acquired cursor"
        );
        let docs = mirror_documents(&runtime, &token).await;
        let edges = mirror_edges(&runtime, &token).await;
        let cursor_rows = rows(&runtime).await;
        assert_eq!(docs.len(), 2, "only the two healthy documents may commit");
        assert_eq!(
            cursor_rows.len(),
            2,
            "only the two healthy cursors may commit"
        );
        assert!(
            !format!("{docs:?}{edges:?}{cursor_rows:?}{summary:?}").contains(SECRET),
            "raw credential leaked through domain rows or pass summary"
        );
        assert_eq!(
            mirror_lexical_rows(&runtime).await,
            (2, 2),
            "only healthy lexical rows may exist"
        );
    })
    .await;
    mirror_done(NAME);
}

#[cfg(unix)]
async fn mirror_same_inode_swap_case(mode: u8) {
    use std::os::unix::fs::symlink;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let tree = MirrorTree::new();
    let outside = MirrorTree::new();
    let (target, target_path) = match mode {
        0 => (
            ".khive/notes/a-file.md",
            tree.write(
                ".khive/notes/a-file.md",
                "outside marker must never be stored",
            ),
        ),
        1 => {
            let path = tree.write(
                ".khive/notes/a-dir/injected.md",
                "outside marker must never be stored",
            );
            (".khive/notes/a-dir", path.parent().unwrap().to_path_buf())
        }
        2 => {
            let path = tree.write(
                ".khive/notes/a-ancestor/deeper/injected.md",
                "outside marker must never be stored",
            );
            (
                ".khive/notes/a-ancestor",
                path.parent().unwrap().parent().unwrap().to_path_buf(),
            )
        }
        _ => panic!("unknown swap case"),
    };
    tree.write(".khive/notes/z-safe.md", "safe regular file");
    let canonical_target = std::fs::canonicalize(&target_path).unwrap();
    let moved = outside.0.join("moved-original");
    let fired = Arc::new(AtomicBool::new(false));
    let fired_hook = Arc::clone(&fired);
    let mut hook = move |path: &std::path::Path| {
        if path == canonical_target.as_path() && !fired_hook.swap(true, Ordering::SeqCst) {
            std::fs::rename(path, &moved).expect("move original inode outside root");
            symlink(&moved, path).expect("replace original name with symlink to same inode");
        }
    };
    let (runtime, registry, token) = mirror_fixture();
    let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
    let result = crate::mirror::mirror_directory_once_with_hook(
        &runtime, &registry, &token, &tree.0, &config, &mut hook,
    )
    .await;
    assert!(
        fired.load(Ordering::SeqCst),
        "swap seam must actually execute"
    );
    let summary = result.expect("swapped link must be a per-entry skip");
    assert!(
        summary
            .skipped_entries
            .contains(&std::path::PathBuf::from(target)),
        "swapped entry must be named as skipped"
    );
    assert_eq!(summary.committed, 1, "independent safe file must commit");
    let docs = mirror_documents(&runtime, &token).await;
    assert_eq!(docs.len(), 1);
    assert!(
        !format!("{docs:?}").contains("outside marker must never be stored"),
        "outside file was mirrored"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn mirror_file_swapped_to_same_inode_link_is_skipped() {
    const NAME: &str = "mirror_file_swapped_to_same_inode_link_is_skipped";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(mirror_same_inode_swap_case(0)).await;
    mirror_done(NAME);
}

#[cfg(unix)]
#[tokio::test]
async fn mirror_directory_swapped_to_same_inode_link_is_skipped() {
    const NAME: &str = "mirror_directory_swapped_to_same_inode_link_is_skipped";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(mirror_same_inode_swap_case(1)).await;
    mirror_done(NAME);
}

#[cfg(unix)]
#[tokio::test]
async fn mirror_ancestor_swapped_before_open_is_skipped() {
    const NAME: &str = "mirror_ancestor_swapped_before_open_is_skipped";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(mirror_same_inode_swap_case(2)).await;
    mirror_done(NAME);
}

#[cfg(unix)]
#[tokio::test]
async fn mirror_root_symlink_spellings_never_follow_final_link() {
    use std::os::unix::fs::symlink;
    const NAME: &str = "mirror_root_symlink_spellings_never_follow_final_link";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        let fixture = MirrorTree::new();
        let real = fixture.0.join("real");
        std::fs::create_dir_all(real.join(".khive/notes")).unwrap();
        std::fs::write(real.join(".khive/notes/safe.md"), "root positive baseline").unwrap();
        let link = fixture.0.join("link");
        symlink(&real, &link).unwrap();
        let paths = [
            link.clone(),
            std::path::PathBuf::from(format!("{}/", link.display())),
            link.join("."),
            std::path::PathBuf::from(format!("{}/", real.display())),
            real.join("."),
            fixture.0.join(".").join("real"),
            real.join("..").join("real"),
        ];
        let (runtime, registry, token) = mirror_fixture();
        let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
        let baseline =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &real, &config)
                .await
                .expect("ordinary root spelling positive baseline");
        assert_eq!(baseline.committed, 1);
        let before_documents = mirror_documents(&runtime, &token).await;
        let before_rows = rows(&runtime).await;
        assert!(
            crate::mirror::validate_root_spelling_for_test(&real).is_ok(),
            "ordinary root spelling must pass the pure spelling guard"
        );
        assert!(
            crate::mirror::validate_root_spelling_for_test(std::path::Path::new("/")).is_err(),
            "filesystem root must be rejected without opening or traversing it"
        );
        for path in paths {
            assert!(
                crate::mirror::mirror_directory_once(
                    &runtime, &registry, &token, &path, &config
                )
                .await
                .is_err(),
                "invalid root spelling or final-component link must be refused"
            );
            assert_eq!(mirror_documents(&runtime, &token).await, before_documents);
            assert_eq!(rows(&runtime).await, before_rows);
        }
    })
    .await;
    mirror_done(NAME);
}

#[cfg(unix)]
#[tokio::test]
async fn mirror_root_swap_after_pin_fails_before_reads() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    const NAME: &str = "mirror_root_swap_after_pin_fails_before_reads";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        let fixture = MirrorTree::new();
        let baseline = fixture.0.join("baseline");
        std::fs::create_dir_all(baseline.join(".khive/notes")).unwrap();
        std::fs::write(
            baseline.join(".khive/notes/baseline.md"),
            "positive root baseline",
        )
        .unwrap();
        let root = fixture.0.join("root");
        std::fs::create_dir_all(root.join(".khive/notes")).unwrap();
        std::fs::write(root.join(".khive/notes/pinned.md"), "pinned original body").unwrap();
        let (runtime, registry, token) = mirror_fixture();
        let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
        let first = crate::mirror::mirror_directory_once(
            &runtime, &registry, &token, &baseline, &config,
        )
        .await
        .expect("ordinary root must mirror before root swap test");
        assert_eq!(first.committed, 1);
        let before_documents = mirror_documents(&runtime, &token).await;
        let before_cursors = rows(&runtime).await;
        let before_edges = mirror_edges(&runtime, &token).await;
        let before_lexical = mirror_lexical_rows(&runtime).await;
        let original = root.clone();
        let moved = fixture.0.join("pinned-original");
        let fired = Arc::new(AtomicBool::new(false));
        let fired_hook = Arc::clone(&fired);
        let descendant_open = Arc::new(AtomicBool::new(false));
        let descendant_open_hook = Arc::clone(&descendant_open);
        let mut hook = move |path: &std::path::Path| {
            if path == original.as_path() && !fired_hook.swap(true, Ordering::SeqCst) {
                std::fs::rename(&original, &moved).expect("move pinned root after open");
                std::fs::create_dir_all(original.join(".khive/notes")).unwrap();
                std::fs::write(
                    original.join(".khive/notes/replacement.md"),
                    "replacement root body",
                )
                .unwrap();
            } else {
                descendant_open_hook.store(true, Ordering::SeqCst);
            }
        };
        let result = crate::mirror::mirror_directory_once_with_hook(
            &runtime, &registry, &token, &root, &config, &mut hook,
        )
        .await;
        assert!(
            fired.load(Ordering::SeqCst),
            "root-pin seam must actually execute"
        );
        assert!(
            !descendant_open.load(Ordering::SeqCst),
            "root mismatch must abort before descendant opens"
        );
        let error = result.expect_err("mismatched root label identity must fail before reads");
        assert_eq!(
            error.commit_state,
            crate::mirror::FileCommitState::NotAttempted
        );
        assert_eq!(error.progress.committed, 0);
        assert_eq!(mirror_documents(&runtime, &token).await, before_documents);
        assert_eq!(rows(&runtime).await, before_cursors);
        assert_eq!(mirror_edges(&runtime, &token).await, before_edges);
        assert_eq!(mirror_lexical_rows(&runtime).await, before_lexical);
    })
    .await;
    mirror_done(NAME);
}

#[cfg(windows)]
#[test]
fn mirror_windows_root_spelling_guard_is_pure() {
    use std::path::Path;

    assert!(
        crate::mirror::validate_root_spelling_for_test(Path::new(r"C:\safe\project")).is_ok(),
        "ordinary drive-rooted path must pass the pure spelling guard"
    );
    for path in [
        r"C:\safe\project\",
        r"C:\safe\project\.",
        r"C:\safe\.\project",
        r"C:\safe\..\project",
        r"C:\",
        "C:/safe/project/",
    ] {
        assert!(crate::mirror::validate_root_spelling_for_test(Path::new(path)).is_err(),
            "ambiguous or root-only Windows spelling must be refused without filesystem I/O: {path:?}");
    }
}

async fn mirror_ambiguous_once(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &khive_runtime::NamespaceToken,
    tree: &MirrorTree,
    config: &crate::WorkspaceMirrorConfig,
) -> crate::mirror::MirrorPassSummary {
    crate::mirror::mirror_directory_once(runtime, registry, token, &tree.0, config)
        .await
        .expect("ambiguous head is a per-file skip")
}

#[tokio::test]
async fn mirror_non_utf8_diagnostic_masks_credential_path() {
    const NAME: &str = "mirror_non_utf8_diagnostic_masks_credential_path";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        const SECRET: &str = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
        let tree = MirrorTree::new();
        let relative = format!(".khive/notes/{SECRET}.md");
        tree.write(&relative, [0xff, 0xfe]);
        tree.write(".khive/notes/safe.md", "safe utf8 file");
        let (runtime, registry, token) = mirror_fixture();
        let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
        let summary =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect("non-UTF8 file must be skipped");
        assert_eq!(summary.committed, 1, "healthy file must commit");
        assert_eq!(summary.skipped_non_utf8.len(), 1);
        let shown = summary.skipped_non_utf8[0].to_string_lossy();
        assert!(
            !shown.contains(SECRET),
            "non-UTF8 diagnostic leaked credential path"
        );
        assert_ne!(
            shown.to_string(),
            relative,
            "non-UTF8 diagnostic must be masked"
        );
        assert_eq!(
            shown,
            khive_runtime::secret_gate::mask_for_redaction_surface(
                khive_runtime::secret_gate::RedactionSurface::WorkspaceMirror,
                &relative,
            ),
            "non-UTF8 diagnostic must use the canonical whole-path mask"
        );
        assert!(!format!("{summary:?}").contains(SECRET));
    })
    .await;
    mirror_done(NAME);
}

#[cfg(unix)]
#[tokio::test]
async fn mirror_link_diagnostic_masks_credential_path() {
    use std::os::unix::fs::symlink;
    const NAME: &str = "mirror_link_diagnostic_masks_credential_path";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        const SECRET: &str = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
        let tree = MirrorTree::new();
        let outside = MirrorTree::new();
        let outside_path = outside.write("outside.md", "outside secret-path link body");
        let relative = format!(".khive/notes/{SECRET}.md");
        let link = tree.0.join(&relative);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&outside_path, &link).unwrap();
        tree.write(".khive/notes/safe.md", "safe utf8 file");
        let (runtime, registry, token) = mirror_fixture();
        let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
        let summary =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect("symlink must be skipped");
        assert_eq!(summary.committed, 1, "healthy file must commit");
        assert_eq!(summary.skipped_entries.len(), 1);
        let shown = summary.skipped_entries[0].to_string_lossy();
        assert!(
            !shown.contains(SECRET),
            "link diagnostic leaked credential path"
        );
        assert_ne!(
            shown.to_string(),
            relative,
            "link diagnostic must be masked"
        );
        assert_eq!(
            shown,
            khive_runtime::secret_gate::mask_for_redaction_surface(
                khive_runtime::secret_gate::RedactionSurface::WorkspaceMirror,
                &relative,
            ),
            "link diagnostic must use the canonical whole-path mask"
        );
        assert!(!format!("{summary:?}").contains(SECRET));
        assert!(!format!("{:?}", mirror_documents(&runtime, &token).await)
            .contains("outside secret-path link body"));
    })
    .await;
    mirror_done(NAME);
}

#[tokio::test]
async fn mirror_no_current_diagnostic_masks_credential_path() {
    const NAME: &str = "mirror_no_current_diagnostic_masks_credential_path";
    if mirror_child(NAME) {
        return;
    }
    mirror_bounded(async {
        const SECRET: &str = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
        let tree = MirrorTree::new();
        let (runtime, registry, token) = mirror_fixture();
        let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
        tree.write(".khive/notes/safe.md", "safe first version");
        let first =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect("positive healthy baseline");
        assert_eq!(first.committed, 1);
        let relative = format!(".khive/notes/{SECRET}.md");
        tree.write(&relative, "unadmitted body");
        let (_, key) = mirror_slot(&tree, &relative, &token);
        put(
            &runtime,
            &key,
            Some(1),
            Some("fixture-previous-hash"),
            Some(2),
        )
        .await;
        let before = read(&runtime, &key).await.expect("seeded prior cursor");
        assert!(mirror_documents(&runtime, &token)
            .await
            .iter()
            .all(|doc| doc["name"].as_str() != Some(relative.as_str())));
        tree.write(".khive/notes/safe.md", "safe second version");
        let summary =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect("no-current slot must be skipped");
        assert_eq!(summary.committed, 1, "independent healthy file must commit");
        assert_eq!(summary.skipped_no_current.len(), 1);
        let shown = summary.skipped_no_current[0].to_string_lossy();
        assert!(
            !shown.contains(SECRET),
            "no-current diagnostic leaked credential path"
        );
        assert_ne!(
            shown.to_string(),
            relative,
            "no-current diagnostic must be masked"
        );
        assert_eq!(
            shown,
            khive_runtime::secret_gate::mask_for_redaction_surface(
                khive_runtime::secret_gate::RedactionSurface::WorkspaceMirror,
                &relative,
            ),
            "no-current diagnostic must use the canonical whole-path mask"
        );
        assert!(!format!("{summary:?}").contains(SECRET));
        assert_eq!(
            read(&runtime, &key).await,
            Some(before),
            "skipped slot cursor must stay fixed"
        );
    })
    .await;
    mirror_done(NAME);
}
