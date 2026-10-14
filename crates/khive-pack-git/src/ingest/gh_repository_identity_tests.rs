use super::parse_gh_repository_identity;

#[test]
fn accepts_matching_github_identity() {
    assert_eq!(
            parse_gh_repository_identity(
                br#"{"nameWithOwner":"Fixture/Repository","url":"https://github.com/Fixture/Repository"}"#,
                "fixture/repository",
            ),
            Ok("Fixture/Repository".to_string())
        );
}

#[test]
fn rejects_successful_non_github_identity() {
    assert_eq!(
            parse_gh_repository_identity(
                br#"{"nameWithOwner":"fixture/repository","url":"https://gitlab.com/fixture/repository"}"#,
                "fixture/repository",
            ),
            Err("gh CLI did not resolve a github.com repository for this checkout")
        );
}

#[test]
fn rejects_mismatched_slug_and_url() {
    assert_eq!(
            parse_gh_repository_identity(
                br#"{"nameWithOwner":"fixture/repository","url":"https://github.com/other/repository"}"#,
                "fixture/repository",
            ),
            Err("gh CLI did not resolve a github.com repository for this checkout")
        );
}

#[test]
fn rejects_malformed_identity() {
    assert_eq!(
        parse_gh_repository_identity(
            br#"{"nameWithOwner":"fixture/repository"}"#,
            "fixture/repository",
        ),
        Err("gh CLI returned an invalid repository identity")
    );
}

#[test]
fn rejects_self_consistent_but_unexpected_repository() {
    assert_eq!(
        parse_gh_repository_identity(
            br#"{"nameWithOwner":"other/repository","url":"https://github.com/other/repository"}"#,
            "fixture/repository",
        ),
        Err("gh CLI resolved a different repository than the digest source")
    );
}
