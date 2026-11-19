#[tokio::test]
async fn repo_identity_https_and_ssh_spellings_of_same_remote_converge() {
    let https = DigestSource::Remote {
        canonical: "https://github.com/org/repo".to_string(),
        gh_slug: Some(("org".to_string(), "repo".to_string())),
    };
    assert_eq!(repo_identity(&https).await, "github.com/org/repo");
}

#[tokio::test]
async fn repo_identity_unsluggable_remote_fallback_is_redacted_and_converges() {
    // A single-path-segment URL does not normalize to a slug; the raw-URL
    // identity fallback must never carry userinfo/query/fragment, and
    // query-only spelling variants must converge on one identity. Drive
    // the accepted public source parser rather than constructing the
    // enum directly so this also pins the input-surface contract.
    let with_secret = parse_source("https://user:tok3n@example.com/repo?token=SECRET#frag")
        .expect("accepted unsluggable HTTPS source");
    let identity = repo_identity(&with_secret).await;
    assert_eq!(identity, "https://example.com/repo");
    assert!(!identity.contains("tok3n") && !identity.contains("SECRET"));

    let bare = parse_source("https://example.com/repo").expect("accepted bare HTTPS source");
    assert_eq!(repo_identity(&bare).await, identity);
}

#[tokio::test]
async fn repo_identity_unsluggable_remote_fallback_canonicalizes_equivalent_spellings() {
    let spellings = [
        "https://user:tok3n@example.com/repo",
        "https://example.com/repo/",
        "https://example.com/repo?view=compact",
        "https://example.com/repo.git?token=SECRET",
        "https://example.com/repo/?view=compact#top",
        "https://example.com/repo.git/?view=compact",
    ];
    for spelling in spellings {
        let source = parse_source(spelling).expect("accepted unsluggable HTTPS source");
        assert_eq!(
            repo_identity(&source).await,
            "https://example.com/repo",
            "equivalent fallback spelling must converge: {spelling:?}"
        );
    }

    let source = parse_source("https://user:tok3n@example.com/repo.git?token=SECRET")
        .expect("accepted credential-bearing HTTPS source");
    let DigestSource::Remote { canonical, .. } = source else {
        panic!("expected remote source");
    };
    assert!(
        canonical.contains("tok3n") && canonical.contains("SECRET"),
        "identity redaction must not mutate the in-memory clone URL"
    );
}

#[tokio::test]
async fn repo_identity_unsluggable_remote_fallback_lowercases_host_only() {
    let mixed_case_host = parse_source("https://Example.COM/Repo")
        .expect("accepted mixed-case-host unsluggable HTTPS source");
    let lowercase_host = parse_source("https://example.com/Repo")
        .expect("accepted lowercase-host unsluggable HTTPS source");
    let mixed_identity = repo_identity(&mixed_case_host).await;
    assert_eq!(
        mixed_identity,
        repo_identity(&lowercase_host).await,
        "mixed-case and lowercase host spellings of the same remote must converge"
    );
    assert_eq!(
        mixed_identity, "https://example.com/Repo",
        "host must be lowercased while path case is preserved"
    );

    let different_path = parse_source("https://example.com/OtherRepo")
        .expect("accepted lowercase-host unsluggable HTTPS source with a different path");
    assert_ne!(
        repo_identity(&different_path).await,
        mixed_identity,
        "differing paths must not collapse onto the same anchor"
    );
}

#[test]
fn redact_repo_url_trims_recognized_remotes_but_not_local_paths() {
    assert_eq!(
        redact_repo_url("  https://user:tok3n@github.com/org/repo.git?token=SECRET  "),
        "https://github.com/org/repo.git"
    );
    assert_eq!(
        redact_repo_url("  git@github.com:org/repo.git?token=SECRET  "),
        "github.com:org/repo.git"
    );
    assert_eq!(
        redact_repo_url(" /tmp/a repo "),
        " /tmp/a repo ",
        "local filesystem spelling is display metadata and must not be indiscriminately trimmed"
    );
}

#[test]
fn redact_repo_url_preserves_question_mark_in_local_path() {
    assert_eq!(redact_repo_url("/tmp/repo?worktree"), "/tmp/repo?worktree");
}

#[test]
fn redact_repo_url_preserves_hash_in_local_path() {
    assert_eq!(redact_repo_url("/tmp/repo#worktree"), "/tmp/repo#worktree");
}

#[test]
fn repo_basename_strips_query_and_fragment() {
    let src = DigestSource::Remote {
        canonical: "https://example.com/org/repo?x=1#frag".to_string(),
        gh_slug: None,
    };
    assert_eq!(repo_basename(&src), "repo");
}

#[test]
fn repo_basename_never_carries_userinfo_for_root_path_remotes() {
    let src = DigestSource::Remote {
        canonical: "https://user:tok3n@example.com".to_string(),
        gh_slug: None,
    };
    let name = repo_basename(&src);
    assert_eq!(name, "example.com");
    assert!(!name.contains("tok3n"));
}

#[tokio::test]
async fn repo_identity_local_path_with_origin_matches_remote_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_origin(dir.path(), "git@github.com:org/repo.git");

    let local = DigestSource::Local(dir.path().to_path_buf());
    let remote = DigestSource::Remote {
        canonical: "https://github.com/org/repo".to_string(),
        gh_slug: Some(("org".to_string(), "repo".to_string())),
    };
    assert_eq!(repo_identity(&local).await, repo_identity(&remote).await);
    assert_eq!(repo_identity(&local).await, "github.com/org/repo");
}

/// Round-2 finding: a local clone whose `origin` is bracketed-IPv6 SCP
/// shorthand must converge with the equivalent `ssh://` spelling's
/// slug, not fall back to `local:<path>`.
#[tokio::test]
async fn repo_identity_local_path_with_bracketed_ipv6_scp_origin_converges() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_origin(dir.path(), "git@[::1]:group/repo.git");

    let local = DigestSource::Local(dir.path().to_path_buf());
    let remote = DigestSource::Remote {
        canonical: "https://[::1]/group/repo".to_string(),
        gh_slug: None,
    };
    assert_eq!(repo_identity(&local).await, "[::1]/group/repo");
    assert_eq!(repo_identity(&local).await, repo_identity(&remote).await);
}

#[tokio::test]
async fn repo_identity_local_path_without_remote_falls_back_to_path_form() {
    let dir = tempfile::tempdir().expect("tempdir");
    let status = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["init", "-q"])
        .status()
        .expect("spawn git init");
    assert!(status.success());

    let local = DigestSource::Local(dir.path().to_path_buf());
    let identity = repo_identity(&local).await;
    assert!(identity.starts_with("local:"), "{identity}");
    let canon = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(identity, format!("local:{}", canon.to_string_lossy()));
}
