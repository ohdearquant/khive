use super::{bounded_client_cwd, CLIENT_CWD_MAX_BYTES};

#[test]
fn a_short_path_is_kept_whole_and_not_marked_cut() {
    let value = bounded_client_cwd("/home/dev/scratch");
    assert_eq!(value["path"], "/home/dev/scratch");
    assert_eq!(value["cut"], false);
}

#[test]
fn control_format_and_separator_characters_are_removed() {
    let value = bounded_client_cwd("/home/dev\n/a\u{1b}[31m\u{202e}b\u{2028}c\u{2029}d");
    assert_eq!(value["path"], "/home/dev/a[31mbcd");
    assert_eq!(value["cut"], false);
}

#[test]
fn accented_and_cjk_names_are_kept() {
    let value = bounded_client_cwd("/home/dév/项目");
    assert_eq!(value["path"], "/home/dév/项目");
}

#[test]
fn a_long_path_is_cut_on_a_character_boundary_and_marked_cut() {
    // 'é' is two bytes, so a 1-byte prefix puts the byte limit inside a character.
    let raw = format!("/{}", "é".repeat(CLIENT_CWD_MAX_BYTES));
    let value = bounded_client_cwd(&raw);
    let path = value["path"].as_str().expect("path is a string");
    assert_eq!(path.len(), CLIENT_CWD_MAX_BYTES - 1);
    assert!(raw.starts_with(path));
    assert_eq!(value["cut"], true);
}

#[test]
fn a_path_of_exactly_the_limit_is_not_cut() {
    let raw = "a".repeat(CLIENT_CWD_MAX_BYTES);
    let value = bounded_client_cwd(&raw);
    assert_eq!(
        value["path"].as_str().map(str::len),
        Some(CLIENT_CWD_MAX_BYTES)
    );
    assert_eq!(value["cut"], false);
}
