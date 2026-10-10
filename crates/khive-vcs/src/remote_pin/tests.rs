use super::*;

const OLD: &str = "1111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222";

fn schema() -> String {
    format!(
        "# retain these bytes on a no-op\nformat_version: '2.0.0'\nontology_version: '8.3.1'\nkhive_version: 'old-writer'\ncustom: {{answer: 42, enabled: true}}\nremotes:\n  - name: origin\n    url: ./remote.git\n    commit: '{OLD}'\n    ref: stable\n    pin: sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n    path: nested/kg\n    namespace: research\n  - name: other\n    repo: example/elsewhere\n    commit: '{OLD}'\n"
    )
}

fn fixture(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join(".khive/kg/schema.yaml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    (temp, path)
}

fn origin() -> RemoteName {
    RemoteName::parse("origin").unwrap()
}

fn apply(temp: &tempfile::TempDir, commit: &str) -> Result<RemotePinUpdate> {
    update_remote_pin_with(
        temp.path(),
        &origin(),
        None,
        |_, _, _| Ok(commit.to_owned()),
        |_| Ok(()),
    )
}

fn value(path: &Path) -> Value {
    serde_yaml::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn pending_count(path: &Path) -> usize {
    fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".schema-update-")
        })
        .count()
}

#[test]
fn publication_changes_only_selected_commit_and_preserves_permissions() {
    let (temp, path) = fixture(schema().as_bytes());
    let mut expected = value(&path);
    expected["remotes"][0]["commit"] = Value::String(NEW.to_owned());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
    }
    let report = update_remote_pin_with(
        temp.path(),
        &origin(),
        Some("v2"),
        |source, reference, name| {
            assert_eq!(
                source,
                temp.path()
                    .canonicalize()
                    .unwrap()
                    .join("./remote.git")
                    .as_os_str()
            );
            assert_eq!(reference, "v2");
            assert_eq!(name.as_str(), "origin");
            Ok(NEW.to_owned())
        },
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(report).unwrap(),
        serde_json::json!({
            "remote":"origin", "requested_ref":"v2", "previous_commit":OLD, "commit":NEW, "updated":true
        })
    );
    assert_eq!(value(&path), expected);
    assert_eq!(pending_count(&path), 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}

#[test]
fn equal_commit_is_a_byte_preserving_noop_even_with_uppercase_old_sha() {
    let old = "ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD";
    let source = schema().replace(OLD, old);
    let (temp, path) = fixture(source.as_bytes());
    let report = update_remote_pin_with(
        temp.path(),
        &origin(),
        None,
        |_, _, _| Ok(old.to_ascii_lowercase()),
        |_| panic!("a no-op must not stage or publish schema bytes"),
    )
    .unwrap();
    assert!(!report.updated);
    assert_eq!(report.previous_commit, old);
    assert_eq!(report.commit, old.to_ascii_lowercase());
    assert_eq!(fs::read(path).unwrap(), source.as_bytes());
}

#[test]
fn malformed_or_ambiguous_schema_refuses_before_resolution_or_lock_creation() {
    let baseline = schema();
    let cases = [
        "remotes: [".to_owned(),
        "- not-a-mapping\n".to_owned(),
        format!("{baseline}\n---\n{{}}\n"),
        baseline.replace("'2.0.0'", "'3.0.0'"),
        baseline.replace("'2.0.0'", "'2-bad'"),
        baseline.replace("format_version:", "unknown_version:"),
        baseline.replace("name: other", "name: origin"),
        baseline.replace("name: origin", "name: missing"),
        baseline.replace("name: origin", "name: true"),
        baseline.replace("name: origin", "name: ../origin"),
        baseline.replace("url: ./remote.git", "url: 7"),
        baseline.replace("url: ./remote.git", "url: ''"),
        baseline.replace(
            "url: ./remote.git",
            "url: ./remote.git\n    repo: example/project",
        ),
        baseline.replace("url: ./remote.git", "absent_url: ./remote.git"),
        baseline.replace(OLD, "short"),
        baseline.replace(&format!("commit: '{OLD}'"), "commit: 42"),
        baseline.replace("ref: stable", "ref: false"),
        baseline.replace("ref: stable", "ref: ''"),
        baseline.replace("ref: stable", "ref: 'refs/heads/*'"),
        "format_version: '2.0.0'\nremotes: {}\n".to_owned(),
        "format_version: '2.0.0'\nremotes: [7]\n".to_owned(),
        "format_version: '2.0.0'\nremotes: [{}]\n".to_owned(),
    ];
    for bytes in cases {
        let (temp, path) = fixture(bytes.as_bytes());
        let error = update_remote_pin_with(
            temp.path(),
            &origin(),
            None,
            |_, _, _| panic!("malformed schema reached remote resolution"),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(!error.to_string().is_empty());
        assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
        assert!(!temp.path().join(".khive/state").exists());
        assert_eq!(pending_count(&path), 0);
    }
}

#[test]
fn reference_precedence_and_single_ref_validation_have_no_silent_fallback() {
    let source = schema();
    let (temp, _) = fixture(source.as_bytes());
    let choose = |bytes: &[u8], explicit| select_remote(bytes, temp.path(), &origin(), explicit);
    assert_eq!(
        choose(source.as_bytes(), Some("v2")).unwrap().reference,
        "v2"
    );
    assert_eq!(choose(source.as_bytes(), None).unwrap().reference, "stable");
    assert_eq!(
        choose(source.replace("    ref: stable\n", "").as_bytes(), None)
            .unwrap()
            .reference,
        "HEAD"
    );
    assert!(choose(source.as_bytes(), Some("")).is_err());
    // A malformed configured field remains malformed even with an override.
    assert!(choose(
        source.replace("ref: stable", "ref: false").as_bytes(),
        Some("v2")
    )
    .is_err());
    for reference in [
        "-upload-pack=bad",
        "a:b",
        "refs/*",
        "^main",
        "main^{commit}",
        "main..other",
        "@",
        "a@{1}",
        "a\0b",
        "a\nb",
        "a b",
        "a?b",
        "a[b",
        "a\\b",
        "a~b",
        "trailing.",
        ".hidden",
        "a.lock",
        "a//b",
        "a/",
        "/a",
    ] {
        assert!(
            validate_reference(reference).is_err(),
            "accepted {reference:?}"
        );
    }
    for reference in [
        "HEAD",
        "stable",
        "refs/heads/release/v2",
        "refs/tags/v2",
        "v2.1",
        OLD,
        "résumé",
    ] {
        validate_reference(reference).unwrap();
    }
}

#[test]
fn source_selection_distinguishes_shorthand_urls_and_repo_relative_paths() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    assert_eq!(
        github_source("example/project").unwrap(),
        OsStr::new("https://github.com/example/project.git")
    );
    for repository in [
        "../escape",
        "example/..",
        "owner",
        "owner/name/extra",
        "example/name?x",
        "example/",
        "/name",
    ] {
        assert!(github_source(repository).is_err());
    }
    for url in [
        "https://example.invalid/a.git",
        "ssh://git@example.invalid/a.git",
        "git@example.invalid:a.git",
        "file:///tmp/a.git",
    ] {
        assert_eq!(explicit_source(root, url).unwrap(), OsStr::new(url));
    }
    assert_eq!(
        explicit_source(root, "../remote.git").unwrap(),
        root.join("../remote.git").into_os_string()
    );
    assert_eq!(
        explicit_source(root, "dir with spaces/remote.git").unwrap(),
        root.join("dir with spaces/remote.git").into_os_string()
    );
    for url in [
        "-option",
        "ext::arbitrary command",
        "unknown://host/path",
        "host:",
        "foo\nbar",
        "foo\0bar",
    ] {
        assert!(explicit_source(root, url).is_err());
    }
}

#[test]
fn invalid_resolver_result_and_resolution_failure_keep_original_bytes() {
    for invalid in [
        "",
        "short",
        "11111111111111111111111111111111111111111g",
        "1111111111111111111111111111111111111111\n2222222222222222222222222222222222222222",
    ] {
        let source = schema();
        let (temp, path) = fixture(source.as_bytes());
        assert!(apply(&temp, invalid)
            .unwrap_err()
            .to_string()
            .contains("40-character"));
        assert_eq!(fs::read(path).unwrap(), source.as_bytes());
    }
    let source = schema();
    let (temp, path) = fixture(source.as_bytes());
    let error = update_remote_pin_with(
        temp.path(),
        &origin(),
        None,
        |_, _, _| bail!("remote resolution refused"),
        |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "remote resolution refused");
    assert_eq!(fs::read(path).unwrap(), source.as_bytes());
}

#[test]
fn concurrent_editor_during_resolution_is_not_overwritten() {
    let source = schema();
    let changed = source.replace("answer: 42", "answer: 43");
    let (temp, path) = fixture(source.as_bytes());
    let error = update_remote_pin_with(
        temp.path(),
        &origin(),
        None,
        |_, _, _| {
            fs::write(&path, &changed).unwrap();
            Ok(NEW.to_owned())
        },
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("schema changed"));
    assert_eq!(fs::read(&path).unwrap(), changed.as_bytes());
    assert_eq!(pending_count(&path), 0);
}

#[test]
fn prepublication_failure_preserves_original_and_cleans_unique_pending_file() {
    let source = schema();
    let (temp, path) = fixture(source.as_bytes());
    let error = update_remote_pin_with(
        temp.path(),
        &origin(),
        None,
        |_, _, _| Ok(NEW.to_owned()),
        |stage| {
            assert_eq!(stage, Publication::BeforeRename);
            assert_eq!(
                pending_count(&path),
                1,
                "failure must occur after real staging"
            );
            Err(
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "publication refused")
                    .into(),
            )
        },
    )
    .unwrap_err();
    assert!(error.downcast_ref::<std::io::Error>().is_some());
    assert_eq!(fs::read(&path).unwrap(), source.as_bytes());
    assert_eq!(pending_count(&path), 0);
    // The failed writer released the lock, so a retry can complete.
    assert!(apply(&temp, NEW).unwrap().updated);
}

#[test]
fn editor_change_after_staging_is_checked_again_before_rename() {
    let source = schema();
    let changed = source.replace("answer: 42", "answer: 43");
    let (temp, path) = fixture(source.as_bytes());
    let error = update_remote_pin_with(
        temp.path(),
        &origin(),
        None,
        |_, _, _| Ok(NEW.to_owned()),
        |stage| {
            assert_eq!(stage, Publication::BeforeRename);
            assert_eq!(pending_count(&path), 1);
            fs::write(&path, &changed).unwrap();
            Ok(())
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("schema changed"));
    assert_eq!(fs::read(&path).unwrap(), changed.as_bytes());
    assert_eq!(pending_count(&path), 0);
}

#[test]
fn postpublication_failure_truthfully_reports_the_new_commit_is_visible() {
    let (temp, path) = fixture(schema().as_bytes());
    let error = update_remote_pin_with(
        temp.path(),
        &origin(),
        None,
        |_, _, _| Ok(NEW.to_owned()),
        |stage| {
            if stage == Publication::AfterRename {
                assert_eq!(value(&path)["remotes"][0]["commit"].as_str(), Some(NEW));
                bail!("injected directory-sync refusal");
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("new schema commit was published"));
    assert_eq!(value(&path)["remotes"][0]["commit"].as_str(), Some(NEW));
    assert_eq!(pending_count(&path), 0);
    assert!(!apply(&temp, NEW).unwrap().updated);
}

#[test]
fn project_lock_refuses_a_competing_writer_before_transport_then_allows_retry() {
    let source = schema();
    let (temp, path) = fixture(source.as_bytes());
    let report = update_remote_pin_with(
        temp.path(),
        &origin(),
        None,
        |_, _, _| {
            let error = update_remote_pin_with(
                temp.path(),
                &origin(),
                None,
                |_, _, _| panic!("competing writer must not reach Git"),
                |_| Ok(()),
            )
            .unwrap_err();
            assert!(error.to_string().contains("holds the project lock"));
            assert_eq!(fs::read(&path).unwrap(), source.as_bytes());
            Ok(NEW.to_owned())
        },
        |_| Ok(()),
    )
    .unwrap();
    assert!(report.updated);
    assert!(!apply(&temp, NEW).unwrap().updated);
}

#[test]
fn missing_or_directory_schema_is_not_created_or_replaced() {
    let (temp, path) = fixture(schema().as_bytes());
    fs::remove_file(&path).unwrap();
    assert!(apply(&temp, NEW).is_err());
    assert!(!path.exists());
    fs::create_dir(&path).unwrap();
    assert!(apply(&temp, NEW).is_err());
    assert!(path.is_dir());
    assert!(!temp.path().join(".khive/state").exists());
}

#[cfg(unix)]
#[test]
fn final_schema_and_lock_symlinks_are_refused_without_touching_their_targets() {
    use std::os::unix::fs::symlink;
    let source = schema();
    let (temp, path) = fixture(source.as_bytes());
    let target = temp.path().join("outside.yaml");
    fs::rename(&path, &target).unwrap();
    symlink(&target, &path).unwrap();
    assert!(apply(&temp, NEW).is_err());
    assert_eq!(fs::read(&target).unwrap(), source.as_bytes());
    fs::remove_file(&path).unwrap();
    fs::rename(&target, &path).unwrap();
    let state = temp.path().join(".khive/state");
    fs::create_dir(&state).unwrap();
    fs::write(&target, "untouched lock target").unwrap();
    symlink(&target, state.join("kg-update.lock")).unwrap();
    assert!(apply(&temp, NEW).is_err());
    assert_eq!(fs::read(&path).unwrap(), source.as_bytes());
    assert_eq!(fs::read_to_string(target).unwrap(), "untouched lock target");
}

#[cfg(unix)]
#[test]
fn git_failure_uses_remote_name_and_masks_url_credentials() {
    let mut command = Command::new("sh");
    command.args(["-c", "printf '%s' 'fatal: https://reader:private-sentinel@example.invalid/repo git@example.invalid:repo' >&2; exit 1"]);
    let error = run_git(&mut command, "fetch", &origin(), None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("remote origin: Git fetch failed"));
    assert!(error.contains("<url-redacted>"));
    for private in ["reader", "private-sentinel", "example.invalid", "https://"] {
        assert!(!error.contains(private), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn git_failure_masks_exact_local_and_userless_scp_sources() {
    for source in [
        "example.invalid:private/repo",
        "/tmp/private repository.git",
    ] {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf 'fatal: %s is unavailable' \"$1\" >&2; exit 1",
            "fixture",
            source,
        ]);
        let error = run_git(&mut command, "fetch", &origin(), Some(OsStr::new(source)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("<url-redacted>"));
        assert!(!error.contains(source), "{error}");
    }
}
