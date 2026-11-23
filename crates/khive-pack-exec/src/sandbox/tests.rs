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
    assert_eq!(network_rules(&p), vec![RUN_1_SOCKETS]);
}

/// The one network rule a profile for `/private/tmp/run-1` may carry.
const RUN_1_SOCKETS: &str = "(allow network-bind network-outbound (subpath \"/private/tmp/run-1\") (subpath \"/private/tmp/run-1.tmp\"))";

fn network_rules(profile: &str) -> Vec<&str> {
    profile
        .lines()
        .filter(|line| line.contains("network"))
        .collect()
}

#[test]
fn profile_network_rule_names_only_the_run_and_its_temporary_directory() {
    let run_dir = Path::new("/private/tmp/run-1");
    assert_eq!(
        scratch_dir(run_dir),
        PathBuf::from("/private/tmp/run-1.tmp")
    );
    let profile = render_profile(
        run_dir,
        &[PathBuf::from("/opt/py")],
        &[PathBuf::from("/Volumes/work/cache")],
        &[],
    );
    // One rule, two path filters, both the resolved real paths. A path filter
    // cannot match an inet address, so no tcp, udp or ip term may appear, and
    // no rule may name the /tmp spelling or the whole of /private/tmp.
    assert_eq!(network_rules(&profile), vec![RUN_1_SOCKETS]);
    for absent in [
        "network-inbound",
        "(remote ",
        "(local ",
        "tcp",
        "udp",
        "(subpath \"/tmp",
        "(subpath \"/private/tmp\")",
        "(literal \"/tmp",
    ] {
        assert!(!profile.contains(absent), "{absent} in {profile}");
    }
    // Neither a read root nor a cache root takes a socket.
    let rule = network_rules(&profile)[0];
    assert!(!rule.contains("/opt/py"), "{rule}");
    assert!(!rule.contains("/Volumes/work/cache"), "{rule}");
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
fn profile_without_cache_roots_grants_writes_to_the_run_and_its_temporary_directory_only() {
    let profile = render_profile(Path::new("/private/tmp/run-1"), &[], &[], &[]);
    assert!(profile.contains(
        "(allow file-write* (subpath \"/private/tmp/run-1\") (subpath \"/private/tmp/run-1.tmp\"))\n"
    ));
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
        format!(
            "(allow file-write* (subpath \"/private/tmp/run-1\") (subpath \"/private/tmp/run-1.tmp\") {cache})"
        )
    );
    assert_eq!(network_rules(&profile), vec![RUN_1_SOCKETS]);
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

#[cfg(unix)]
#[test]
fn resolved_cache_roots_are_the_canonical_paths_the_check_compared() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let base = std::fs::canonicalize(dir.path()).expect("canonical base");
    let root = base.join("runs");
    let parent = base.join("parent");
    for path in [&root, &parent] {
        std::fs::create_dir_all(path).expect("fixture directory");
    }
    let alias = base.join("alias");
    std::os::unix::fs::symlink(&parent, &alias).expect("symbolic link to the parent");
    let configured = alias.join("cache");
    let canonical = parent.join("cache");

    // Not created yet: refused, and the current form keeps the configured text.
    assert!(resolve_cache_roots(&root, &[], std::slice::from_ref(&configured)).is_err());
    assert_eq!(
        current_cache_roots(std::slice::from_ref(&configured)),
        vec![configured.clone()]
    );

    std::fs::create_dir(&canonical).expect("cache root created after the configuration");
    let resolved = resolve_cache_roots(&root, &[], &[configured.clone(), canonical.clone()])
        .expect("both spellings resolve");
    assert_eq!(
        resolved,
        vec![canonical.clone()],
        "two spellings of one directory resolve to its canonical path once"
    );
    assert_eq!(
        current_cache_roots(&[configured.clone(), canonical.clone()]),
        resolved
    );

    let granted = format!("(subpath \"{}\")", canonical.display());
    let named = format!("(subpath \"{}\")", configured.display());
    let profile = render_profile(Path::new("/private/tmp/run-1"), &[], &resolved, &[]);
    assert!(profile.contains(&granted), "{profile}");
    assert!(!profile.contains(&named), "{profile}");
    // Control: the configured text rendered as it stands names a path the
    // sandbox never matches, and not the directory a write reaches.
    let unresolved = render_profile(
        Path::new("/private/tmp/run-1"),
        &[],
        std::slice::from_ref(&configured),
        &[],
    );
    assert!(unresolved.contains(&named), "{unresolved}");
    assert!(!unresolved.contains(&granted), "{unresolved}");
}

#[cfg(unix)]
#[test]
fn a_cache_root_through_a_link_a_run_can_write_is_refused() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let base = std::fs::canonicalize(dir.path()).expect("canonical base");
    let root = base.join("runs");
    let shared = base.join("shared");
    let target = base.join("target");
    for path in [&root, &shared, &target.join("sub")] {
        std::fs::create_dir_all(path).expect("fixture directory");
    }
    let in_cache = shared.join("link");
    std::os::unix::fs::symlink(&target, &in_cache).expect("link inside a cache root");
    let in_run_root = root.join("link");
    std::os::unix::fs::symlink(&target, &in_run_root).expect("link inside the run root");
    let outside = base.join("alias");
    std::os::unix::fs::symlink(&target, &outside).expect("link where no run writes");

    // Control: a link outside the run root and every cache root is followed.
    assert_eq!(
        resolve_cache_roots(&root, &[], std::slice::from_ref(&outside)),
        Ok(vec![target.clone()])
    );
    assert_eq!(
        resolve_cache_roots(&root, &[], &[shared.clone(), outside.clone()]),
        Ok(vec![shared.clone(), target.clone()])
    );

    let refused = [
        (
            "a link inside another cache root",
            vec![shared.clone(), in_cache.clone()],
            &in_cache,
            &shared,
        ),
        (
            "a link inside another cache root, as a middle component",
            vec![shared.clone(), in_cache.join("sub")],
            &in_cache,
            &shared,
        ),
        (
            "a link inside the run root",
            vec![in_run_root.clone()],
            &in_run_root,
            &root,
        ),
    ];
    for (why, entries, link, holder) in refused {
        let message =
            resolve_cache_roots(&root, &[], &entries).expect_err(&format!("{why} must be refused"));
        assert!(message.contains(&format!("{link:?}")), "{why}: {message}");
        assert!(message.contains(&format!("{holder:?}")), "{why}: {message}");
        assert!(message.contains("symbolic link"), "{why}: {message}");
    }
}

#[cfg(unix)]
#[test]
fn a_link_met_inside_another_links_target_is_checked_like_one_in_the_entry() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let base = std::fs::canonicalize(dir.path()).expect("canonical base");
    let root = base.join("runs");
    let shared = base.join("shared");
    let operator = base.join("op");
    let target = base.join("target");
    for path in [&root, &shared, &operator, &target.join("x")] {
        std::fs::create_dir_all(path).expect("fixture directory");
    }
    let symlink = |to: &Path, at: &Path| std::os::unix::fs::symlink(to, at).expect("symbolic link");

    // The operator's alias sits where no run writes, and points through a
    // `current` link that sits inside the cache root `shared`.
    let current = shared.join("current");
    symlink(&target, &current);
    let alias = operator.join("alias");
    symlink(&current.join("x"), &alias);
    let message = resolve_cache_roots(&root, &[], &[shared.clone(), alias.clone()])
        .expect_err("an alias through a link inside a cache root must be refused");
    assert!(message.contains(&format!("{current:?}")), "{message}");
    assert!(message.contains(&format!("{shared:?}")), "{message}");
    assert!(message.contains(&format!("{alias:?}")), "{message}");
    // Control: without `shared` among the cache roots nothing a run writes
    // holds the link, and the alias resolves to the directory behind it.
    assert_eq!(
        resolve_cache_roots(&root, &[], std::slice::from_ref(&alias)),
        Ok(vec![target.join("x")])
    );

    // A link to a link, both where no run writes, the second relative and
    // stepping out with `..`, is followed to the directory behind them.
    let second = operator.join("second");
    symlink(Path::new("../target"), &second);
    let first = base.join("first");
    symlink(&second, &first);
    assert_eq!(
        resolve_cache_roots(&root, &[], &[first.join("x")]),
        Ok(vec![target.join("x")])
    );
    assert_eq!(
        current_cache_roots(&[first.join("x")]),
        vec![target.join("x")]
    );

    // A loop is refused rather than followed forever.
    let one = base.join("loop-one");
    let two = base.join("loop-two");
    symlink(&two, &one);
    symlink(&one, &two);
    let message = resolve_cache_roots(&root, &[], &[one.join("x")])
        .expect_err("a loop of links must be refused");
    assert!(
        message.contains("follows more than 40 symbolic links"),
        "{message}"
    );
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

/// ADR-181 Amendment 16 acceptance 80: under the rendered profile a unix
/// socket binds and takes a connection inside the run directory and inside
/// the run's temporary directory, and every other bind or connect is refused,
/// through a link included. A connect is judged by delivery: the listener
/// writes what it receives to a capture file and the arm reads its token back,
/// because `nc -U -z` exits 1 against a live unix listener on macOS. Each
/// refusal has a control that runs the same command outside the sandbox and
/// succeeds, so a refusal is the profile's and not the command's.
#[cfg(target_os = "macos")]
#[test]
fn sandboxed_unix_sockets_reach_the_run_and_its_temporary_directory_only() {
    use std::io::Write;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    if !Path::new(SANDBOX_EXEC).exists() {
        return;
    }
    const NC: &str = "/usr/bin/nc";
    const LN: &str = "/bin/ln";
    for tool in [NC, LN] {
        assert!(Path::new(tool).exists(), "{tool} drives these arms");
    }
    // Short names: a unix socket path holds at most 103 bytes on macOS.
    let holder = tempfile::tempdir().expect("base directory");
    let base = std::fs::canonicalize(holder.path()).expect("canonical base");
    let run_dir = base.join("r");
    let scratch = scratch_dir(&run_dir);
    let outside = base.join("o");
    // Shares the run directory's text prefix, so a prefix match would admit it.
    let sibling = base.join("rx");
    // Listener output lands here, outside every directory under test.
    let captures = base.join("k");
    for dir in [&run_dir, &scratch, &outside, &sibling, &captures] {
        std::fs::create_dir(dir).expect("fixture directory");
    }
    let profile_path = base.join("p.sb");
    std::fs::write(&profile_path, render_profile(&run_dir, &[], &[], &[])).expect("write profile");

    let sandboxed = |program: &str, args: &[&str]| {
        let mut command = Command::new(SANDBOX_EXEC);
        command.arg("-f").arg(&profile_path).arg(program).args(args);
        command
    };
    let plain = |program: &str, args: &[&str]| {
        let mut command = Command::new(program);
        command.args(args);
        command
    };
    let succeeds = |mut command: Command| {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("launch fixture command")
            .success()
    };
    let stop = |mut child: Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    let text = |path: &Path| path.to_str().expect("utf-8 fixture path").to_string();
    // Start a listener with its output in `capture`; report whether `socket`
    // appeared before the listener exited.
    let listen = |mut command: Command, socket: &Path, capture: &Path| -> (bool, Child) {
        let out = std::fs::File::create(capture).expect("capture file");
        let mut child = command
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(Stdio::null())
            .spawn()
            .expect("launch nc listener");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists()
            && child.try_wait().expect("poll listener").is_none()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        (socket.exists(), child)
    };
    // Run a client with one token line on its standard input.
    let send = |mut command: Command, token: &str| {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("launch nc client");
        if let Some(mut stdin) = child.stdin.take() {
            // A refused connect closes the pipe; the write error is that refusal.
            let _ = writeln!(stdin, "{token}");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().expect("poll client").is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        stop(child);
    };
    let holds = |capture: &Path, token: &str| {
        std::fs::read_to_string(capture)
            .map(|received| received.lines().any(|line| line == token))
            .unwrap_or(false)
    };
    let arrives = |capture: &Path, token: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !holds(capture, token) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        holds(capture, token)
    };
    // The sandboxed send goes first, then the same send unsandboxed. The
    // listener takes connections in order, so once the control's token has
    // arrived, a sandboxed token that got through is already in the capture.
    // Returns (control delivered, sandboxed delivered).
    let deliveries = |socket: &Path, capture: &Path, name: &str| {
        let token = format!("{name}-sandboxed");
        let control = format!("{name}-plain");
        send(sandboxed(NC, &["-U", &text(socket)]), &token);
        send(plain(NC, &["-U", &text(socket)]), &control);
        (arrives(capture, &control), holds(capture, &token))
    };

    // Control: a sandboxed listener and a sandboxed client deliver a token in
    // each allowed place.
    for (n, dir) in [&run_dir, &scratch].into_iter().enumerate() {
        let socket = dir.join("s");
        let capture = captures.join(format!("allowed{n}"));
        let (bound, listener) = listen(
            sandboxed(NC, &["-U", "-l", "-k", &text(&socket)]),
            &socket,
            &capture,
        );
        if bound {
            send(sandboxed(NC, &["-U", &text(&socket)]), "inside");
        }
        let delivered = bound && arrives(&capture, "inside");
        stop(listener);
        assert!(bound, "a bind under {dir:?} must be allowed");
        assert!(delivered, "a connect under {dir:?} must deliver");
    }

    // A bind anywhere else is refused, including beside the run directory
    // under a name that shares its text prefix.
    for (n, dir) in [&outside, &sibling].into_iter().enumerate() {
        let refused = dir.join("b");
        let (bound, listener) = listen(
            sandboxed(NC, &["-U", "-l", "-k", &text(&refused)]),
            &refused,
            &captures.join(format!("bind{n}")),
        );
        stop(listener);
        assert!(!bound, "a bind under {dir:?} must be refused");
        let control = dir.join("c");
        let (bound, listener) = listen(
            plain(NC, &["-U", "-l", "-k", &text(&control)]),
            &control,
            &captures.join(format!("bind-control{n}")),
        );
        stop(listener);
        assert!(
            bound,
            "control: the same bind under {dir:?} works unsandboxed"
        );
    }

    // A token sent from inside the sandbox does not reach a listener made
    // outside it. The one in `outside` stands for the khive daemon's socket,
    // which lives outside both allowed directories by construction.
    for (n, dir) in [&outside, &sibling].into_iter().enumerate() {
        let socket = dir.join("d.sock");
        let capture = captures.join(format!("outside{n}"));
        let (bound, listener) = listen(
            plain(NC, &["-U", "-l", "-k", &text(&socket)]),
            &socket,
            &capture,
        );
        let (control, leaked) = if bound {
            deliveries(&socket, &capture, &format!("direct{n}"))
        } else {
            (false, false)
        };
        stop(listener);
        assert!(bound, "control: a listener binds at {socket:?} unsandboxed");
        assert!(control, "control: a token reaches {socket:?} unsandboxed");
        assert!(
            !leaked,
            "a sandboxed connect to {socket:?} must not deliver"
        );
    }

    // Nor through a link inside the run directory that names such a socket:
    // a symbolic link and a hard link, each made outside the sandbox.
    let socket = outside.join("l.sock");
    let capture = captures.join("links");
    let (bound, listener) = listen(
        plain(NC, &["-U", "-l", "-k", &text(&socket)]),
        &socket,
        &capture,
    );
    let symbolic = run_dir.join("sl");
    std::os::unix::fs::symlink(&socket, &symbolic).expect("symbolic link to the outside socket");
    let hard = run_dir.join("hl");
    let hard_made = bound && std::fs::hard_link(&socket, &hard).is_ok();
    let through_symbolic = if bound {
        deliveries(&symbolic, &capture, "symbolic")
    } else {
        (false, false)
    };
    let through_hard = if hard_made {
        deliveries(&hard, &capture, "hard")
    } else {
        (false, false)
    };
    // A hard link to that socket made from inside the sandbox is refused; the
    // same link made unsandboxed is the control.
    let inner = run_dir.join("hi");
    let linked_inside = succeeds(sandboxed(LN, &[&text(&socket), &text(&inner)]));
    let inner_exists = inner.exists();
    let linked_plain = succeeds(plain(LN, &[&text(&socket), &text(&run_dir.join("hc"))]));
    stop(listener);
    assert!(bound, "control: a listener binds at {socket:?} unsandboxed");
    assert!(hard_made, "control: a hard link to {socket:?} can be made");
    assert!(
        through_symbolic.0,
        "control: a token reaches {socket:?} through {symbolic:?} unsandboxed"
    );
    assert!(
        !through_symbolic.1,
        "a sandboxed connect through the symbolic link {symbolic:?} must not deliver"
    );
    assert!(
        through_hard.0,
        "control: a token reaches {socket:?} through {hard:?} unsandboxed"
    );
    assert!(
        !through_hard.1,
        "a sandboxed connect through the hard link {hard:?} must not deliver"
    );
    assert!(
        !linked_inside && !inner_exists,
        "a hard link to {socket:?} made inside the sandbox must be refused"
    );
    assert!(
        linked_plain,
        "control: the same hard link is made unsandboxed"
    );

    // A bind through a directory link inside the run directory that points
    // outside leaves no socket there; the same bind unsandboxed does.
    let door = run_dir.join("d");
    std::os::unix::fs::symlink(&outside, &door).expect("directory link to the outside");
    let (bound_through, listener) = listen(
        sandboxed(NC, &["-U", "-l", "-k", &text(&door.join("x"))]),
        &outside.join("x"),
        &captures.join("door"),
    );
    stop(listener);
    let (control_through, listener) = listen(
        plain(NC, &["-U", "-l", "-k", &text(&door.join("y"))]),
        &outside.join("y"),
        &captures.join("door-control"),
    );
    stop(listener);
    assert!(
        !bound_through && !outside.join("x").exists(),
        "a bind through {door:?} must leave no socket in {outside:?}"
    );
    assert!(
        control_through,
        "control: the same bind through {door:?} works unsandboxed"
    );

    // Loopback stays refused.
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let port = tcp
        .local_addr()
        .expect("loopback address")
        .port()
        .to_string();
    assert!(
        succeeds(plain(NC, &["-z", "-w", "2", "127.0.0.1", &port])),
        "control: the loopback listener takes a connection unsandboxed"
    );
    assert!(
        !succeeds(sandboxed(NC, &["-z", "-w", "2", "127.0.0.1", &port])),
        "a loopback connect must stay refused"
    );
}

#[test]
fn profile_metadata_names_the_root_links_and_nothing_under_them() {
    let profile = render_profile(Path::new("/private/tmp/run-1"), &[], &[], &[]);
    let metadata_rule = profile
        .lines()
        .find(|line| line.trim_start().starts_with("(allow file-read-metadata "))
        .expect("scoped metadata rule");
    for link in SYSTEM_ROOT_LINKS {
        assert!(metadata_rule.contains(&format!("(literal \"{link}\")")));
        assert!(!profile.contains(&format!("(subpath \"{link}\")")));
    }
    assert!(!profile.contains("(subpath \"/private/var\")"));
}

#[cfg(target_os = "macos")]
#[test]
fn system_files_resolve_through_the_root_links() {
    if !Path::new(SANDBOX_EXEC).exists() {
        return;
    }
    let run = tempfile::tempdir().expect("run directory");
    let run_dir = std::fs::canonicalize(run.path()).expect("canonical run directory");
    let profile_path = run_dir.join("profile.sb");
    std::fs::write(&profile_path, render_profile(&run_dir, &[], &[], &[])).expect("write profile");
    let reads = |path: &str| {
        std::process::Command::new(SANDBOX_EXEC)
            .arg("-f")
            .arg(&profile_path)
            .arg("/bin/cat")
            .arg(path)
            .output()
            .expect("launch sandboxed cat")
            .status
            .success()
    };
    assert!(reads("/private/etc/hosts"), "resolved-path control");
    assert!(reads("/etc/hosts"), "read through the /etc link");
    if Path::new("/usr/share/zoneinfo/UTC").exists() {
        assert!(reads("/usr/share/zoneinfo/UTC"), "read through /var");
    }
    // The links open nothing the roots do not: a log under /private/var stays refused by either name.
    if Path::new("/private/var/log/install.log").exists() {
        assert!(!reads("/private/var/log/install.log"));
        assert!(!reads("/var/log/install.log"));
    }
    let lists = std::process::Command::new(SANDBOX_EXEC)
        .arg("-f")
        .arg(&profile_path)
        .arg("/bin/ls")
        .arg("/var/")
        .output()
        .expect("launch sandboxed ls")
        .status
        .success();
    assert!(!lists, "the directory behind /var does not list");
}

#[test]
fn profile_reads_the_system_tool_directory_and_no_neighbour() {
    let profile = render_profile(Path::new("/private/tmp/run-1"), &[], &[], &[]);
    let rule = |head: &str| {
        profile
            .lines()
            .find(|line| line.trim_start().starts_with(head))
            .unwrap_or_else(|| panic!("no rule starting {head}"))
            .to_string()
    };
    assert!(rule("(allow file-read* ").contains("(subpath \"/bin\")"));
    assert!(rule("(allow file-read-metadata ").contains("(subpath \"/bin\")"));
    // Code loading is as it was: the launch of a tool there never needed it.
    assert!(!rule("(allow file-map-executable ").contains("\"/bin\""));
    assert!(!rule("(allow file-write* ").contains("\"/bin\""));
    for neighbour in ["/sbin", "/opt", "/Applications", "/Users"] {
        assert!(!profile.contains(&format!("(subpath \"{neighbour}\")")));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_shell_finds_a_system_tool_by_name() {
    if !Path::new(SANDBOX_EXEC).exists() {
        return;
    }
    let run = tempfile::tempdir().expect("run directory");
    let run_dir = std::fs::canonicalize(run.path()).expect("canonical run directory");
    let rendered = render_profile(&run_dir, &[], &[], &[]);
    let profile_path = run_dir.join("profile.sb");
    std::fs::write(&profile_path, &rendered).expect("write profile");
    // Control: the same profile with the `/bin` root taken out of both rules,
    // which is the template as it was before the root was added.
    let before = rendered.replace(" (subpath \"/bin\")", "");
    assert_eq!(rendered.matches(" (subpath \"/bin\")").count(), 2);
    let control_path = run_dir.join("control.sb");
    std::fs::write(&control_path, &before).expect("write control profile");
    let shell = |profile: &Path, script: &str| {
        std::process::Command::new(SANDBOX_EXEC)
            .arg("-f")
            .arg(profile)
            .arg("/bin/sh")
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&run_dir)
            .output()
            .expect("launch sandboxed shell")
    };

    let made = shell(&profile_path, "mkdir by-name && command -v mkdir");
    assert!(
        made.status.success(),
        "mkdir by name: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&made.stdout).trim(), "/bin/mkdir");
    assert!(run_dir.join("by-name").is_dir());

    let control = shell(&control_path, "mkdir control-by-name");
    assert_eq!(
        control.status.code(),
        Some(127),
        "without the root the shell does not find it: {}",
        String::from_utf8_lossy(&control.stderr)
    );
    assert!(!run_dir.join("control-by-name").exists());
    // The control's refusal is the search and not the launch.
    assert!(shell(&control_path, "/bin/mkdir control-by-path")
        .status
        .success());

    // The root opens that directory and no neighbour.
    assert!(shell(&profile_path, "ls /bin").status.success());
    assert!(!shell(&profile_path, "ls /sbin").status.success());
    assert!(!shell(&profile_path, "mkdir /bin/khive-no-such-dir")
        .status
        .success());
}

fn tool_profile(cfg: ExecToolProfileConfig, user_cache: Option<&str>) -> ToolProfile {
    resolve_tool_profile(&cfg, user_cache.map(PathBuf::from))
}

fn named(tool: &str) -> ExecToolProfileConfig {
    ExecToolProfileConfig {
        tool: tool.into(),
        ..Default::default()
    }
}

#[test]
fn a_run_with_no_tool_profile_gets_the_template_byte_for_byte() {
    let run = Path::new("/private/tmp/run-1");
    let roots = [PathBuf::from("/opt/py")];
    let cache = [PathBuf::from("/Volumes/work/cache")];
    let template = render_profile(run, &roots, &cache, &[]);
    assert_eq!(
        render_profile_for_tool(run, &roots, &cache, &[], None),
        template
    );
    // A profile that turns nothing on adds nothing either.
    let empty = tool_profile(named("sh-empty"), Some("/private/var/folders/aa/bb/C"));
    assert_eq!(render_tool_rules(&empty), "");
    assert_eq!(
        render_profile_for_tool(run, &roots, &cache, &[], Some(&empty)),
        template
    );
}

#[test]
fn metal_rules_name_the_device_the_compiler_and_metals_own_cache_folder() {
    let profile = tool_profile(
        ExecToolProfileConfig {
            metal: true,
            ..named("sh-gpu")
        },
        Some("/private/var/folders/aa/bb/C"),
    );
    assert_eq!(
        profile.metal_cache.as_deref(),
        Some(Path::new("/private/var/folders/aa/bb/C/com.apple.metal"))
    );
    let rules = render_tool_rules(&profile);
    assert!(
        rules.contains("(allow iokit-open (iokit-user-client-class \"AGXDeviceUserClient\"))\n")
    );
    assert!(rules.contains(
        "(allow mach-lookup (global-name \"com.apple.MTLCompilerService\") (global-name \"com.apple.bsd.dirhelper\"))\n"
    ));
    assert!(rules.contains(
        "(allow file-read* file-write* (subpath \"/private/var/folders/aa/bb/C/com.apple.metal\"))\n"
    ));
    // The user's cache folder is named for traversal only: nothing in it but
    // Metal's own folder is readable or writable.
    let metadata = rules
        .lines()
        .find(|line| line.starts_with("(allow file-read-metadata "))
        .expect("metadata rule");
    for literal in [
        "(literal \"/private/var/folders/aa/bb/C\")",
        "(literal \"/private/var/folders/aa/bb\")",
        "(literal \"/private/var/folders\")",
        "(literal \"/var\")",
    ] {
        assert!(metadata.contains(literal), "{literal} missing: {metadata}");
    }
    assert!(!rules.contains("(subpath \"/private/var/folders/aa/bb/C\")"));
    assert!(!rules.contains("network"));
    assert_eq!(rules.matches("iokit-open").count(), 1);
    assert_eq!(rules.matches("mach-lookup").count(), 1);

    // Off means off: the same entry without `metal` names no device.
    let off = tool_profile(named("sh-gpu"), Some("/private/var/folders/aa/bb/C"));
    assert_eq!(off.metal_cache, None);
    assert!(!render_tool_rules(&off).contains("iokit"));
}

/// The `unix_sockets` key loads, adds no rule and is reported as superseded:
/// the template's one socket rule is the only network rule a profiled run has.
#[test]
fn the_unix_sockets_key_adds_no_rule_and_is_reported_superseded() {
    let run = Path::new("/private/tmp/run-1");
    let cache = [PathBuf::from("/Volumes/work/cache")];
    let base = ExecToolProfileConfig {
        files: vec!["/khive-no-such-dir/locks/gpu.lock".into()],
        ..named("sh-sock")
    };
    let without = tool_profile(base.clone(), None);
    let with = tool_profile(
        ExecToolProfileConfig {
            unix_sockets: true,
            ..base
        },
        None,
    );
    assert_eq!(render_tool_rules(&with), render_tool_rules(&without));
    assert_eq!(tool_rules_digest(&with), tool_rules_digest(&without));
    let rendered = render_profile_for_tool(run, &[], &cache, &[], Some(&with));
    assert_eq!(network_rules(&rendered), vec![RUN_1_SOCKETS]);
    assert!(!rendered.contains("unix-socket"), "{rendered}");
    assert!(!rendered.contains("network-inbound"), "{rendered}");
    assert_eq!(with.superseded_keys, vec!["unix_sockets"]);
    assert!(without.superseded_keys.is_empty());
    assert_eq!(
        tool_profile_json(&with)["superseded_keys"],
        json!(["unix_sockets"])
    );
    assert_eq!(tool_profile_json(&without)["superseded_keys"], json!([]));
}

#[test]
fn file_and_directory_rules_are_exact_paths_with_their_ancestors() {
    let profile = tool_profile(
        ExecToolProfileConfig {
            files: vec!["/khive-no-such-dir/locks/gpu.lock".into()],
            list_dirs: vec!["/khive-no-such-dir/models".into()],
            ..named("sh-files")
        },
        None,
    );
    let rules = render_tool_rules(&profile);
    assert!(rules.contains(
        "(allow file-read* file-write* (literal \"/khive-no-such-dir/locks/gpu.lock\"))\n"
    ));
    assert!(rules.contains("(allow file-read-data (literal \"/khive-no-such-dir/models\"))\n"));
    assert!(!rules.contains("subpath"));
    let metadata = rules
        .lines()
        .find(|line| line.starts_with("(allow file-read-metadata "))
        .expect("metadata rule");
    assert_eq!(
        metadata,
        "(allow file-read-metadata (literal \"/\") (literal \"/khive-no-such-dir\") \
         (literal \"/khive-no-such-dir/locks\") (literal \"/khive-no-such-dir/models\"))"
    );
}

#[test]
fn tool_profile_conflicts_are_refused_with_the_reason() {
    let root = Path::new("/khive-no-such-dir/exec-root");
    let ok = tool_profile(
        ExecToolProfileConfig {
            files: vec!["/khive-no-such-dir/locks/gpu.lock".into()],
            ..named("sh-gpu")
        },
        None,
    );
    let reads = [PathBuf::from("/khive-no-such-dir/models")];
    let caches = [PathBuf::from("/khive-no-such-dir/cache")];
    let conflict =
        |profiles: &[ToolProfile]| tool_profile_conflict(root, &reads, &caches, profiles);
    // Control: a well-formed list is accepted, twice over two names, and a
    // directory under a read root may be listed.
    let other = ToolProfile {
        tool: "sh-other".into(),
        ..tool_profile(
            ExecToolProfileConfig {
                files: vec!["/khive-no-such-dir/locks/other.lock".into()],
                list_dirs: vec!["/khive-no-such-dir/models/weights".into()],
                ..named("sh-other")
            },
            None,
        )
    };
    assert_eq!(conflict(&[ok.clone(), other]), None);

    let unnamed = tool_profile(named("  "), None);
    assert!(conflict(&[unnamed])
        .expect("empty name refused")
        .contains("empty tool name"));
    assert!(conflict(&[ok.clone(), ok.clone()])
        .expect("duplicate refused")
        .contains("more than once"));
    for (file, kind, root_named) in [
        (
            "/Library/khive-no-such-file.lock",
            "system read root",
            "/Library",
        ),
        (
            "/khive-no-such-dir/models/weights.lock",
            "read root",
            "/khive-no-such-dir/models",
        ),
        (
            "/khive-no-such-dir/cache/shared.lock",
            "cache root",
            "/khive-no-such-dir/cache",
        ),
    ] {
        let under = tool_profile(
            ExecToolProfileConfig {
                files: vec![file.into()],
                ..named("sh-under")
            },
            None,
        );
        let reason = conflict(&[under]).unwrap_or_else(|| panic!("{file} refused"));
        assert!(
            reason.contains(&format!("is under the {kind} {root_named:?}"))
                && reason.contains(file),
            "{file}: {reason}"
        );
    }
    let relative = tool_profile(
        ExecToolProfileConfig {
            list_dirs: vec!["models".into()],
            ..named("sh-rel")
        },
        None,
    );
    assert!(conflict(&[relative])
        .expect("relative path refused")
        .contains("not an absolute path"));
    let inside = tool_profile(
        ExecToolProfileConfig {
            files: vec!["/khive-no-such-dir/exec-root/other-run/file".into()],
            ..named("sh-in")
        },
        None,
    );
    assert!(conflict(&[inside])
        .expect("path under the run root refused")
        .contains("under the run root"));
}

#[test]
fn tool_profile_report_names_what_is_on_and_a_digest_that_follows_it() {
    let gpu = tool_profile(
        ExecToolProfileConfig {
            metal: true,
            ..named("sh-gpu")
        },
        Some("/private/var/folders/aa/bb/C"),
    );
    let report = tool_profile_json(&gpu);
    assert_eq!(report["tool"], "sh-gpu");
    assert_eq!(report["metal"], true);
    assert_eq!(
        report["metal_cache"],
        "/private/var/folders/aa/bb/C/com.apple.metal"
    );
    assert_eq!(report["files"], json!([]));
    assert_eq!(report["superseded_keys"], json!([]));
    let plain = tool_profile(named("sh-gpu"), None);
    assert_ne!(tool_rules_digest(&gpu), tool_rules_digest(&plain));
    assert_eq!(report["rules_digest"], json!(tool_rules_digest(&gpu)));
    assert_eq!(
        tool_profile_for(&[plain.clone(), gpu.clone()], "sh-gpu"),
        Some(&plain)
    );
    assert_eq!(tool_profile_for(&[gpu], "sh"), None);
}

#[cfg(target_os = "macos")]
#[test]
fn the_user_cache_folder_resolves_to_an_existing_directory() {
    let cache = user_cache_dir().expect("the system names a cache folder for this user");
    assert!(cache.is_absolute(), "{cache:?}");
    assert!(cache.is_dir(), "{cache:?}");
    assert_eq!(std::fs::canonicalize(&cache).expect("canonical"), cache);
}

/// Run `program` with `args` under `profile`, written into `run_dir`.
#[cfg(target_os = "macos")]
fn sandboxed_status(run_dir: &Path, profile: &str, program: &str, args: &[&str]) -> bool {
    let profile_path = run_dir.join("profile.sb");
    std::fs::write(&profile_path, profile).expect("write profile");
    std::process::Command::new(SANDBOX_EXEC)
        .arg("-f")
        .arg(&profile_path)
        .arg(program)
        .args(args)
        .output()
        .expect("launch sandboxed program")
        .status
        .success()
}

#[cfg(target_os = "macos")]
#[test]
fn sandboxed_run_reaches_a_named_file_and_directory_and_nothing_beside_them() {
    if !Path::new(SANDBOX_EXEC).exists() {
        return;
    }
    let run = tempfile::tempdir().expect("run directory");
    let outside = tempfile::tempdir().expect("directory outside the run");
    let run_dir = std::fs::canonicalize(run.path()).expect("canonical run directory");
    // The spelling a program would use: on macOS a temporary directory is
    // named through a link, and the profile must admit both spellings.
    let written = outside.path().to_path_buf();
    let canonical = std::fs::canonicalize(&written).expect("canonical outside directory");
    std::fs::create_dir(canonical.join("listed")).expect("listed directory");
    std::fs::create_dir(canonical.join("listed").join("below")).expect("directory below");
    std::fs::write(canonical.join("listed").join("entry"), b"x").expect("entry");
    let lock = written.join("gpu.lock");
    let listed = written.join("listed");

    let profile = tool_profile(
        ExecToolProfileConfig {
            files: vec![lock.to_string_lossy().into()],
            list_dirs: vec![listed.to_string_lossy().into()],
            ..named("sh-files")
        },
        None,
    );
    let with_tool = render_profile_for_tool(&run_dir, &[], &[], &[], Some(&profile));
    let template = render_profile(&run_dir, &[], &[], &[]);
    let path = |p: &Path| p.to_string_lossy().to_string();
    let run_with = |program: &str, target: &Path| {
        sandboxed_status(&run_dir, &with_tool, program, &[&path(target)])
    };
    let run_template = |program: &str, target: &Path| {
        sandboxed_status(&run_dir, &template, program, &[&path(target)])
    };

    assert!(
        run_with("/usr/bin/touch", &run_dir.join("inside")),
        "run-tree control must work"
    );
    // The named file, by the spelling in the configuration and by its
    // resolved path, and not for a run without the profile.
    assert!(run_with("/usr/bin/touch", &lock));
    assert!(canonical.join("gpu.lock").exists());
    assert!(run_with("/usr/bin/touch", &canonical.join("gpu.lock")));
    assert!(
        !run_with("/usr/bin/touch", &written.join("beside")),
        "a file beside the named one must be refused"
    );
    assert!(!canonical.join("beside").exists());
    assert!(
        !run_template("/usr/bin/touch", &written.join("unlisted")),
        "the template alone reaches nothing outside the run"
    );
    assert!(!canonical.join("unlisted").exists());
    // The named directory lists; what is below it and beside it does not.
    assert!(run_with("/bin/ls", &listed));
    assert!(!run_with("/bin/ls", &listed.join("below")));
    assert!(!run_with("/bin/cat", &listed.join("entry")));
    assert!(!run_with("/bin/ls", &written));
    assert!(!run_template("/bin/ls", &listed));
}

/// Acceptance on a host with a GPU. The probe is `tests/fixtures/metal_probe.swift`,
/// compiled by the operator; its path is given in `KHIVE_EXEC_METAL_PROBE`. It
/// compiles a compute kernel from source, runs it on the device and prints
/// `METAL-OK` only when the numbers are right.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs a Metal GPU and a compiled probe named in KHIVE_EXEC_METAL_PROBE"]
fn sandboxed_run_computes_on_the_gpu_only_with_the_metal_profile() {
    let probe = std::env::var("KHIVE_EXEC_METAL_PROBE")
        .expect("KHIVE_EXEC_METAL_PROBE names the compiled probe");
    let probe = std::fs::canonicalize(&probe).expect("the probe exists");
    let probe_dir = probe.parent().expect("probe directory").to_path_buf();
    let run = tempfile::tempdir().expect("run directory");
    let run_dir = std::fs::canonicalize(run.path()).expect("canonical run directory");
    let reads = [probe_dir];
    let output = |profile: Option<String>| {
        let mut command = match &profile {
            Some(profile) => {
                let profile_path = run_dir.join("profile.sb");
                std::fs::write(&profile_path, profile).expect("write profile");
                let mut command = std::process::Command::new(SANDBOX_EXEC);
                command.arg("-f").arg(profile_path).arg(&probe);
                command
            }
            None => std::process::Command::new(&probe),
        };
        let out = command.output().expect("launch the probe");
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    // Control: the host has a device and the probe works on it.
    assert!(
        output(None).contains("METAL-OK"),
        "the probe must pass unsandboxed"
    );

    let cache = user_cache_dir().expect("user cache folder");
    let gpu = resolve_tool_profile(
        &ExecToolProfileConfig {
            metal: true,
            ..named("sh-gpu")
        },
        Some(cache),
    );
    let with_tool = render_profile_for_tool(&run_dir, &reads, &[], &[], Some(&gpu));
    assert!(
        output(Some(with_tool)).contains("METAL-OK"),
        "the Metal profile must reach the device"
    );
    let template = render_profile(&run_dir, &reads, &[], &[]);
    let refused = output(Some(template));
    assert!(
        !refused.contains("METAL-OK") && refused.contains("FAIL device"),
        "the template alone must see no device: {refused}"
    );
}
