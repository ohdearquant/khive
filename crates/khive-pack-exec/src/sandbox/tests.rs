use super::*;

#[test]
fn check_backend_accepts_a_backend_that_is_present() {
    // Positive control: a path that exists on every host running this suite,
    // so the refusal arm below is about absence and not about the check.
    check_backend("/bin/sh").expect("an existing path reads as a present backend");
}

#[test]
fn check_backend_refuses_and_names_the_platform_when_absent() {
    let missing = "/usr/bin/sandbox-exec-that-is-not-installed";
    assert!(!Path::new(missing).exists());
    let error = check_backend(missing).unwrap_err();
    assert!(error.contains(missing), "{error}");
    assert!(error.contains(std::env::consts::OS), "{error}");
    assert!(error.contains("seatbelt"), "{error}");
}

#[test]
fn profile_names_run_dir_and_roots_only() {
    let p = render_profile(
        Path::new("/private/tmp/run-1"),
        &[PathBuf::from("/opt/py")],
        &[],
        &[PathBuf::from("/opt/never/tool")],
    );
    assert!(p.contains("(deny default)"));
    assert!(p.contains(
        "(deny process-exec (regex #\"(^|/)(git|gh)$\") (regex #\"/git-[^/]+$\") (literal \"/opt/never/tool\"))"
    ));
    assert!(p.contains("(subpath \"/private/tmp/run-1\")"));
    assert!(p.contains("(subpath \"/opt/py\")"));
    assert!(!p.contains("/Users"));
    assert!(!p.contains("network"));
}

#[test]
fn profile_metadata_reads_use_allowed_trees_and_exact_ancestors() {
    let profile = render_profile(
        Path::new("/private/tmp/run-1"),
        &[PathBuf::from("/opt/py")],
        &[],
        &[],
    );
    let metadata_rule = profile
        .lines()
        .find(|line| line.trim_start().starts_with("(allow file-read-metadata "))
        .expect("scoped metadata rule");
    assert!(metadata_rule.contains("(subpath \"/private/tmp/run-1\")"));
    assert!(metadata_rule.contains("(subpath \"/opt/py\")"));
    assert!(metadata_rule.contains("(literal \"/private/tmp\")"));
    assert!(metadata_rule.contains("(literal \"/opt\")"));
    assert!(!profile.contains("(allow file-read-metadata)"));
    assert!(!metadata_rule.contains("(subpath \"/Users\")"));
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_metadata_probe_is_limited_to_the_run_tree() {
    if !Path::new(SANDBOX_EXEC).exists() {
        return;
    }
    let run = tempfile::tempdir().expect("run directory");
    let outside = tempfile::tempdir().expect("outside directory");
    let run_dir = std::fs::canonicalize(run.path()).expect("canonical run directory");
    let inside_file = run_dir.join("inside");
    let outside_file = outside.path().join("outside");
    std::fs::write(&inside_file, b"inside").expect("inside fixture");
    std::fs::write(&outside_file, b"outside").expect("outside fixture");
    let outside_file = std::fs::canonicalize(outside_file).expect("canonical outside file");
    let profile_path = run_dir.join("profile.sb");
    std::fs::write(&profile_path, render_profile(&run_dir, &[], &[], &[])).expect("write profile");

    let stat = |path: &Path| {
        std::process::Command::new(SANDBOX_EXEC)
            .arg("-f")
            .arg(&profile_path)
            .arg("/usr/bin/stat")
            .arg("-f")
            .arg("%z")
            .arg(path)
            .output()
            .expect("launch sandboxed stat")
    };
    assert!(
        stat(&inside_file).status.success(),
        "run-tree control must work"
    );
    assert!(
        !stat(&outside_file).status.success(),
        "metadata outside the allowed tree must be refused"
    );
}

#[test]
fn roots_digest_changes_with_roots() {
    let a = read_roots_digest(&[PathBuf::from("/a")]);
    let b = read_roots_digest(&[PathBuf::from("/a"), PathBuf::from("/b")]);
    assert_ne!(a, b);
    assert_eq!(a, digest_hex(br#"["/a"]"#));
}

#[cfg(unix)]
#[test]
fn forbidden_basenames_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let git = dir.path().join("git");
    std::fs::write(&git, b"#!/bin/sh\n").unwrap();
    let alias = dir.path().join("innocent");
    std::os::unix::fs::symlink(&git, &alias).unwrap();
    assert!(matches!(
        check_binary(alias.to_str().unwrap(), &[]),
        Err(BinaryRefusal::Forbidden { .. })
    ));
    let other = dir.path().join("tool");
    std::fs::write(&other, b"x").unwrap();
    let canonical = std::fs::canonicalize(&other).unwrap();
    assert!(matches!(
        check_binary(other.to_str().unwrap(), std::slice::from_ref(&canonical)),
        Err(BinaryRefusal::Never { .. })
    ));
    assert_eq!(
        check_binary(other.to_str().unwrap(), &[]).unwrap(),
        canonical
    );
}

#[test]
fn profile_without_cache_roots_grants_writes_to_the_run_directory_only() {
    let profile = render_profile(Path::new("/private/tmp/run-1"), &[], &[], &[]);
    assert!(profile.contains("(allow file-write* (subpath \"/private/tmp/run-1\"))\n"));
}

#[test]
fn profile_grants_a_cache_root_reads_writes_and_code_loading() {
    let profile = render_profile(
        Path::new("/private/tmp/run-1"),
        &[],
        &[PathBuf::from("/Volumes/work/cache")],
        &[],
    );
    let rule = |head: &str| {
        profile
            .lines()
            .find(|line| line.trim_start().starts_with(head))
            .unwrap_or_else(|| panic!("no {head} rule"))
            .to_string()
    };
    let cache = "(subpath \"/Volumes/work/cache\")";
    assert!(rule("(allow file-read* ").contains(cache));
    assert!(rule("(allow file-read-metadata ").contains(cache));
    assert!(rule("(allow file-read-metadata ").contains("(literal \"/Volumes/work\")"));
    assert!(rule("(allow file-map-executable ").contains(cache));
    assert_eq!(
        rule("(allow file-write* "),
        format!("(allow file-write* (subpath \"/private/tmp/run-1\") {cache})")
    );
    assert!(!profile.contains("network"));
}

#[cfg(unix)]
#[test]
fn cache_roots_must_exist_be_absolute_and_clear_of_the_run_root() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let base = std::fs::canonicalize(dir.path()).expect("canonical base");
    let root = base.join("runs");
    let inner = root.join("inner");
    let sibling = base.join("cache");
    std::fs::create_dir_all(&inner).expect("inner directory");
    std::fs::create_dir_all(&sibling).expect("sibling directory");
    let alias = base.join("alias");
    std::os::unix::fs::symlink(&inner, &alias).expect("symbolic link into the run root");

    assert_eq!(cache_root_conflict(&root, &[], &[]), None);
    assert_eq!(
        cache_root_conflict(&root, &[], std::slice::from_ref(&sibling)),
        None
    );

    let refused = [
        ("holds the run root", base.clone()),
        ("equals the run root", root.clone()),
        ("sits inside the run root", inner.clone()),
        ("alias of a directory inside the run root", alias),
        (
            "`..` path that resolves inside the run root",
            base.join("cache").join("..").join("runs").join("inner"),
        ),
        ("relative", PathBuf::from("relative/cache")),
    ];
    for (why, entry) in refused {
        let message = cache_root_conflict(&root, &[], std::slice::from_ref(&entry))
            .unwrap_or_else(|| panic!("{why} must be refused"));
        assert!(message.contains(&format!("{entry:?}")), "{why}: {message}");
    }

    let missing = base.join("missing");
    let message = cache_root_conflict(&root, &[], std::slice::from_ref(&missing))
        .expect("a cache root that does not exist must be refused");
    assert!(message.contains(&format!("{missing:?}")), "{message}");
    assert!(message.contains("must exist"), "{message}");

    // A missing path under a real directory is refused too, even though its
    // text sits inside a directory the check can resolve.
    let under_run_root = inner.join("not-yet");
    assert!(
        cache_root_conflict(&root, &[], std::slice::from_ref(&under_run_root)).is_some(),
        "a missing path inside the run root must be refused"
    );
}

#[cfg(unix)]
#[test]
fn cache_roots_must_not_overlap_a_read_root() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let base = std::fs::canonicalize(dir.path()).expect("canonical base");
    let root = base.join("runs");
    let shared = base.join("shared");
    let deep = shared.join("deep");
    let read = base.join("read");
    let read_sibling = base.join("read-cache");
    let cache = base.join("cache");
    for path in [&root, &deep, &read, &read_sibling, &cache] {
        std::fs::create_dir_all(path).expect("fixture directory");
    }

    // Control: siblings, including a name that only shares a text prefix.
    for sibling in [&cache, &read_sibling] {
        assert_eq!(
            cache_root_conflict(
                &root,
                std::slice::from_ref(&read),
                std::slice::from_ref(sibling)
            ),
            None,
            "{sibling:?} sits beside the read root and must be accepted"
        );
    }

    let overlaps = [
        ("equals", shared.clone(), shared.clone()),
        ("holds", deep.clone(), shared.clone()),
        ("sits inside", shared.clone(), deep.clone()),
    ];
    for (why, read_root, cache_root) in overlaps {
        let message = cache_root_conflict(
            &root,
            std::slice::from_ref(&read_root),
            std::slice::from_ref(&cache_root),
        )
        .unwrap_or_else(|| panic!("a cache root that {why} a read root must be refused"));
        assert!(
            message.contains(&format!("{cache_root:?}")),
            "{why}: {message}"
        );
        assert!(
            message.contains(&format!("{read_root:?}")),
            "{why}: {message}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn sandboxed_write_reaches_a_cache_root_and_nothing_beside_it() {
    if !Path::new(SANDBOX_EXEC).exists() {
        return;
    }
    let run = tempfile::tempdir().expect("run directory");
    let cache = tempfile::tempdir().expect("cache directory");
    let beside = tempfile::tempdir().expect("directory beside the cache");
    let run_dir = std::fs::canonicalize(run.path()).expect("canonical run directory");
    let cache_dir = std::fs::canonicalize(cache.path()).expect("canonical cache directory");
    let beside_dir = std::fs::canonicalize(beside.path()).expect("canonical beside directory");
    let profile_path = run_dir.join("profile.sb");

    let touch = |profile: String, target: &Path| {
        std::fs::write(&profile_path, profile).expect("write profile");
        std::process::Command::new(SANDBOX_EXEC)
            .arg("-f")
            .arg(&profile_path)
            .arg("/usr/bin/touch")
            .arg(target)
            .output()
            .expect("launch sandboxed touch")
            .status
            .success()
    };
    let with_cache = render_profile(&run_dir, &[], std::slice::from_ref(&cache_dir), &[]);
    assert!(
        touch(with_cache.clone(), &run_dir.join("inside")),
        "run-tree control must work"
    );
    assert!(touch(with_cache.clone(), &cache_dir.join("kept")));
    assert!(cache_dir.join("kept").exists());
    assert!(
        !touch(with_cache, &beside_dir.join("escaped")),
        "a write beside the cache root must be refused"
    );
    assert!(!beside_dir.join("escaped").exists());
    let without_cache = render_profile(&run_dir, &[], &[], &[]);
    assert!(
        !touch(without_cache, &cache_dir.join("unlisted")),
        "the same directory is closed to a run that has no cache root"
    );
    assert!(!cache_dir.join("unlisted").exists());
}
