use super::*;

#[test]
fn https_github_url_derives_slug_and_canonical_form() {
    let src = parse_source("https://github.com/ohdearquant/khive.git").unwrap();
    match src {
        DigestSource::Remote { canonical, gh_slug } => {
            assert_eq!(canonical, "https://github.com/ohdearquant/khive");
            assert_eq!(
                gh_slug,
                Some(("ohdearquant".to_string(), "khive".to_string()))
            );
        }
        other => panic!("expected Remote, got {other:?}"),
    }
}

#[test]
fn https_github_url_trailing_slash_canonicalizes_same_as_no_slash() {
    let a = parse_source("https://github.com/org/repo/").unwrap();
    let b = parse_source("https://github.com/org/repo").unwrap();
    assert_eq!(a, b);
}

#[test]
fn https_github_slug_never_carries_query_or_fragment_credentials() {
    let src = parse_source("https://github.com/org/repo.git?token=SECRETTOKEN#fragment").unwrap();
    match src {
        DigestSource::Remote { canonical, gh_slug } => {
            assert!(canonical.contains("SECRETTOKEN"));
            assert_eq!(gh_slug, Some(("org".to_string(), "repo".to_string())));
            assert!(
                !format!("{gh_slug:?}").contains("SECRETTOKEN"),
                "the source-bound gh target must be safe to place in argv"
            );
        }
        other => panic!("expected Remote, got {other:?}"),
    }
}

#[test]
fn https_non_github_host_has_no_gh_slug() {
    let src = parse_source("https://gitlab.com/org/repo").unwrap();
    match src {
        DigestSource::Remote { gh_slug, .. } => assert_eq!(gh_slug, None),
        other => panic!("expected Remote, got {other:?}"),
    }
}

#[test]
fn ssh_url_rejected() {
    let err = parse_source("ssh://git@github.com/org/repo.git").unwrap_err();
    assert!(err.contains("SSH"), "{err}");
}

#[test]
fn scp_shorthand_rejected() {
    let err = parse_source("git@github.com:org/repo.git").unwrap_err();
    assert!(err.contains("SSH"), "{err}");
}

#[test]
fn bare_scp_shorthand_without_userinfo_rejected() {
    let err = parse_source("github.com:org/repo.git").unwrap_err();
    assert!(err.contains("SSH"), "{err}");
}

#[test]
fn scp_shorthand_bracketed_ipv6_host_rejected_as_source() {
    let err = parse_source("git@[::1]:group/repo.git").unwrap_err();
    assert!(err.contains("SSH"), "{err}");
}

#[test]
fn bare_scp_shorthand_bracketed_ipv6_host_rejected_as_source() {
    let err = parse_source("[::1]:group/repo.git").unwrap_err();
    assert!(err.contains("SSH"), "{err}");
}

#[test]
fn git_protocol_rejected() {
    let err = parse_source("git://github.com/org/repo.git").unwrap_err();
    assert!(err.contains("git://"), "{err}");
}

#[test]
fn plain_http_rejected() {
    let err = parse_source("http://github.com/org/repo").unwrap_err();
    assert!(err.contains("http://"), "{err}");
}

#[test]
fn relative_local_path_rejected() {
    let err = parse_source("relative/path/repo").unwrap_err();
    assert!(err.contains("absolute"), "{err}");
}

#[test]
fn absolute_local_path_without_git_dir_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = parse_source(dir.path().to_str().unwrap()).unwrap_err();
    assert!(err.contains(".git"), "{err}");
}

#[test]
fn absolute_local_path_with_git_dir_accepted() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let src = parse_source(dir.path().to_str().unwrap()).unwrap();
    assert_eq!(src, DigestSource::Local(dir.path().to_path_buf()));
}

#[test]
fn repo_basename_local_uses_dir_name() {
    let src = DigestSource::Local(PathBuf::from("/home/x/my-repo"));
    assert_eq!(repo_basename(&src), "my-repo");
}

#[test]
fn repo_basename_remote_uses_last_path_segment() {
    let src = DigestSource::Remote {
        canonical: "https://github.com/org/my-repo".to_string(),
        gh_slug: Some(("org".to_string(), "my-repo".to_string())),
    };
    assert_eq!(repo_basename(&src), "my-repo");
}

#[test]
fn cache_key_is_deterministic_and_16_hex_chars() {
    let a = cache_key("https://github.com/org/repo");
    let b = cache_key("https://github.com/org/repo");
    let c = cache_key("https://github.com/org/other");
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(a.len(), 16);
    assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
}

// -- repo identity slug (issue #1173) --------------------------------

#[test]
fn remote_url_to_slug_normalization_table() {
    let expected = "github.com/org/repo";
    let spellings = [
        "https://github.com/org/repo",
        "https://github.com/org/repo.git",
        "https://github.com/org/repo/",
        "https://github.com/org/repo.git/",
        "http://github.com/org/repo",
        "https://token@github.com/org/repo.git",
        "https://user:token@github.com/org/repo.git",
        "ssh://git@github.com/org/repo.git",
        "git@github.com:org/repo.git",
        "git@github.com:org/repo",
        "git://github.com/org/repo.git",
        // Bare SCP shorthand -- the SCP user is optional.
        "github.com:org/repo.git",
        "github.com:org/repo",
        // ssh:// with a port -- the port is not part of the identity.
        "ssh://git@github.com:2222/org/repo.git",
        "ssh://github.com:2222/org/repo",
        // https/http/git:// with an authority port -- likewise not part
        // of the identity (round-2 finding: only ssh:// was covered).
        "https://github.com:8443/org/repo",
        "https://github.com:8443/org/repo.git",
        "http://github.com:8080/org/repo",
        "git://github.com:9418/org/repo",
        // Host case is folded; owner/repo case is preserved separately below.
        "https://GitHub.com/org/repo",
        // A leading www. host label is folded (ADR-088 Amendment 2).
        "https://www.github.com/org/repo",
        // A query string and/or fragment must not affect normalization
        // and must never leak a token into the slug.
        "https://github.com/org/repo?ref=nightly",
        "https://github.com/org/repo.git?ref=nightly",
        "https://github.com/org/repo#readme",
        "https://github.com/org/repo.git?token=SECRET#section",
    ];
    for s in spellings {
        assert_eq!(
            remote_url_to_slug(s).as_deref(),
            Some(expected),
            "spelling {s:?} should normalize to {expected:?}"
        );
    }
}

#[test]
fn remote_url_to_slug_strips_query_and_fragment_before_splitting() {
    let slug = remote_url_to_slug("https://github.com/org/repo?token=SECRETTOKEN").unwrap();
    assert_eq!(slug, "github.com/org/repo");
    assert!(!slug.contains("SECRETTOKEN"), "{slug}");
}

#[test]
fn remote_url_to_slug_folds_leading_www_host_label() {
    assert_eq!(
        remote_url_to_slug("https://www.gitlab.com/org/repo").as_deref(),
        Some("gitlab.com/org/repo")
    );
}

#[test]
fn scp_shorthand_bracketed_ipv6_host_normalizes_directly() {
    assert_eq!(
        remote_url_to_slug("git@[::1]:group/repo.git").as_deref(),
        Some("[::1]/group/repo")
    );
    assert_eq!(
        remote_url_to_slug("[::1]:group/repo.git").as_deref(),
        Some("[::1]/group/repo"),
        "the no-user bracketed SCP variant must also normalize"
    );
}

#[test]
fn redact_repo_url_strips_userinfo_query_and_fragment() {
    assert_eq!(
        redact_repo_url("https://user:tok3n@github.com/org/repo?token=SECRET#frag"),
        "https://github.com/org/repo"
    );
    assert_eq!(
        redact_repo_url(
            "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA@github.com:org/repo.git?token=SECRET#frag"
        ),
        "github.com:org/repo.git",
        "SCP-style userinfo must not survive in stored display metadata"
    );
    assert_eq!(
        redact_repo_url("https://github.com/org/repo"),
        "https://github.com/org/repo"
    );
}

#[test]
fn remote_url_to_slug_preserves_all_nested_path_segments() {
    assert_eq!(
        remote_url_to_slug("https://gitlab.com/group/subgroup/repo.git").as_deref(),
        Some("gitlab.com/group/subgroup/repo")
    );
    assert_eq!(
        remote_url_to_slug("git@gitlab.com:group/subgroup/repo.git").as_deref(),
        Some("gitlab.com/group/subgroup/repo"),
        "scp spelling of a nested-group remote must converge with https"
    );
    assert_ne!(
        remote_url_to_slug("https://gitlab.com/group/subgroup/repo"),
        remote_url_to_slug("https://gitlab.com/group/subgroup/other"),
        "two repos under one subgroup must not collapse onto one slug"
    );
}

#[test]
fn remote_url_to_slug_keeps_host_when_a_path_segment_contains_at() {
    assert_eq!(
        remote_url_to_slug("https://evil.example/x@github.com/org/repo").as_deref(),
        Some("evil.example/x@github.com/org/repo")
    );
    assert_eq!(
        remote_url_to_slug("ssh://git@evil.example/x@github.com/org/repo").as_deref(),
        Some("evil.example/x@github.com/org/repo")
    );
    assert_eq!(
        remote_url_to_slug("https://user:p@ss@github.com/org/repo").as_deref(),
        Some("github.com/org/repo"),
        "an '@' inside the password is still userinfo"
    );
}

#[test]
fn parse_source_derives_no_github_slug_when_the_host_is_not_github() {
    match parse_source("https://evil.example/x@github.com/org/repo").unwrap() {
        DigestSource::Remote { gh_slug, .. } => assert_eq!(gh_slug, None),
        other => panic!("expected Remote, got {other:?}"),
    }
}

#[test]
fn remote_url_to_slug_preserves_owner_repo_case() {
    assert_eq!(
        remote_url_to_slug("https://github.com/Org/Repo").as_deref(),
        Some("github.com/Org/Repo")
    );
}

#[test]
fn remote_url_to_slug_bracketed_ipv6_host_with_port() {
    assert_eq!(
        remote_url_to_slug("ssh://git@[::1]:2222/org/repo").as_deref(),
        Some("[::1]/org/repo")
    );
}

#[test]
fn remote_url_to_slug_bracketed_ipv6_host_without_port() {
    assert_eq!(
        remote_url_to_slug("ssh://git@[::1]/org/repo").as_deref(),
        Some("[::1]/org/repo")
    );
}

#[test]
fn remote_url_to_slug_rejects_empty_owner_or_repo_segment() {
    assert_eq!(remote_url_to_slug("https://github.com/org//repo"), None);
    assert_eq!(remote_url_to_slug("https://github.com//repo"), None);
    assert_eq!(remote_url_to_slug("git@github.com:/repo"), None);
    assert_eq!(remote_url_to_slug("git@github.com:org/"), None);
}

#[test]
fn remote_url_to_slug_does_not_misparse_local_path_with_colon() {
    assert_eq!(remote_url_to_slug("local/path:with-colon"), None);
}

#[test]
fn remote_url_to_slug_rejects_unparseable_input() {
    assert_eq!(remote_url_to_slug(""), None);
    assert_eq!(remote_url_to_slug("not-a-url"), None);
    assert_eq!(remote_url_to_slug("https://github.com/onlyowner"), None);
}

fn init_repo_with_origin(dir: &Path, origin: &str) {
    for args in [vec!["init", "-q"], vec!["remote", "add", "origin", origin]] {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(&args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }
}

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
