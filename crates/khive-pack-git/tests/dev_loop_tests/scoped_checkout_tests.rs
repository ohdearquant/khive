/// Commit the layout the scoped checkout arms read: a directory with a nested file, a file beside
/// it whose name begins with the directory's, and the fixture's own `a.txt`. Returns the commit.
fn scoped_layout(f: &Fixture) -> String {
    std::fs::create_dir_all(f.repo.join("docs/sub")).expect("docs directory");
    std::fs::write(f.repo.join("docs/one.md"), b"one\n").expect("docs/one.md");
    std::fs::write(f.repo.join("docs/sub/two.md"), b"two\n").expect("docs/sub/two.md");
    std::fs::write(f.repo.join("docs-index.md"), b"index\n").expect("docs-index.md");
    std::fs::create_dir(f.repo.join("src")).expect("src directory");
    std::fs::write(f.repo.join("src/lib.rs"), b"lib\n").expect("src/lib.rs");
    f.git_bytes(&["add", "-A"]);
    f.git_bytes(&["commit", "-q", "-m", "scoped layout"]);
    f.git_text(&["rev-parse", "HEAD"])
}

/// The entries of a tree and its scope (null for a whole tree), as `exec.tree_get` reports them.
async fn tree_entries(f: &Fixture, tree: &Value) -> (Vec<Value>, Value) {
    let read = f.call("exec.tree_get", json!({"tree":tree})).await;
    let entries = read["entries"].as_array().expect("tree entries").clone();
    (entries, read["scope"].clone())
}

fn entry_paths(entries: &[Value]) -> Vec<String> {
    let mut paths: Vec<String> = entries
        .iter()
        .map(|entry| entry["path"].as_str().expect("entry path").to_string())
        .collect();
    paths.sort();
    paths
}

/// ADR-182 Amendment 24 arm 122: a checkout naming one directory and one file holds exactly the
/// entries at or beneath them, at their full paths and with their bytes, and its receipt names
/// both paths in sorted order. The whole checkout of the same commit holds all of them and more.
#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn amendment24_arm122_a_checkout_of_named_paths_holds_exactly_their_entries() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, false).await;
    let head = scoped_layout(&f);
    let result = f
        .call(
            "git.checkout",
            json!({"repo":f.repo,"ref":head,"paths":["docs","a.txt"]}),
        )
        .await;
    assert_eq!(result["commit"], head, "{result}");
    assert_eq!(result["paths"], json!(["a.txt", "docs"]), "{result}");
    let (entries, scope) = tree_entries(&f, &result["tree"]).await;
    assert_eq!(scope, json!(["a.txt", "docs"]));
    assert_eq!(
        entry_paths(&entries),
        ["a.txt", "docs/one.md", "docs/sub/two.md"]
    );
    for (path, bytes) in [
        ("a.txt", &b"old\n"[..]),
        ("docs/one.md", &b"one\n"[..]),
        ("docs/sub/two.md", &b"two\n"[..]),
    ] {
        let entry = entries
            .iter()
            .find(|entry| entry["path"] == path)
            .expect("listed entry");
        assert_eq!(
            f.blob(entry["ref"].as_str().unwrap()).await,
            bytes,
            "{path}"
        );
    }
    let receipt = f.success_receipt(&result).await;
    assert_eq!(
        receipt["inputs"]["paths"],
        json!(["a.txt", "docs"]),
        "{receipt}"
    );

    // Control: the whole checkout holds every scoped entry, byte for byte, and more.
    let whole = f
        .call("git.checkout", json!({"repo":f.repo,"ref":head}))
        .await;
    assert!(whole.get("paths").is_none(), "{whole}");
    let (all, whole_scope) = tree_entries(&f, &whole["tree"]).await;
    assert!(whole_scope.is_null(), "a whole checkout has no scope");
    for entry in &entries {
        assert!(all.contains(entry), "{entry} is not in the whole tree");
    }
    assert!(all.len() > entries.len());
}

/// ADR-182 Amendment 24 arm 123: a malformed path refuses `invalid_path` and a path inside another
/// `overlapping_paths`, before git runs; a path that names nothing refuses `path_not_found`, naming
/// it, before any blob is read.
#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn amendment24_arm123_named_paths_refuse_before_git_runs_or_before_any_blob_read() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, false).await;
    let head = scoped_layout(&f);
    let bin = f.dir.path().join("git-call-log");
    std::fs::create_dir(&bin).expect("shim directory");
    let log = f.dir.path().join("git.calls");
    executable(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\nexec {git} \"$@\"\n",
            log = quoted(&log),
            git = quoted(&f.git),
        ),
    );
    let mut search = vec![bin];
    search.extend(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH"),
    ));
    let calls = || std::fs::read_to_string(&log).unwrap_or_default();
    let checkout = |paths: &Value| json!({"repo":f.repo,"ref":head,"paths":paths});

    let before_git = [
        ("invalid_path", json!(["/a.txt"])),
        ("invalid_path", json!([""])),
        ("invalid_path", json!(["docs//one.md"])),
        ("invalid_path", json!(["./a.txt"])),
        ("invalid_path", json!(["docs/./one.md"])),
        ("invalid_path", json!(["../a.txt"])),
        ("invalid_path", json!(["sub/.."])),
        ("invalid_path", json!(["sub/"])),
        ("invalid_path", json!(["a\u{0}b"])),
        ("overlapping_paths", json!(["docs", "docs/one.md"])),
        ("overlapping_paths", json!(["docs/sub/two.md", "docs"])),
        ("overlapping_paths", json!(["a.txt", "a.txt"])),
    ];
    let too_many: Vec<String> = (0..65).map(|index| format!("f{index}")).collect();
    let malformed = [json!([]), json!("a.txt"), json!(too_many)];
    let not_found = [
        (json!(["a.txt", "missing/file.txt"]), "missing/file.txt"),
        // The start of a directory's name is not a path of its own.
        (json!(["doc"]), "doc"),
    ];
    let mut refused = Vec::new();
    let mut missing = Vec::new();
    {
        let _env = EnvGuard::set(&[("PATH", std::env::join_paths(search).expect("shim PATH"))]);
        for (reason, paths) in &before_git {
            refused.push((*reason, paths, f.err("git.checkout", checkout(paths)).await));
        }
        for paths in &malformed {
            let error = f.err("git.checkout", checkout(paths)).await;
            assert!(error.contains("invalid"), "{paths}: {error}");
        }
        assert_eq!(
            calls(),
            "",
            "a malformed path set must refuse before git runs"
        );
        for (paths, path) in &not_found {
            missing.push((*path, f.err("git.checkout", checkout(paths)).await));
        }
    }
    let seen = calls();
    assert!(
        seen.contains("ls-tree"),
        "the shim must see the listing: {seen}"
    );
    assert!(
        !seen.contains("cat-file"),
        "a path that names nothing must refuse before any blob is read: {seen}"
    );
    for (reason, paths, error) in &refused {
        assert!(error.contains(reason), "{paths}: {error}");
        let receipt = f.refusal_receipt(error).await;
        assert_eq!(receipt["reason"], *reason, "{paths}: {receipt}");
    }
    for (path, error) in &missing {
        assert!(error.contains(&format!("{path:?}")), "{error}");
        let receipt = f.refusal_receipt(error).await;
        assert_eq!(receipt["reason"], "path_not_found", "{receipt}");
    }

    // Control: a path whose name begins with another's is beside it, not inside it.
    let result = f
        .call("git.checkout", checkout(&json!(["docs-index.md", "docs"])))
        .await;
    assert_eq!(
        result["paths"],
        json!(["docs", "docs-index.md"]),
        "{result}"
    );
    let (entries, _) = tree_entries(&f, &result["tree"]).await;
    assert_eq!(
        entry_paths(&entries),
        ["docs-index.md", "docs/one.md", "docs/sub/two.md"]
    );
}

/// ADR-182 Amendment 24 arm 124: a named path is matched as its literal text. A name holding glob
/// characters reads only itself, and pathspec magic is a name like any other.
#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn amendment24_arm124_a_named_path_is_matched_as_its_literal_text() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, false).await;
    for name in [
        "g[ab].txt",
        "ga.txt",
        "star*.md",
        "starx.md",
        "q?.md",
        "qq.md",
    ] {
        std::fs::write(f.repo.join(name), name.as_bytes()).expect("glob-named file");
    }
    f.git_bytes(&["add", "-A"]);
    f.git_bytes(&["commit", "-q", "-m", "glob names"]);
    let head = f.git_text(&["rev-parse", "HEAD"]);
    for name in ["g[ab].txt", "star*.md", "q?.md"] {
        let result = f
            .call(
                "git.checkout",
                json!({"repo":f.repo,"ref":head,"paths":[name]}),
            )
            .await;
        let (entries, _) = tree_entries(&f, &result["tree"]).await;
        assert_eq!(entry_paths(&entries), [name], "{name}");
        assert_eq!(
            f.blob(entries[0]["ref"].as_str().unwrap()).await,
            name.as_bytes()
        );
    }

    // Control: native git without literal pathspecs reads this magic as the file a.txt.
    let magic = ":(top)a.txt";
    assert_eq!(
        f.git_text(&["ls-tree", "-r", "--name-only", &head, "--", magic]),
        "a.txt"
    );
    let error = f
        .err(
            "git.checkout",
            json!({"repo":f.repo,"ref":head,"paths":[magic]}),
        )
        .await;
    assert!(error.contains("path_not_found"), "{error}");
    assert!(error.contains(&format!("{magic:?}")), "{error}");
}

/// ADR-182 Amendment 24 arm 125: a commit whose whole tree exceeds the manifest limit refuses
/// `output_limit` naming `paths`, and a checkout of named paths reads it.
#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn amendment24_arm125_a_tree_over_the_manifest_limit_checks_out_by_paths() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, false).await;
    let blob = f.git_text(&["rev-parse", "HEAD:a.txt"]);
    let make_tree = |listing: &[u8]| {
        String::from_utf8(output(command(&f.git, &f.repo, &["mktree"], true), Some(listing)).stdout)
            .expect("tree id")
            .trim()
            .to_string()
    };
    // 60000 entries of about 200 manifest bytes each: past the 8 MiB manifest limit, with every
    // entry naming the same blob.
    let mut listing = Vec::new();
    for index in 0..60000 {
        listing.extend_from_slice(
            format!("100644 blob {blob}\t{index:06}-{}\n", "x".repeat(100)).as_bytes(),
        );
    }
    let big = make_tree(&listing);
    let mut root = f.git_bytes(&["ls-tree", "HEAD"]);
    root.extend_from_slice(format!("040000 tree {big}\tbig\n").as_bytes());
    let root = make_tree(&root);
    let head = String::from_utf8(
        output(
            command(
                &f.git,
                &f.repo,
                &["commit-tree", &root, "-p", "HEAD", "-m", "large tree"],
                true,
            ),
            None,
        )
        .stdout,
    )
    .expect("commit id")
    .trim()
    .to_string();

    let error = f
        .err("git.checkout", json!({"repo":f.repo,"ref":head}))
        .await;
    assert!(error.contains("output_limit"), "{error}");
    assert!(error.contains("may name paths"), "{error}");
    f.refusal_receipt(&error).await;

    let error = f
        .err(
            "git.checkout",
            json!({"repo":f.repo,"ref":head,"paths":["big"]}),
        )
        .await;
    assert!(error.contains("output_limit"), "{error}");
    assert!(error.contains("named paths"), "{error}");

    let result = f
        .call(
            "git.checkout",
            json!({"repo":f.repo,"ref":head,"paths":["a.txt","removed.txt"]}),
        )
        .await;
    let (entries, scope) = tree_entries(&f, &result["tree"]).await;
    assert_eq!(scope, json!(["a.txt", "removed.txt"]));
    assert_eq!(entry_paths(&entries), ["a.txt", "removed.txt"]);
}

/// ADR-182 Amendment 24 arm 127: a link named by its path, or inside a named directory, keeps mode
/// 120000 and its literal target.
#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn amendment24_arm127_a_scoped_link_keeps_its_mode_and_literal_target() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, false).await;
    std::fs::create_dir(f.repo.join("docs")).expect("docs directory");
    std::fs::write(f.repo.join("docs/one.md"), b"one\n").expect("docs/one.md");
    std::os::unix::fs::symlink("../a.txt", f.repo.join("docs/link")).expect("link fixture");
    f.git_bytes(&["add", "-A"]);
    f.git_bytes(&["commit", "-q", "-m", "scoped link"]);
    let head = f.git_text(&["rev-parse", "HEAD"]);
    for paths in [json!(["docs/link"]), json!(["docs"])] {
        let result = f
            .call(
                "git.checkout",
                json!({"repo":f.repo,"ref":head,"paths":paths}),
            )
            .await;
        let (entries, _) = tree_entries(&f, &result["tree"]).await;
        let link = entries
            .iter()
            .find(|entry| entry["path"] == "docs/link")
            .unwrap_or_else(|| panic!("{paths}: the link is an entry"));
        assert_eq!(link["mode"], 120000, "{paths}: {link}");
        assert_eq!(f.blob(link["ref"].as_str().unwrap()).await, b"../a.txt");
    }
}

/// ADR-182 Amendment 24 arm 126, the run case: a run on a scoped checkout writes a scoped
/// `tree_out`, and a commit of that tree refuses `scoped_tree`.
#[cfg(target_os = "macos")]
#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn amendment24_arm126_a_run_on_a_scoped_checkout_writes_a_tree_the_commit_refuses() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::with_exec().await;
    f.policy("git.commit", "allow").await;
    f.call(
        "tool.register",
        json!({
            "name": "sh", "kind": "tool", "description": "shell", "source": "exec:/bin/sh",
            "side_effect": "write", "trust": "first_party"
        }),
    )
    .await;
    f.policy("sh", "allow").await;
    let checkout = f
        .call(
            "git.checkout",
            json!({"repo":f.repo,"ref":f.base,"paths":["a.txt"]}),
        )
        .await;
    let out = f
        .call(
            "exec.run",
            json!({
                "tree": checkout["tree"], "tool": "sh", "args": ["-c", "printf edited > a.txt"],
                "actor": ACTOR, "session_id": "scoped-run"
            }),
        )
        .await;
    let receipt = &out["receipt"];
    assert_eq!(receipt["exit_code"], 0, "{receipt}");
    let tree_out = receipt["tree_out"].as_str().expect("tree_out").to_string();
    let (entries, scope) = tree_entries(&f, &json!(tree_out)).await;
    assert_eq!(scope, json!(["a.txt"]));
    assert_eq!(f.blob(entries[0]["ref"].as_str().unwrap()).await, b"edited");
    let error = f.err("git.commit", f.commit_params(&tree_out)).await;
    let refusal = f.refusal_receipt(&error).await;
    assert_eq!(refusal["reason"], "scoped_tree", "{refusal}");
    assert_eq!(refusal["result"]["scope"], json!(["a.txt"]), "{refusal}");
}
