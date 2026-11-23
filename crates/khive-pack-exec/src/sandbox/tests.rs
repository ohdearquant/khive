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

#[test]
fn cache_roots_must_be_absolute_and_clear_of_the_run_root() {
    let root = Path::new("/nonexistent-exec-root/runs");
    assert_eq!(cache_root_conflict(root, &[]), None);
    assert_eq!(
        cache_root_conflict(root, &[PathBuf::from("/nonexistent-exec-root/cache")]),
        None
    );
    for bad in [
        "/nonexistent-exec-root",
        "/nonexistent-exec-root/runs",
        "/nonexistent-exec-root/runs/inner",
        "relative/cache",
    ] {
        assert!(
            cache_root_conflict(root, &[PathBuf::from(bad)]).is_some(),
            "{bad} must be refused"
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
    assert_eq!(render_tool_rules(run, &cache, &empty), "");
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
    let rules = render_tool_rules(Path::new("/private/tmp/run-1"), &[], &profile);
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
    assert!(!render_tool_rules(Path::new("/private/tmp/run-1"), &[], &off).contains("iokit"));
}

#[test]
fn unix_socket_rules_name_the_run_directory_and_the_cache_roots_and_no_address() {
    let profile = tool_profile(
        ExecToolProfileConfig {
            unix_sockets: true,
            ..named("sh-sock")
        },
        None,
    );
    let rules = render_tool_rules(
        Path::new("/private/tmp/run-1"),
        &[PathBuf::from("/Volumes/work/cache")],
        &profile,
    );
    let lines: Vec<&str> = rules.lines().collect();
    assert_eq!(
        lines,
        vec![
            "(allow network-bind network-inbound (local unix-socket (subpath \"/private/tmp/run-1\")))",
            "(allow network-outbound (remote unix-socket (subpath \"/private/tmp/run-1\")))",
            "(allow network-bind network-inbound (local unix-socket (subpath \"/Volumes/work/cache\")))",
            "(allow network-outbound (remote unix-socket (subpath \"/Volumes/work/cache\")))",
        ]
    );
    assert!(!rules.contains(" ip "));
    assert!(!rules.contains("localhost"));
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
    let rules = render_tool_rules(Path::new("/private/tmp/run-1"), &[], &profile);
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
    // Control: a well-formed list is accepted, twice over two names.
    let other = ToolProfile {
        tool: "sh-other".into(),
        ..ok.clone()
    };
    assert_eq!(tool_profile_conflict(root, &[ok.clone(), other]), None);

    let unnamed = tool_profile(named("  "), None);
    assert!(tool_profile_conflict(root, &[unnamed])
        .expect("empty name refused")
        .contains("empty tool name"));
    assert!(tool_profile_conflict(root, &[ok.clone(), ok.clone()])
        .expect("duplicate refused")
        .contains("more than once"));
    let relative = tool_profile(
        ExecToolProfileConfig {
            list_dirs: vec!["models".into()],
            ..named("sh-rel")
        },
        None,
    );
    assert!(tool_profile_conflict(root, &[relative])
        .expect("relative path refused")
        .contains("not an absolute path"));
    let inside = tool_profile(
        ExecToolProfileConfig {
            files: vec!["/khive-no-such-dir/exec-root/other-run/file".into()],
            ..named("sh-in")
        },
        None,
    );
    assert!(tool_profile_conflict(root, &[inside])
        .expect("path under the run root refused")
        .contains("under the run root"));
}

#[test]
fn tool_profile_report_names_what_is_on_and_a_digest_that_follows_it() {
    let gpu = tool_profile(
        ExecToolProfileConfig {
            metal: true,
            unix_sockets: true,
            ..named("sh-gpu")
        },
        Some("/private/var/folders/aa/bb/C"),
    );
    let report = tool_profile_json(&gpu);
    assert_eq!(report["tool"], "sh-gpu");
    assert_eq!(report["metal"], true);
    assert_eq!(report["unix_sockets"], true);
    assert_eq!(
        report["metal_cache"],
        "/private/var/folders/aa/bb/C/com.apple.metal"
    );
    assert_eq!(report["files"], json!([]));
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
fn sandboxed(run_dir: &Path, profile: &str, program: &str, args: &[&str]) -> bool {
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

    assert!(
        sandboxed(
            &run_dir,
            &with_tool,
            "/usr/bin/touch",
            &[&path(&run_dir.join("inside"))]
        ),
        "run-tree control must work"
    );
    // The named file, by the spelling in the configuration and by its
    // resolved path, and not for a run without the profile.
    assert!(sandboxed(
        &run_dir,
        &with_tool,
        "/usr/bin/touch",
        &[&path(&lock)]
    ));
    assert!(canonical.join("gpu.lock").exists());
    assert!(sandboxed(
        &run_dir,
        &with_tool,
        "/usr/bin/touch",
        &[&path(&canonical.join("gpu.lock"))]
    ));
    assert!(
        !sandboxed(
            &run_dir,
            &with_tool,
            "/usr/bin/touch",
            &[&path(&written.join("beside"))]
        ),
        "a file beside the named one must be refused"
    );
    assert!(!canonical.join("beside").exists());
    assert!(
        !sandboxed(
            &run_dir,
            &template,
            "/usr/bin/touch",
            &[&path(&written.join("unlisted"))]
        ),
        "the template alone reaches nothing outside the run"
    );
    assert!(!canonical.join("unlisted").exists());
    // The named directory lists; what is below it and beside it does not.
    assert!(sandboxed(
        &run_dir,
        &with_tool,
        "/bin/ls",
        &[&path(&listed)]
    ));
    assert!(!sandboxed(
        &run_dir,
        &with_tool,
        "/bin/ls",
        &[&path(&listed.join("below"))]
    ));
    assert!(!sandboxed(
        &run_dir,
        &with_tool,
        "/bin/cat",
        &[&path(&listed.join("entry"))]
    ));
    assert!(!sandboxed(
        &run_dir,
        &with_tool,
        "/bin/ls",
        &[&path(&written)]
    ));
    assert!(!sandboxed(
        &run_dir,
        &template,
        "/bin/ls",
        &[&path(&listed)]
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn sandboxed_run_binds_a_unix_socket_in_its_own_directory_and_no_other_socket() {
    const PERL: &str = "/usr/bin/perl";
    if !Path::new(SANDBOX_EXEC).exists() || !Path::new(PERL).exists() {
        return;
    }
    const BIND_UNIX: &str = "use Socket; socket(my $s, PF_UNIX, SOCK_STREAM, 0) or exit 2; \
                             bind($s, pack_sockaddr_un($ARGV[0])) ? exit 0 : exit 1;";
    const BIND_TCP: &str = "use Socket; socket(my $s, PF_INET, SOCK_STREAM, 0) or exit 2; \
                            bind($s, pack_sockaddr_in(0, inet_aton('127.0.0.1'))) ? exit 0 : exit 1;";
    const CONNECT_TCP: &str = "use Socket; socket(my $s, PF_INET, SOCK_STREAM, 0) or exit 2; \
                               connect($s, pack_sockaddr_in($ARGV[0], inet_aton('127.0.0.1'))) ? exit 0 : exit 1;";
    let run = tempfile::tempdir().expect("run directory");
    let outside = tempfile::tempdir().expect("directory outside the run");
    let run_dir = std::fs::canonicalize(run.path()).expect("canonical run directory");
    let outside_dir = std::fs::canonicalize(outside.path()).expect("canonical outside directory");
    let path = |p: &Path| p.to_string_lossy().to_string();

    // Controls, no sandbox: each probe succeeds when nothing refuses it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let port = listener
        .local_addr()
        .expect("listener address")
        .port()
        .to_string();
    let free = |script: &str, arg: &str| {
        std::process::Command::new(PERL)
            .args(["-e", script, arg])
            .status()
            .expect("launch perl")
            .success()
    };
    assert!(free(BIND_UNIX, &path(&outside_dir.join("control.sock"))));
    assert!(free(BIND_TCP, ""));
    assert!(free(CONNECT_TCP, &port));

    let profile = tool_profile(
        ExecToolProfileConfig {
            unix_sockets: true,
            ..named("sh-sock")
        },
        None,
    );
    let with_tool = render_profile_for_tool(&run_dir, &[], &[], &[], Some(&profile));
    let template = render_profile(&run_dir, &[], &[], &[]);
    let probe = |profile: &str, script: &str, arg: &str| {
        sandboxed(&run_dir, profile, PERL, &["-e", script, arg])
    };

    assert!(probe(
        &with_tool,
        BIND_UNIX,
        &path(&run_dir.join("inside.sock"))
    ));
    assert!(run_dir.join("inside.sock").exists());
    assert!(
        !probe(
            &with_tool,
            BIND_UNIX,
            &path(&outside_dir.join("outside.sock"))
        ),
        "a unix socket outside the run directory must be refused"
    );
    assert!(!outside_dir.join("outside.sock").exists());
    assert!(
        !probe(&with_tool, BIND_TCP, ""),
        "a loopback bind must be refused"
    );
    assert!(
        !probe(&with_tool, CONNECT_TCP, &port),
        "a loopback connection must be refused"
    );
    assert!(
        !probe(&template, BIND_UNIX, &path(&run_dir.join("template.sock"))),
        "the template alone binds no socket"
    );
    drop(listener);
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
