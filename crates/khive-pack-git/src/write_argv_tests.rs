use super::*;

// -- validate_ref_name --------------------------------------------------

#[test]
fn ref_name_accepts_ordinary_branch_names() {
    assert!(validate_ref_name("name", "feat/adr108-git-write-verbs").is_ok());
    assert!(validate_ref_name("name", "main").is_ok());
    assert!(validate_ref_name("name", "release-1.2.3").is_ok());
}

#[test]
fn ref_name_rejects_empty() {
    assert_eq!(
        validate_ref_name("name", ""),
        Err(GitArgError::Empty("name"))
    );
}

#[test]
fn ref_name_rejects_leading_dash() {
    for injected in ["-f", "--upload-pack=evil", "-- ", "-x"] {
        let err = validate_ref_name("name", injected).unwrap_err();
        assert!(
            matches!(err, GitArgError::LeadingDash(..)),
            "{injected}: {err}"
        );
    }
}

#[test]
fn ref_name_rejects_whitespace() {
    let err = validate_ref_name("name", "feat branch").unwrap_err();
    assert!(matches!(err, GitArgError::InvalidCharacter(..)));
}

#[test]
fn ref_name_rejects_semicolon_injection() {
    let err = validate_ref_name("name", "main;rm -rf /").unwrap_err();
    assert!(matches!(err, GitArgError::InvalidCharacter(..)));
}

#[test]
fn ref_name_rejects_path_traversal() {
    let err = validate_ref_name("name", "../../etc/passwd").unwrap_err();
    assert!(matches!(err, GitArgError::PathTraversal(..)));
}

#[test]
fn ref_name_rejects_double_dot_anywhere() {
    let err = validate_ref_name("name", "feat/foo..bar").unwrap_err();
    assert!(matches!(err, GitArgError::PathTraversal(..)));
}

#[test]
fn ref_name_rejects_leading_and_trailing_slash() {
    assert!(validate_ref_name("name", "/main").is_err());
    assert!(validate_ref_name("name", "main/").is_err());
    assert!(validate_ref_name("name", "a//b").is_err());
}

#[test]
fn ref_name_rejects_leading_dot_and_lock_suffix() {
    assert!(validate_ref_name("name", ".hidden").is_err());
    assert!(validate_ref_name("name", "main.lock").is_err());
    assert!(validate_ref_name("name", "main.").is_err());
}

#[test]
fn ref_name_rejects_at_brace() {
    let err = validate_ref_name("name", "main@{upstream}").unwrap_err();
    assert!(matches!(err, GitArgError::InvalidCharacter(..)));
}

#[test]
fn ref_name_rejects_too_long() {
    let long = "a".repeat(MAX_REF_LEN + 1);
    assert!(matches!(
        validate_ref_name("name", &long),
        Err(GitArgError::TooLong(..))
    ));
}

#[test]
fn ref_name_rejects_null_byte() {
    let err = validate_ref_name("name", "main\0evil").unwrap_err();
    assert!(matches!(err, GitArgError::InvalidCharacter(..)));
}

// -- validate_remote_name ------------------------------------------------

#[test]
fn remote_name_accepts_ordinary_names() {
    assert!(validate_remote_name("origin").is_ok());
    assert!(validate_remote_name("upstream-2").is_ok());
}

#[test]
fn remote_name_rejects_slash() {
    let err = validate_remote_name("org/repo").unwrap_err();
    assert!(matches!(err, GitArgError::InvalidCharacter(..)));
}

#[test]
fn remote_name_rejects_leading_dash_flag_shapes() {
    for injected in ["--upload-pack=/bin/sh", "-o", "--exec=evil"] {
        let err = validate_remote_name(injected).unwrap_err();
        assert!(
            matches!(err, GitArgError::LeadingDash(..)),
            "{injected}: {err}"
        );
    }
}

#[test]
fn remote_name_rejects_whitespace_and_semicolons() {
    assert!(validate_remote_name("origin; rm -rf /").is_err());
    assert!(validate_remote_name("evil host").is_err());
}

// -- validate_commit_path -------------------------------------------------

#[test]
fn commit_path_accepts_ordinary_relative_paths() {
    assert!(validate_commit_path("src/main.rs").is_ok());
    assert!(validate_commit_path("README.md").is_ok());
}

#[test]
fn commit_path_accepts_names_that_are_literalized_before_git() {
    for literal in [
        "--upload-pack=x",
        "a\nb",
        ":(top)a.txt",
        "src/:(top)evil",
        "a\tb",
        "a\u{1b}b",
        "src/m\u{0430}in.rs",
        ":!x",
        "*.rs",
        "?.rs",
        "[ab].rs",
    ] {
        assert!(validate_commit_path(literal).is_ok(), "{literal:?}");
    }
}

#[test]
fn commit_path_rejects_absolute_path() {
    let err = validate_commit_path("/etc/passwd").unwrap_err();
    assert!(matches!(err, GitArgError::InvalidCharacter(..)));
}

#[test]
fn commit_path_rejects_traversal_segment() {
    for injected in ["../../etc/passwd", "a/../../b", ".."] {
        let err = validate_commit_path(injected).unwrap_err();
        assert!(
            matches!(err, GitArgError::PathTraversal(..)),
            "{injected}: {err}"
        );
    }
}

#[test]
fn commit_path_rejects_nul() {
    let err = validate_commit_path("a\0b").unwrap_err();
    assert!(matches!(err, GitArgError::InvalidCharacter(..)));
}

// -- validate_message ------------------------------------------------------

#[test]
fn message_accepts_multiline_text() {
    assert!(validate_message("subject\n\nbody line\nwith more text").is_ok());
}

#[test]
fn message_accepts_leading_dash_since_it_is_never_a_bare_argv_entry() {
    // "-m" binds this as a value, not a new flag -- see doc comment.
    assert!(validate_message("--upload-pack=evil").is_ok());
}

#[test]
fn message_rejects_empty_or_whitespace_only() {
    assert!(validate_message("").is_err());
    assert!(validate_message("   \n  ").is_err());
}

#[test]
fn message_rejects_null_byte() {
    let err = validate_message("hello\0world").unwrap_err();
    assert!(matches!(err, GitArgError::InvalidCharacter(..)));
}

#[test]
fn message_rejects_too_long() {
    let long = "a".repeat(MAX_MESSAGE_LEN + 1);
    assert!(matches!(
        validate_message(&long),
        Err(GitArgError::TooLong(..))
    ));
}

// -- validate_author ---------------------------------------------------

#[test]
fn author_accepts_name_and_email() {
    assert!(validate_author("Test User <test@example.com>").is_ok());
}

#[test]
fn author_rejects_leading_dash() {
    let err = validate_author("--upload-pack=evil").unwrap_err();
    assert!(matches!(err, GitArgError::LeadingDash(..)));
}

#[test]
fn author_rejects_embedded_newline() {
    let err = validate_author("Test User\n<test@example.com>").unwrap_err();
    assert!(matches!(err, GitArgError::InvalidCharacter(..)));
}

// -- reject_force --------------------------------------------------------

#[test]
fn reject_force_denies_explicit_true() {
    assert_eq!(reject_force(Some(true)), Err(GitArgError::ForceDenied));
}

#[test]
fn reject_force_allows_false_or_absent() {
    assert!(reject_force(Some(false)).is_ok());
    assert!(reject_force(None).is_ok());
}

// -- validate_repo_path ----------------------------------------------------

#[test]
fn repo_path_rejects_relative() {
    let err = validate_repo_path(Path::new("relative/repo")).unwrap_err();
    assert!(matches!(err, GitArgError::NotAbsolute(_)));
}

#[test]
fn repo_path_rejects_missing_git_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = validate_repo_path(dir.path()).unwrap_err();
    assert!(matches!(err, GitArgError::NotARepo(_)));
}

#[test]
fn repo_path_accepts_dir_with_git_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    assert!(validate_repo_path(dir.path()).is_ok());
}

// -- build_add_argv ------------------------------------------------------

#[test]
fn build_add_argv_fixed_shape() {
    let argv = build_add_argv(&["a.rs".to_string(), "b/c.rs".to_string()]).unwrap();
    assert_eq!(
        argv,
        vec!["add", "--", ":(literal)a.rs", ":(literal)b/c.rs"]
    );
}

#[test]
fn build_add_argv_literalizes_caller_paths() {
    let argv = build_add_argv(&[":(top)".to_string(), "*.rs".to_string()]).unwrap();
    assert_eq!(
        argv,
        vec!["add", "--", ":(literal):(top)", ":(literal)*.rs"]
    );
}

// -- build_commit_argv -----------------------------------------------------

#[test]
fn build_commit_argv_no_paths_uses_dash_a() {
    let argv = build_commit_argv("fix: thing", &[], None).unwrap();
    assert_eq!(argv, vec!["commit", "-a", "-m", "fix: thing"]);
}

#[test]
fn build_commit_argv_with_paths_scopes_and_appends_dashdash() {
    let argv = build_commit_argv("fix: thing", &["src/lib.rs".to_string()], None).unwrap();
    assert_eq!(
        argv,
        vec!["commit", "-m", "fix: thing", "--", ":(literal)src/lib.rs"]
    );
}

#[test]
fn build_commit_argv_with_author() {
    let argv = build_commit_argv("msg", &[], Some("Test User <test@example.com>")).unwrap();
    assert_eq!(
        argv,
        vec![
            "commit",
            "-a",
            "--author=Test User <test@example.com>",
            "-m",
            "msg"
        ]
    );
}

#[test]
fn build_commit_argv_rejects_bad_message() {
    assert!(build_commit_argv("", &[], None).is_err());
}

#[test]
fn build_commit_argv_literalizes_caller_paths() {
    let argv = build_commit_argv("msg", &[":(glob)*".to_string()], None).unwrap();
    assert_eq!(
        argv,
        vec!["commit", "-m", "msg", "--", ":(literal):(glob)*"]
    );
}

// -- build_branch_argv -----------------------------------------------------

#[test]
fn build_branch_argv_name_only() {
    let argv = build_branch_argv("feat/x", None).unwrap();
    assert_eq!(argv, vec!["branch", "--", "feat/x"]);
}

#[test]
fn build_branch_argv_with_from() {
    let argv = build_branch_argv("feat/x", Some("main")).unwrap();
    assert_eq!(argv, vec!["branch", "--", "feat/x", "main"]);
}

#[test]
fn build_branch_argv_rejects_injection_shaped_name() {
    for injected in ["-D", "--upload-pack=evil", "main;rm -rf /", "a b"] {
        assert!(build_branch_argv(injected, None).is_err(), "{injected}");
    }
}

#[test]
fn build_branch_argv_rejects_injection_shaped_from() {
    assert!(build_branch_argv("feat/x", Some("--upload-pack=evil")).is_err());
}

// -- build_push_argv -----------------------------------------------------

#[test]
fn build_push_argv_fixed_shape() {
    let argv = build_push_argv("origin", "feat/x").unwrap();
    assert_eq!(argv, vec!["push", "--", "origin", "feat/x"]);
}

#[test]
fn build_push_argv_never_contains_force_flag() {
    // No caller input can produce a force flag -- build_push_argv has no
    // parameter for it at all. This test pins that invariant against a
    // future edit that might add one.
    let argv = build_push_argv("origin", "feat/x").unwrap();
    assert!(!argv.iter().any(|a| a.contains("force") || a == "-f"));
}

#[test]
fn build_push_argv_rejects_injection_shaped_remote() {
    for injected in ["--upload-pack=evil", "-o", "ext::sh -c evil"] {
        assert!(build_push_argv(injected, "feat/x").is_err(), "{injected}");
    }
}

#[test]
fn build_push_argv_rejects_injection_shaped_branch() {
    for injected in ["-D", "--upload-pack=evil", "a;b"] {
        assert!(build_push_argv("origin", injected).is_err(), "{injected}");
    }
}
