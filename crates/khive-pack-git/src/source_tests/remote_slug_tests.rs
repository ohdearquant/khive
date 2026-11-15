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
        // Authority ports do not change repository identity for
        // https, http, or git URLs.
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
