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
