//! The listing check of a checkout of named paths (ADR-182 Amendment 24, item 1).

use super::{check_listing_scope, ListedBlob};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn listed(paths: &[&str]) -> Vec<ListedBlob> {
    paths
        .iter()
        .map(|path| ListedBlob {
            path: (*path).into(),
            oid: "0".repeat(40),
            mode: 644,
        })
        .collect()
}

fn scope(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| (*path).to_string()).collect()
}

#[test]
fn a_listing_under_its_named_paths_passes() {
    check_listing_scope(
        &listed(&["a.txt", "docs/one.md", "docs/sub/two.md"]),
        &scope(&["a.txt", "docs"]),
        COMMIT,
    )
    .expect("every entry lies under a named path");
}

#[test]
fn a_named_path_that_covers_nothing_refuses_path_not_found_naming_it() {
    // `docs-index.md` begins with `docs` and is not inside it.
    let error = check_listing_scope(
        &listed(&["a.txt", "docs-index.md"]),
        &scope(&["a.txt", "docs"]),
        COMMIT,
    )
    .expect_err("docs covers nothing");
    assert_eq!(error.code(), "path_not_found");
    assert!(error.message().contains("\"docs\""), "{error}");
    assert!(error.message().contains(COMMIT), "{error}");
}

#[test]
fn an_entry_under_no_named_path_refuses() {
    let error = check_listing_scope(
        &listed(&["star*.md", "starx.md"]),
        &scope(&["star*.md"]),
        COMMIT,
    )
    .expect_err("an entry the caller did not name");
    assert_eq!(error.code(), "git_output");
    assert!(error.message().contains("starx.md"), "{error}");
}
