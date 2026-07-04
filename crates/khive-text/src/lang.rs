//! Script/alphabet identification for per-language routing decisions.

use std::collections::HashMap;

/// Returns true if `c` falls within standard CJK Unicode blocks (Unified, Extension A/B, Compatibility, Hiragana, Katakana, Hangul).
#[inline]
pub fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309F}'     // Hiragana
        | '\u{30A0}'..='\u{30FF}'   // Katakana
        | '\u{3400}'..='\u{4DBF}'   // CJK Extension A
        | '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{F900}'..='\u{FAFF}'   // CJK Compatibility Ideographs
        | '\u{AC00}'..='\u{D7AF}'   // Hangul Syllables
        | '\u{20000}'..='\u{2A6DF}' // CJK Extension B
    )
}

/// Returns true when more than 15% of the characters in `text` are CJK.
pub fn contains_cjk(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return false;
    }
    let cjk_count = chars.iter().filter(|&&c| is_cjk_char(c)).count();
    (cjk_count as f64 / chars.len() as f64) > 0.15
}

/// Lightweight script profile computed over a single string.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptProfile {
    /// Fraction of characters that are CJK (0.0-1.0).
    pub cjk_fraction: f64,
    /// Fraction of characters that are ASCII letters (0.0-1.0).
    pub latin_fraction: f64,
    /// Total character count (not byte count).
    pub char_count: usize,
}

impl ScriptProfile {
    /// Analyze `text` and return a ScriptProfile.
    pub fn analyze(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        if n == 0 {
            return Self {
                cjk_fraction: 0.0,
                latin_fraction: 0.0,
                char_count: 0,
            };
        }
        let cjk = chars.iter().filter(|&&c| is_cjk_char(c)).count();
        let latin = chars.iter().filter(|&&c| c.is_ascii_alphabetic()).count();
        Self {
            cjk_fraction: cjk as f64 / n as f64,
            latin_fraction: latin as f64 / n as f64,
            char_count: n,
        }
    }

    /// True when CJK fraction exceeds 15%.
    pub fn is_cjk_dominant(&self) -> bool {
        self.cjk_fraction > 0.15
    }
}

/// Returns true when `query` is worth sending to a retrieval backend.
/// Rejects empty, symbol-only, single ASCII letter, and repeated-char (>80%) gibberish.
pub fn is_meaningful_query(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }

    let non_ws: Vec<char> = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if non_ws.is_empty() {
        return false;
    }

    // Symbol/punctuation/emoji-only queries are not meaningful, including Unicode symbols.
    if !non_ws.iter().any(|c| c.is_alphanumeric()) {
        return false;
    }

    // Single ASCII letter
    if non_ws.len() == 1 && non_ws[0].is_ascii_alphabetic() {
        return false;
    }

    // Repeated-char gibberish: dominant char > 80% of non-ws chars.
    // Skip when total == 1 — a single character cannot exhibit gibberish repetition.
    let total = non_ws.len();
    if total > 1 {
        let mut counts: HashMap<char, usize> = HashMap::new();
        for c in &non_ws {
            *counts.entry(*c).or_insert(0) += 1;
        }
        if let Some(&max_count) = counts.values().max() {
            if max_count as f64 / total as f64 > 0.80 {
                return false;
            }
        }
    }

    true
}

#[cfg(test)]
mod tests {
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

    // --- contains_cjk ---

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

    // --- ScriptProfile ---

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

    // --- is_meaningful_query ---

    #[test]
    fn empty_not_meaningful() {
        assert!(!is_meaningful_query(""));
        assert!(!is_meaningful_query("   "));
    }

    #[test]
    fn symbols_only_not_meaningful() {
        assert!(!is_meaningful_query("!!!"));
        assert!(!is_meaningful_query("@#$%"));
        assert!(!is_meaningful_query("..."));
    }

    #[test]
    fn single_latin_char_not_meaningful() {
        assert!(!is_meaningful_query("a"));
        assert!(!is_meaningful_query("Z"));
    }

    #[test]
    fn repeated_char_gibberish_not_meaningful() {
        assert!(!is_meaningful_query("aaaaaaa")); // 100%
        assert!(!is_meaningful_query("aaaaab")); // 5/6 ≈ 83% > 80%
    }

    #[test]
    fn repeated_char_below_threshold_is_meaningful() {
        assert!(is_meaningful_query("aaab")); // 3/4 = 75% ≤ 80%
    }

    #[test]
    fn normal_queries_are_meaningful() {
        assert!(is_meaningful_query("rust programming"));
        assert!(is_meaningful_query("你好世界"));
        assert!(is_meaningful_query("BM25"));
        assert!(is_meaningful_query("ab"));
    }

    #[test]
    fn single_digit_is_meaningful() {
        // Only single ASCII letter is blocked, not digit
        assert!(is_meaningful_query("5"));
    }

    #[test]
    fn unicode_symbol_only_queries_are_not_meaningful() {
        assert!(!is_meaningful_query("\u{FF0C}")); // fullwidth comma
        assert!(!is_meaningful_query("\u{3001}")); // ideographic comma
        assert!(!is_meaningful_query("\u{FF01}\u{FF1F}")); // fullwidth ! ?
        assert!(!is_meaningful_query("\u{1F600}")); // emoji-only

        assert!(is_meaningful_query("\u{4F60}\u{597D}\u{4E16}\u{754C}")); // 你好世界
        assert!(is_meaningful_query("BM25"));
        assert!(is_meaningful_query("5"));
    }
}
