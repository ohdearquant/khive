use super::*;

#[test]
fn cjk_ideograph() {
    assert!(is_cjk_char('使'));
    assert!(is_cjk_char('用'));
    assert!(is_cjk_char('进'));
}

#[test]
fn hiragana_katakana() {
    assert!(is_cjk_char('あ'));
    assert!(is_cjk_char('ア'));
}

#[test]
fn hangul() {
    assert!(is_cjk_char('가'));
}

#[test]
fn latin_ascii_not_cjk() {
    assert!(!is_cjk_char('a'));
    assert!(!is_cjk_char('Z'));
    assert!(!is_cjk_char('0'));
    assert!(!is_cjk_char('-'));
}

#[test]
fn cjk_extension_a_boundary() {
    assert!(is_cjk_char('\u{3400}')); // first Extension A
    assert!(is_cjk_char('\u{4DBF}')); // last Extension A
    assert!(!is_cjk_char('\u{33FF}')); // just before
    assert!(!is_cjk_char('\u{4DC0}')); // just after Extension A
}

#[test]
fn unified_ideographs_boundary() {
    assert!(is_cjk_char('\u{4E00}')); // first CJK Unified
    assert!(is_cjk_char('\u{9FFF}')); // last CJK Unified
    assert!(!is_cjk_char('\u{A000}')); // just after
}

#[test]
fn compatibility_ideographs_boundary() {
    assert!(is_cjk_char('\u{F900}')); // first Compatibility
    assert!(is_cjk_char('\u{FAFF}')); // last Compatibility
    assert!(!is_cjk_char('\u{F8FF}')); // just before
    assert!(!is_cjk_char('\u{FB00}')); // just after
}

#[test]
fn hiragana_boundary() {
    assert!(is_cjk_char('\u{3040}')); // first Hiragana
    assert!(is_cjk_char('\u{309F}')); // last Hiragana
    assert!(!is_cjk_char('\u{303F}')); // just before
}

#[test]
fn katakana_boundary() {
    assert!(is_cjk_char('\u{30A0}')); // first Katakana
    assert!(is_cjk_char('\u{30FF}')); // last Katakana
    assert!(!is_cjk_char('\u{3100}')); // just after
}

#[test]
fn hangul_boundary() {
    assert!(is_cjk_char('\u{AC00}')); // first Hangul Syllable
    assert!(is_cjk_char('\u{D7AF}')); // last Hangul Syllable
    assert!(!is_cjk_char('\u{D7B0}')); // just after
}

#[test]
fn empty_string_no_cjk() {
    assert!(!contains_cjk(""));
}

#[test]
fn all_latin_no_cjk() {
    assert!(!contains_cjk("hello world"));
}

#[test]
fn all_cjk() {
    assert!(contains_cjk("你好世界"));
}

#[test]
fn mixed_above_threshold() {
    // 2 CJK in 5 chars = 40% > 15%
    assert!(contains_cjk("abc你好"));
}

#[test]
fn mixed_below_threshold() {
    // 1 CJK in 10 chars = 10% ≤ 15%
    assert!(!contains_cjk("abcdefghi你"));
}

#[test]
fn exactly_15_percent_is_false() {
    // 3 CJK in 20 chars = 15.0%, not > 15%
    let text = "你好世abcdefghijklmnopq"; // 3 CJK + 17 latin = 20 chars
    assert_eq!(text.chars().count(), 20);
    assert!(!contains_cjk(text));
}

#[test]
fn f32_classifier_preserves_strict_boundary_and_script_ranges() {
    for (text, expected) in [
        ("", false),
        ("ordinary latin query", false),
        ("漢字かな한글", true),
        ("界界界abcdefghijklmnopq", false),
        ("界界界abcdefghijklmnopqr", false),
        ("界界界abcdefghijklmnop", true),
    ] {
        assert_eq!(contains_cjk_f32(text), expected, "text={text:?}");
    }
}

#[test]
fn profile_pure_latin() {
    let p = ScriptProfile::analyze("hello");
    assert_eq!(p.char_count, 5);
    assert_eq!(p.cjk_fraction, 0.0);
    assert_eq!(p.latin_fraction, 1.0);
    assert!(!p.is_cjk_dominant());
}

#[test]
fn profile_pure_cjk() {
    let p = ScriptProfile::analyze("你好");
    assert_eq!(p.char_count, 2);
    assert_eq!(p.cjk_fraction, 1.0);
    assert_eq!(p.latin_fraction, 0.0);
    assert!(p.is_cjk_dominant());
}

#[test]
fn profile_empty() {
    let p = ScriptProfile::analyze("");
    assert_eq!(p.char_count, 0);
    assert_eq!(p.cjk_fraction, 0.0);
    assert!(!p.is_cjk_dominant());
}

#[test]
fn profile_mixed() {
    // "hi你" — 3 chars: 2 latin, 1 CJK => 33% CJK > 15%
    let p = ScriptProfile::analyze("hi你");
    assert_eq!(p.char_count, 3);
    assert!((p.cjk_fraction - 1.0 / 3.0).abs() < 1e-9);
    assert!((p.latin_fraction - 2.0 / 3.0).abs() < 1e-9);
    assert!(p.is_cjk_dominant());
}

#[test]
#[allow(deprecated)]
fn empty_not_meaningful() {
    assert!(!is_meaningful_query(""));
    assert!(!is_meaningful_query("   "));
}

#[test]
#[allow(deprecated)]
fn symbols_only_not_meaningful() {
    assert!(!is_meaningful_query("!!!"));
    assert!(!is_meaningful_query("@#$%"));
    assert!(!is_meaningful_query("..."));
}

#[test]
#[allow(deprecated)]
fn single_latin_char_not_meaningful() {
    assert!(!is_meaningful_query("a"));
    assert!(!is_meaningful_query("Z"));
}

#[test]
#[allow(deprecated)]
fn repeated_char_gibberish_not_meaningful() {
    assert!(!is_meaningful_query("aaaaaaa")); // 100%
    assert!(!is_meaningful_query("aaaaab")); // 5/6 ≈ 83% > 80%
}

#[test]
#[allow(deprecated)]
fn repeated_char_below_threshold_is_meaningful() {
    assert!(is_meaningful_query("aaab")); // 3/4 = 75% ≤ 80%
}

#[test]
#[allow(deprecated)]
fn normal_queries_are_meaningful() {
    assert!(is_meaningful_query("rust programming"));
    assert!(is_meaningful_query("你好世界"));
    assert!(is_meaningful_query("BM25"));
    assert!(is_meaningful_query("ab"));
}

#[test]
#[allow(deprecated)]
fn single_digit_is_meaningful() {
    // Only single ASCII letter is blocked, not digit
    assert!(is_meaningful_query("5"));
}

#[test]
#[allow(deprecated)]
fn unicode_symbol_only_queries_are_not_meaningful() {
    assert!(!is_meaningful_query("\u{FF0C}")); // fullwidth comma
    assert!(!is_meaningful_query("\u{3001}")); // ideographic comma
    assert!(!is_meaningful_query("\u{FF01}\u{FF1F}")); // fullwidth ! ?
    assert!(!is_meaningful_query("\u{1F600}")); // emoji-only

    assert!(is_meaningful_query("\u{4F60}\u{597D}\u{4E16}\u{754C}")); // 你好世界
    assert!(is_meaningful_query("BM25"));
    assert!(is_meaningful_query("5"));
}
