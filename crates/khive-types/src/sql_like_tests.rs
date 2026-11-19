use super::escape_like_literal;

#[test]
fn escapes_only_like_wildcards_and_the_escape_character() {
    for (input, expected) in [
        ("", ""),
        ("plain text", "plain text"),
        ("%", "\\%"),
        ("_", "\\_"),
        ("\\", "\\\\"),
        ("name%_\\tail", "name\\%\\_\\\\tail"),
        ("中文_é%\\", "中文\\_é\\%\\\\"),
        ("quote' and \"double\"", "quote' and \"double\""),
    ] {
        assert_eq!(escape_like_literal(input), expected, "input={input:?}");
    }
}
