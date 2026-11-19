// -- ADR-182 Amendment 8: git.status and git.log ------------------------------------------------

/// Native porcelain v2 records, headers dropped, so a count here is the population `total` claims.
fn native_status_paths(fixture: &Fixture, untracked: &str) -> Vec<String> {
    let bytes = fixture.git_bytes(&[
        "status",
        "--porcelain=v2",
        "--branch",
        "--no-renames",
        &format!("--untracked-files={untracked}"),
        "-z",
    ]);
    String::from_utf8(bytes)
        .expect("porcelain UTF-8 in this fixture")
        .split('\0')
        .filter(|record| !record.is_empty() && !record.starts_with("# "))
        .map(|record| {
            let (token, rest) = record.split_once(' ').unwrap_or((record, ""));
            match token {
                "?" | "!" => rest.to_string(),
                "1" => rest.splitn(8, ' ').nth(7).unwrap_or_default().to_string(),
                "u" => rest.splitn(10, ' ').nth(9).unwrap_or_default().to_string(),
                _ => panic!("unhandled porcelain token {token:?}"),
            }
        })
        .collect()
}

fn status_paths(result: &Value) -> Vec<String> {
    result["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .map(|entry| entry["path"].as_str().expect("entry path").to_string())
        .collect()
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn status_agrees_with_native_porcelain_and_changes_nothing_on_disk() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    for verb in ["git.status", "git.log"] {
        f.policy(verb, "allow").await;
    }
    let clean = f.call("git.status", json!({"repo": f.repo})).await;
    assert_eq!(clean["clean"], json!(true), "{clean}");
    assert_eq!(clean["total"], json!(0));
    assert_eq!(clean["branch"]["head"], json!("work"));
    assert_eq!(clean["branch"]["oid"], json!(f.base));
    assert_eq!(clean["gate"]["source"], json!("git_write.allowed"));

    std::fs::write(f.repo.join("a.txt"), b"changed\n").expect("dirty a tracked file");
    std::fs::write(f.repo.join("fresh.txt"), b"new\n").expect("an untracked file");
    let index_before = std::fs::read(f.repo.join(".git/index")).expect("index before");
    let dirty = f
        .call("git.status", json!({"repo": f.repo, "untracked": "all"}))
        .await;
    let index_after = std::fs::read(f.repo.join(".git/index")).expect("index after");

    assert_eq!(
        index_before, index_after,
        "git.status refreshed the index; GIT_OPTIONAL_LOCKS=0 is what keeps this read inert"
    );
    assert_eq!(dirty["clean"], json!(false));
    let mut mine = status_paths(&dirty);
    let mut native = native_status_paths(&f, "all");
    mine.sort();
    native.sort();
    assert_eq!(mine, native, "khive and native porcelain disagree");
    assert_eq!(dirty["total"], json!(native.len()));
    assert_eq!(dirty["truncated"], json!(false));
    assert!(
        status_paths(&dirty).contains(&"fresh.txt".to_string()),
        "untracked=all must report an untracked file: {dirty}"
    );
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn status_total_counts_the_whole_repository_even_when_entries_are_capped() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.status", "allow").await;
    for n in 0..7 {
        std::fs::write(f.repo.join(format!("u{n}.txt")), b"x\n").expect("untracked file");
    }
    let capped = f
        .call(
            "git.status",
            json!({"repo": f.repo, "untracked": "all", "limit": 2}),
        )
        .await;
    assert_eq!(capped["entries"].as_array().expect("entries").len(), 2);
    assert_eq!(capped["total"], json!(7), "{capped}");
    assert_eq!(capped["truncated"], json!(true));
    assert_eq!(
        capped["clean"],
        json!(false),
        "a capped page must never report a clean repository"
    );
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn status_keeps_a_path_holding_a_newline_a_space_and_non_ascii_as_one_entry() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.status", "allow").await;
    // A space would split a whitespace parser, a newline would split a line parser, and the CJK
    // and accented characters catch a byte-wise parser that assumes ASCII. `-z` is the only
    // reason all three survive as one record.
    let awkward = "a file\nwith a newline \u{4e2d}\u{6587} caf\u{e9}.txt";
    std::fs::write(f.repo.join(awkward), b"x\n").expect("awkward untracked name");
    let result = f
        .call("git.status", json!({"repo": f.repo, "untracked": "all"}))
        .await;
    assert_eq!(result["total"], json!(1), "{result}");
    assert_eq!(status_paths(&result), vec![awkward.to_string()]);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn status_reports_a_detached_head_as_a_null_branch_beside_a_real_sha() {
    if crate::test_process::run_in_child() {
        return;
    }

    let f = Fixture::new(true, true).await;
    f.policy("git.status", "allow").await;
    let attached = f.call("git.status", json!({"repo": f.repo})).await;
    assert_eq!(
        attached["branch"]["head"],
        json!("work"),
        "control: {attached}"
    );

    output(
        command(
            &f.git,
            &f.repo,
            &["checkout", "-q", "--detach", "HEAD"],
            true,
        ),
        None,
    );
    // #2539: the verb call used to follow `checkout --detach` with nothing
    // between them and intermittently refused `git_failed` on the macOS runner.
    // These two reads are a barrier as well as an assertion: they prove the
    // detach landed, so a failure below is about `git.status` reading a settled
    // repository rather than one still being written.
    assert_eq!(
        f.git_text(&["rev-parse", "--verify", "HEAD"]),
        f.base,
        "the detached HEAD must resolve to the same commit before status reads it"
    );
    assert_eq!(
        f.git_text(&["branch", "--show-current"]),
        "",
        "the fixture must be detached before the verb is called"
    );

    let detached = f.call("git.status", json!({"repo": f.repo})).await;
    assert_eq!(
        detached["branch"]["head"],
        json!(null),
        "a detached head is an absent branch name, never the porcelain sentinel: {detached}"
    );
    assert_eq!(
        detached["branch"]["oid"],
        json!(f.base),
        "the sha is still readable while detached: {detached}"
    );
}
