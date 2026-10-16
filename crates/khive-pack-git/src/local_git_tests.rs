use super::*;

#[cfg(unix)]
fn config_failure_program(directory: &Path, code: i32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let program = directory.join("fake-git");
    let body = format!(
            "#!/bin/sh\nfor arg do\n if [ \"$arg\" = config ]; then exit {code}; fi\ndone\nwhile [ \"$1\" != -C ]; do shift; done\nshift\nprintf executed > \"$1/filter-ran\"\n"
        );
    std::fs::write(&program, body).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    program
}

#[cfg(unix)]
#[test]
fn failed_filter_enumeration_never_starts_git_add() {
    for status in [2, 23] {
        let temp = tempfile::tempdir().unwrap();
        let program = config_failure_program(temp.path(), status);
        let error = run_git(&program, temp.path(), &["add", "--all"], None, None, false)
            .expect_err("failed hardening enumeration must refuse before add");
        assert_eq!(error.code(), "git_config");
        assert!(!temp.path().join("filter-ran").exists());
    }
}

#[cfg(unix)]
#[test]
fn filter_enumeration_no_matching_keys_still_allows_command() {
    let temp = tempfile::tempdir().unwrap();
    let program = config_failure_program(temp.path(), 1);
    run_git(&program, temp.path(), &["add", "--all"], None, None, false)
        .expect("config status1 is the supported no-match outcome");
    assert!(temp.path().join("filter-ran").exists());
}

#[test]
fn filter_enumeration_spawn_failure_is_authored_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let error = git_command(&temp.path().join("absent-git"), temp.path(), &["add"], None)
        .expect_err("enumeration spawn must fail closed");
    assert_eq!(error.code(), "git_config");
    assert_eq!(
        error.to_string(),
        "git_config: could not enumerate repository filters"
    );
}

#[cfg(unix)]
#[test]
fn enumeration_failure_refuses_real_configured_clean_filter() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        vec!["init", "--quiet", "--template="],
        vec!["config", "filter.fixture.clean", "sh .fixture-clean"],
    ] {
        assert!(base_command(Path::new("git"))
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(repo.join(".gitattributes"), "*.txt filter=fixture\n").unwrap();
    std::fs::write(repo.join("input.txt"), "input\n").unwrap();
    std::fs::write(
        repo.join(".fixture-clean"),
        "printf executed > .filter-ran\ncat\n",
    )
    .unwrap();
    let wrapper = temp.path().join("git-wrapper");
    std::fs::write(
            &wrapper,
            "#!/bin/sh\nfor arg do\n if [ \"$arg\" = config ]; then exit 23; fi\ndone\nexec git \"$@\"\n",
        ).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = run_git(
        &wrapper,
        &repo,
        &["add", "--", "input.txt"],
        None,
        None,
        false,
    )
    .expect_err("failed enumeration must prevent the configured clean program");
    assert_eq!(error.code(), "git_config");
    assert!(
        !repo.join(".filter-ran").exists(),
        "repository clean filter ran"
    );
    assert!(
        !repo.join(".git/index").exists(),
        "no add should have started"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn checkout_and_tree_write_enumerate_filters_once_per_operation() {
    use std::os::unix::fs::PermissionsExt;
    for population in [1, 16, 64] {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let native = Path::new("git");
        assert!(base_command(native)
            .arg("-C")
            .arg(&repo)
            .args(["init", "--quiet", "--template="])
            .status()
            .unwrap()
            .success());
        for index in 0..population {
            std::fs::write(
                repo.join(format!("file-{index:04}.txt")),
                format!("content {index}\n"),
            )
            .unwrap();
        }
        for args in [
            vec!["add", "--all"],
            vec![
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        ] {
            assert!(base_command(native)
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        let expected = base_command(native)
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD^{tree}"])
            .output()
            .unwrap();
        assert!(expected.status.success());
        let expected_tree = oid_output(&expected.stdout).unwrap();
        let wrapper = temp.path().join("git-trace-wrapper");
        std::fs::write(&wrapper,
                "#!/bin/sh\nrepo=\nnext_repo=0\nconfig=0\nfor arg do\n if [ \"$next_repo\" = 1 ]; then repo=\"$arg\"; next_repo=0; fi\n if [ \"$arg\" = -C ]; then next_repo=1; fi\n if [ \"$arg\" = config ]; then config=1; fi\ndone\nif [ \"$config\" = 1 ]; then printf 'config\\n' >> \"$repo/config-count\"; fi\nexec git \"$@\"\n"
            ).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = khive_runtime::RuntimeConfig::no_embeddings();
        config.db_path = Some(temp.path().join("runtime.db"));
        config.git_write.program = Some(wrapper);
        let runtime = KhiveRuntime::new(config).unwrap();
        runtime
            .install_blob_store(std::sync::Arc::new(
                khive_db::stores::blob::FsBlobStore::new(temp.path().join("blobs"), 0).unwrap(),
            ))
            .unwrap();
        let checked_out = checkout(&runtime, &repo, "HEAD").await.unwrap();
        let entries = tree::load(&runtime, &checked_out.tree).await.unwrap();
        assert_eq!(entries.len(), population);
        assert_eq!(
            std::fs::read_to_string(repo.join("config-count"))
                .unwrap()
                .lines()
                .count(),
            1,
            "checkout actual filter enumeration scaled with {population} files"
        );
        std::fs::write(repo.join("config-count"), "").unwrap();
        let written = write_manifest_tree(&runtime, &repo, &checked_out.tree)
            .await
            .unwrap();
        assert_eq!(
            written, expected_tree,
            "all original bytes and paths must survive"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("config-count"))
                .unwrap()
                .lines()
                .count(),
            1,
            "tree writer actual filter enumeration scaled with {population} files"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn manifest_roundtrip_preserves_git_symlink_modes_and_target_bytes() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().expect("fixture directory");
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).expect("repo directory");
    run_async(
        Path::new("git"),
        &repo,
        &["init", "-q", "--template="],
        None,
    )
    .await
    .expect("initialize fixture");
    std::fs::write(repo.join("file.txt"), b"file contents\n").expect("file");
    std::fs::create_dir(repo.join("dir")).expect("directory");
    std::fs::write(repo.join("dir/nested.txt"), b"nested contents\n").expect("nested file");
    for (path, target) in [
        ("file-link", b"./file.txt".as_slice()),
        ("directory-link", b"./dir".as_slice()),
        ("dangling-link", b"../missing-\xff".as_slice()),
    ] {
        symlink(OsStr::from_bytes(target), repo.join(path)).expect("symlink");
    }
    run_async(Path::new("git"), &repo, &["add", "--all"], None)
        .await
        .expect("index fixture");
    let original = oid_output(
        &run_async(Path::new("git"), &repo, &["write-tree"], None)
            .await
            .expect("original tree"),
    )
    .expect("tree object id");

    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: Some(dir.path().join("runtime.db")),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("runtime");
    let blobs = khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0)
        .expect("fixture blob store");
    rt.install_blob_store(std::sync::Arc::new(blobs))
        .expect("install blob store");
    let store = tree::blob_store(&rt).expect("blob store");
    let listing = run_async(Path::new("git"), &repo, &["ls-files", "-s", "-z"], None)
        .await
        .expect("index listing");
    let mut entries = Vec::new();
    let mut symlinks = 0;
    for record in listing
        .split(|byte| *byte == 0)
        .filter(|row| !row.is_empty())
    {
        let tab = record.iter().position(|byte| *byte == b'\t').unwrap();
        let metadata = std::str::from_utf8(&record[..tab]).unwrap();
        let fields: Vec<_> = metadata.split_whitespace().collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[2], "0", "fixture must have no unmerged entries");
        let mode = match fields[0] {
            "100644" => 644,
            "100755" => 755,
            "120000" => {
                symlinks += 1;
                120000
            }
            mode => panic!("unexpected index mode {mode}"),
        };
        let path = std::str::from_utf8(&record[tab + 1..]).unwrap();
        let bytes = run_async(
            Path::new("git"),
            &repo,
            &["cat-file", "blob", fields[1]],
            None,
        )
        .await
        .expect("index blob");
        if mode == 120000 {
            assert_eq!(
                bytes,
                std::fs::read_link(repo.join(path))
                    .expect("literal target")
                    .as_os_str()
                    .as_bytes()
            );
        }
        let content = store.put(bytes).await.expect("store index blob");
        entries.push(serde_json::json!({"path":path,"ref":content.as_str(),"mode":mode}));
    }
    assert_eq!(symlinks, 3, "all three symlinks must be indexed as links");
    let manifest = tree::store_from_value(&rt, &serde_json::json!(entries))
        .await
        .expect("store manifest");
    let roundtrip = write_manifest_tree(&rt, &repo, &manifest)
        .await
        .expect("write manifest tree");
    assert_eq!(roundtrip, original);
    let diff = run_async(
        Path::new("git"),
        &repo,
        &[
            "diff-tree",
            "-r",
            "-p",
            "--no-ext-diff",
            &original,
            &roundtrip,
        ],
        None,
    )
    .await
    .expect("roundtrip diff");
    assert!(diff.is_empty(), "roundtrip must have no git differences");
}

#[test]
fn listing_preserves_delimiters_and_refuses_unsupported_modes() {
    let oid = "1234567890123456789012345678901234567890";
    let entries = parse_listing(format!("100755 blob {oid}\ta\tb\nc\0").as_bytes()).unwrap();
    assert_eq!(entries[0].path, "a\tb\nc");
    assert_eq!(entries[0].mode, 755);
    for metadata in ["120000 blob", "160000 commit"] {
        assert_eq!(
            parse_listing(format!("{metadata} {oid}\tx\0").as_bytes())
                .unwrap_err()
                .code(),
            "unsupported_entry"
        );
    }
    assert!(parse_listing(format!("100644 blob {oid}\tx").as_bytes()).is_err());
}

#[test]
fn numstat_counts_binary_files_without_inventing_line_counts() {
    assert_eq!(
        parse_numstat(b"3\t2\ta\n-\t-\timage\n0\t0\tmode\n").unwrap(),
        DiffSummary {
            files: 3,
            additions: 3,
            deletions: 2,
        }
    );
    assert!(parse_numstat(b"3\t-\ta\n").is_err());
    assert!(parse_numstat(b"18446744073709551615\t0\ta\n1\t0\tb\n").is_err());
}

#[test]
fn object_input_is_nul_framed_without_path_reinterpretation() {
    let entry = GitEntry {
        name: "tab\tline\n".into(),
        mode: "100644",
        kind: "blob",
        oid: ZERO_OID.into(),
    };
    assert_eq!(
        mktree_input(&[entry]),
        format!("100644 blob {ZERO_OID}\ttab\tline\n\0").into_bytes()
    );
}

#[test]
fn only_exact_cas_diagnostics_establish_no_effect() {
    let new = "1".repeat(40);
    let expected = "2".repeat(40);
    let actual = "3".repeat(40);
    let argv = [
        "update-ref",
        "--no-deref",
        "--create-reflog",
        "-m",
        "khive-receipt:12345678-1234-4234-8234-123456789012",
        "refs/heads/work",
        &new,
        &expected,
    ];
    let prefix =
        "fatal: update_ref failed for ref 'refs/heads/work': cannot lock ref 'refs/heads/work': ";
    assert!(cas_refused(
        &argv,
        format!("{prefix}is at {actual} but expected {expected}\n").as_bytes()
    ));
    assert!(!cas_refused(
        &argv,
        format!("{prefix}unable to append to HEAD log\n").as_bytes()
    ));
    assert!(!cas_refused(
        &argv,
        format!("warning: config error\n{prefix}is at {actual} but expected {expected}\n")
            .as_bytes()
    ));
    let create = [
        "update-ref",
        "--no-deref",
        "--create-reflog",
        "-m",
        "khive-receipt:12345678-1234-4234-8234-123456789012",
        "refs/heads/work",
        &new,
        ZERO_OID,
    ];
    assert!(cas_refused(
        &create,
        format!("{prefix}reference already exists\n").as_bytes()
    ));
    assert!(!cas_refused(
        &argv,
        format!("{prefix}reference already exists\n").as_bytes()
    ));
}

#[test]
fn reflog_evidence_requires_exact_marker_and_new_sha() {
    let sha = "a".repeat(40);
    let other_sha = "b".repeat(40);
    let marker = receipt_marker("12345678-1234-4234-8234-123456789012").unwrap();
    let record = |oid: &str, message: &str| format!("{oid}\0{message}\0\n").into_bytes();
    assert!(parse_operation_recorded(&record(&sha, &marker), &sha, &marker).unwrap());
    assert!(!parse_operation_recorded(&record(&other_sha, &marker), &sha, &marker).unwrap());
    for message in [
        format!("{marker}-extra"),
        format!("prefix {marker}"),
        "rival".into(),
    ] {
        assert!(!parse_operation_recorded(&record(&sha, &message), &sha, &marker).unwrap());
    }
    let mut history = record(&other_sha, "rival");
    history.extend(record(&sha, &marker));
    assert!(parse_operation_recorded(&history, &sha, &marker).unwrap());
    history.extend_from_slice(b"truncated");
    assert!(parse_operation_recorded(&history, &sha, &marker).is_err());
    assert!(!parse_operation_recorded(b"", &sha, &marker).unwrap());
}

#[test]
fn reflog_evidence_rejects_missing_framing_and_bad_object_ids() {
    let sha = "a".repeat(40);
    let marker = "khive-receipt:12345678-1234-4234-8234-123456789012";
    for invalid in [
        format!("{sha}\0{marker}\0"),
        format!("{sha}\0{marker}\n"),
        format!("short\0{marker}\0\n"),
        format!("{}\0{marker}\0\n", "z".repeat(40)),
    ] {
        assert!(parse_operation_recorded(invalid.as_bytes(), &sha, marker).is_err());
    }
}

#[test]
fn receipt_markers_reject_noncanonical_or_injected_ids() {
    for invalid in [
        "",
        "12345678123442348234123456789012",
        "12345678-1234-4234-8234-12345678901A",
        "12345678-1234-4234-8234-123456789012\nforged",
    ] {
        assert!(receipt_marker(invalid).is_err());
    }
    assert!(receipt_marker("12345678-1234-4234-8234-123456789012").is_ok());
}

#[test]
fn required_compare_never_accepts_empty_or_non_sha() {
    for invalid in ["", "HEAD", "null", "1234", "-1"] {
        assert!(validate_oid(invalid, "expected_head").is_err());
    }
    assert!(validate_oid(ZERO_OID, "expected_head").is_ok());
}

#[test]
fn base_command_argv_and_environment_are_pinned() {
    // The environment carries PATH, which other cases rewrite while holding this guard.
    let _guard = crate::cache::ENV_MUTEX.blocking_lock();
    let command = base_command(Path::new("git"));
    let (args, envs) = git_env::describe(&command);
    assert_eq!(command.get_program(), "git");
    let settings = [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "commit.gpgsign=false",
        "credential.helper=",
        "core.sshCommand=/usr/bin/false",
        "protocol.allow=never",
        "log.showSignature=false",
        "merge.verifySignatures=false",
        "gpg.program=/usr/bin/false",
        "gpg.openpgp.program=/usr/bin/false",
        "gpg.x509.program=/usr/bin/false",
        "gpg.ssh.program=/usr/bin/false",
    ];
    let mut expected_args = Vec::new();
    for setting in settings {
        expected_args.extend(["-c", setting]);
    }
    assert_eq!(args, expected_args);
    let mut expected_envs = BTreeMap::new();
    for (key, value) in [
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("GIT_GRAFT_FILE", ""),
        ("GIT_NO_LAZY_FETCH", "1"),
        ("GIT_NO_REPLACE_OBJECTS", "1"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("LC_ALL", "C"),
    ] {
        expected_envs.insert(key.to_owned(), Some(value.to_owned()));
    }
    if let Some(path) = std::env::var_os("PATH") {
        let path = path.to_string_lossy().into_owned();
        expected_envs.insert("PATH".to_owned(), Some(path));
    }
    assert_eq!(envs, expected_envs);
}
