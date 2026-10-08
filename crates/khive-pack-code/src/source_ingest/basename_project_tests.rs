//! Unit tests for the manifestless project-name fallback.

use super::*;

#[test]
fn basename_fallback_uses_canonical_name_only_when_lexical_name_is_missing() {
    assert_eq!(
        basename_project_name(Path::new("project/child/.."), Path::new("project")),
        "project"
    );
    assert_eq!(
        basename_project_name(Path::new("caller_alias"), Path::new("physical_project")),
        "caller_alias"
    );
    assert_eq!(
        basename_project_name(Path::new("project/."), Path::new("physical_project")),
        "project"
    );
}

#[test]
fn basename_fallback_preserves_the_last_resort_without_filesystem_access() {
    let root = Path::new(std::path::MAIN_SEPARATOR_STR);
    assert_eq!(
        basename_project_name(root, root),
        root.display().to_string()
    );
    assert_eq!(basename_project_name(Path::new("."), Path::new(".")), ".");
}
